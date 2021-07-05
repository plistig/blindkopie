use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet, VecDeque};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::ops::DerefMut;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use futures::stream::TryStreamExt;
use futures::SinkExt;
use log::{debug, error, info, warn};
use ron::de::from_str;
use ron::ser::to_string;
use rustyline::Editor;
use scopeguard::defer;
use serde::{Deserialize, Serialize};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, SeekFrom};
use tokio::select;
use tokio::sync::{Notify, RwLock};
use tokio_util::codec::{Framed, LinesCodec};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::networking::{
    await_with_cancellation_token, sleep_with_cancellation_token,
    with_timeout_and_cancellation_token, SafeHttpsClient,
};
use crate::reddit::{read_my_name, RedditPost, RedditToken};

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct ClientData {
    #[serde(default)]
    pub sr_to_monitor: String,
    #[serde(default)]
    pub sr_to_post_to: String,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub public_key: String,
    #[serde(default)]
    pub private_key: String,
    #[serde(default)]
    pub user_agent: String,
    #[serde(default)]
    pub redirect_uri: String,
}

#[derive(Debug, Default)]
pub struct State {
    pub cancel_token: CancellationToken,
    pub https_client: SafeHttpsClient,
    pub client_data: RwLock<ClientData>,
    pub reddit_token: RwLock<RedditToken>,

    pub seen_posts: RwLock<HashSet<Work>>,

    pub screenshot_queue: WorkQueue,
    pub outline_com_queue: WorkQueue,
    pub wikiwix_queue: WorkQueue,
    pub google_cache_queue: WorkQueue,
    pub comment_queue: WorkQueue,

    have_uncommitted_changes: Notify,
    uncommitted_changes: RwLock<VecDeque<String>>,
    changelog: RwLock<Option<Framed<File, LinesCodec>>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum StateChange<'a> {
    RedditToken(Cow<'a, RedditToken>),
    ClientData(Cow<'a, ClientData>),
    NewPost(Cow<'a, RedditPost>),

    OutlineComOutcome(Cow<'a, RedditPost>, Option<Cow<'a, str>>),
    WikiwixOutcome(Cow<'a, RedditPost>, Option<Cow<'a, str>>),
    GoogleCacheOutcome(Cow<'a, RedditPost>, Option<Cow<'a, str>>),
    ScreenshotOutcome(Cow<'a, RedditPost>, Option<Cow<'a, str>>),
    TextOutcome(Cow<'a, RedditPost>, Option<Cow<'a, str>>),
}

#[derive(Debug, Default)]
pub struct WorkQueue {
    pub notify: Notify,
    pub inner: RwLock<WorkQueueInner>,
}

#[derive(Debug, Default)]
pub struct WorkQueueInner {
    pub unordered_set: HashSet<Work>,
    pub ordered_queue: BinaryHeap<Work>,
}

impl WorkQueueInner {
    pub fn insert(&mut self, work: Work) -> bool {
        if self.unordered_set.insert(work.clone()) {
            self.ordered_queue.push(work);
            true
        } else {
            warn!("Duplicated item {:?} in queue", &work.meta.id);
            false
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Work {
    pub meta: RedditPost,
    pub data: Arc<RwLock<WorkInner>>,
}

impl Work {
    pub fn new(meta: RedditPost) -> Self {
        Self {
            meta,
            ..Self::default()
        }
    }
}

impl Hash for Work {
    fn hash<H>(&self, hasher: &mut H)
    where
        H: Hasher,
    {
        self.meta.hash(hasher)
    }
}

impl PartialOrd for Work {
    fn partial_cmp(&self, other: &Work) -> Option<Ordering> {
        self.meta.partial_cmp(&other.meta)
    }
}

impl Ord for Work {
    fn cmp(&self, other: &Work) -> Ordering {
        self.meta.cmp(&other.meta)
    }
}

impl PartialEq for Work {
    fn eq(&self, other: &Work) -> bool {
        self.meta.eq(&other.meta)
    }
}

impl Eq for Work {}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WorkInner {
    pub outline_com: WorkOutcome,
    pub wikiwix: WorkOutcome,
    pub google_cache: WorkOutcome,
    pub screenshot: WorkOutcome,
    pub text: WorkOutcome,
    pub comment: WorkOutcome,
}

impl WorkInner {
    pub fn ready_to_comment(&self) -> bool {
        !self.comment.is_done()
            && self.outline_com.is_done()
            && self.wikiwix.is_done()
            && self.google_cache.is_done()
            && self.screenshot.is_done()
            && self.text.is_done()
    }

    pub fn is_failure(&self) -> bool {
        use WorkOutcome::Failure;
        matches!(
            (
                &self.outline_com,
                &self.wikiwix,
                &self.screenshot,
                &self.text
            ),
            (Failure, Failure, Failure, Failure),
        )
    }
}

#[derive(Debug, Serialize, Deserialize, PartialOrd, PartialEq, Ord, Eq, Hash)]
pub enum WorkOutcome {
    Waiting,
    Processing,
    Success(String),
    Failure,
}

impl WorkOutcome {
    pub fn is_done(&self) -> bool {
        matches!(self, WorkOutcome::Success(_) | WorkOutcome::Failure)
    }
}

impl Default for WorkOutcome {
    fn default() -> Self {
        Self::Waiting
    }
}

impl State {
    pub async fn new() -> Result<Arc<Self>> {
        let mut result = Self::default();

        let changelog = OpenOptions::new()
            .read(true)
            .append(true)
            .open("changelog.bin")
            .await
            .context("Could not open changelog.bin")?;
        let mut changelog = Framed::new(changelog, LinesCodec::new());
        result.read_changelog(&mut changelog).await?;

        info!("Read changelog");

        result.changelog = RwLock::new(Some(changelog));
        Ok(Arc::new(result))
    }

    pub async fn init() -> Result<Arc<Self>> {
        let mut result = Self::default();

        let changelog = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open("changelog.bin")
            .await
            .context("Could not open changelog.bin")?;
        let mut changelog = Framed::new(changelog, LinesCodec::new());
        result.read_changelog(&mut changelog).await?;

        result.changelog = RwLock::new(Some(changelog));
        Ok(Arc::new(result))
    }

    async fn read_changelog(&mut self, changelog: &mut Framed<File, LinesCodec>) -> Result<()> {
        changelog.get_mut().seek(SeekFrom::Start(0)).await?;
        loop {
            let pos = changelog.get_mut().seek(SeekFrom::Current(0)).await?;
            if let Some(change) = changelog.try_next().await? {
                let change = from_str(&change)?;
                self.apply_change(change).await?;
            } else {
                changelog.get_mut().seek(SeekFrom::Start(pos)).await?;
                break;
            }
        }
        Ok(())
    }

    pub async fn log_changes(&self, changes: &[StateChange<'_>]) -> Result<()> {
        if !changes.is_empty() {
            {
                let mut guard = self.uncommitted_changes.write().await;
                let uncommitted_changes: &mut VecDeque<_> = &mut guard;
                for change in changes {
                    uncommitted_changes.push_back(to_string(change)?);
                }
            }
            self.have_uncommitted_changes.notify_one();
        }
        Ok(())
    }

    async fn apply_change(&mut self, change: StateChange<'_>) -> Result<()> {
        debug!("Got change: {:?}", change);
        match change {
            StateChange::RedditToken(new_reddit_token) => {
                *self.reddit_token.write().await.deref_mut() = new_reddit_token.into_owned();
            }
            StateChange::ClientData(new_client_data) => {
                *self.client_data.write().await.deref_mut() = new_client_data.into_owned();
            }
            StateChange::NewPost(new_post) => {
                let new_work = Work::new(new_post.into_owned());

                self.seen_posts.write().await.insert(new_work.clone());
                self.screenshot_queue
                    .inner
                    .write()
                    .await
                    .insert(new_work.clone());
                self.outline_com_queue
                    .inner
                    .write()
                    .await
                    .insert(new_work.clone());
                self.google_cache_queue
                    .inner
                    .write()
                    .await
                    .insert(new_work.clone());
            }
            StateChange::OutlineComOutcome(post, url) => {
                self.apply_outcome(post, url, |data, outcome| {
                    data.outline_com = outcome;
                })
                .await?;
            }
            StateChange::WikiwixOutcome(post, url) => {
                self.apply_outcome(post, url, |data, outcome| {
                    data.wikiwix = outcome;
                })
                .await?;
            }
            StateChange::GoogleCacheOutcome(post, url) => {
                self.apply_outcome(post, url, |data, outcome| {
                    data.google_cache = outcome;
                })
                .await?;
            }
            StateChange::ScreenshotOutcome(post, url) => {
                self.apply_outcome(post, url, |data, outcome| {
                    data.screenshot = outcome;
                })
                .await?;
            }
            StateChange::TextOutcome(post, text) => {
                self.apply_outcome(post, text, |data, outcome| {
                    data.text = outcome;
                })
                .await?;
            }
        }
        Ok(())
    }

    async fn apply_outcome<'a>(
        &mut self,
        post: Cow<'a, RedditPost>,
        url: Option<Cow<'a, str>>,
        set: impl FnOnce(&mut WorkInner, WorkOutcome),
    ) -> Result<()> {
        if let Some(work) = self
            .seen_posts
            .read()
            .await
            .get(&Work::new(post.into_owned()))
        {
            let mut data = work.data.write().await;
            set(
                &mut data,
                match url {
                    Some(url) => WorkOutcome::Success(url.into_owned()),
                    None => WorkOutcome::Failure,
                },
            );
            if data.ready_to_comment() {
                self.comment_queue.inner.write().await.insert(work.clone());
            }
        }
        Ok(())
    }

    pub async fn commit_changes(&self) -> Result<()> {
        let mut guard = self.changelog.write().await;
        {
            let changelog = if let Some(changelog) = guard.deref_mut() {
                changelog
            } else {
                bail!("Changelog was closed.");
            };

            let mut guard = self.uncommitted_changes.write().await;
            let uncommitted_changes: &mut VecDeque<_> = &mut guard;

            while let Some(change) = uncommitted_changes.pop_front() {
                changelog.send(change).await?;
            }
        }

        if let Some(changelog) = guard.deref_mut() {
            changelog.get_mut().sync_data().await?;
        }

        Ok(())
    }
}

pub async fn changelog_loop(state: Arc<State>) -> Result<()> {
    let state: &State = &state;
    defer! {
        warn!("Commit changes loop is done. Shutting down …");
        state.cancel_token.cancel();
    }
    info!("Changelog loop running.");

    loop {
        state.commit_changes().await?;
        select! {
            _ = state.cancel_token.cancelled() => return Ok(()),
            _ = state.have_uncommitted_changes.notified() => continue,
        };
    }
}

pub async fn subcommand_init() -> Result<()> {
    let state = State::init().await?;
    let state = &state;

    let mut rl = Editor::<()>::new();

    #[derive(Debug, Deserialize)]
    struct Query<'a> {
        code: Cow<'a, str>,
    }

    println!("WARNING: private keys from a prior setup will be printed!");
    rl.readline("Press Ctrl+C to abort. Press enter to continue.")?;

    let new_client_data = {
        let mut client_data = state.client_data.write().await;
        let client_data: &mut ClientData = &mut client_data;

        let sr_to_post_to = loop {
            let default = &client_data.sr_to_post_to;
            let mut sr_to_post_to = rl.readline(&format!(
                "Subreddit to post to (should be private, e.g.: blindkopie) [{}]: ",
                default
            ))?;
            if sr_to_post_to.trim().is_empty() {
                sr_to_post_to = default.to_owned();
            }
            let sr_to_post_to = sr_to_post_to.trim();
            if !sr_to_post_to.is_empty() {
                break sr_to_post_to.to_owned();
            }
        };
        let sr_to_monitor = loop {
            let default = &client_data.sr_to_monitor;
            let mut sr_to_monitor =
                rl.readline(&format!("Subreddit to monitor (e.g.: de) [{}]: ", default))?;
            if sr_to_monitor.trim().is_empty() {
                sr_to_monitor = default.to_owned();
            }
            let sr_to_monitor = sr_to_monitor.trim();
            if !sr_to_monitor.is_empty() {
                break sr_to_monitor.to_owned();
            }
        };
        let public_key = loop {
            let default = &client_data.public_key;
            let mut public_key = rl.readline(&format!("Script public id [{}]: ", default))?;
            if public_key.trim().is_empty() {
                public_key = default.to_owned();
            }
            let public_key = public_key.trim();
            if !public_key.is_empty() {
                break public_key.to_owned();
            }
        };
        let private_key = loop {
            let default = &client_data.private_key;
            let mut private_key =
                rl.readline(&format!("Script secret (REPEATED) [{}]: ", default))?;
            if private_key.trim().is_empty() {
                private_key = default.to_owned();
            }
            let private_key = private_key.trim();
            if !private_key.is_empty() {
                break private_key.to_owned();
            }
        };
        let user_agent = loop {
            let default = format!("Linux:{}:v0.1.0 (by /u/plistig for /r/de)", &public_key);
            let mut user_agent = rl.readline(&format!("User agent [{}]: ", &default))?;
            if user_agent.trim().is_empty() {
                user_agent = default;
            }
            let user_agent = user_agent.trim();
            if !user_agent.is_empty() {
                break user_agent.to_owned();
            }
        };
        let redirect_uri = loop {
            let default = "http://localhost/";
            let mut redirect_uri = rl.readline(&format!("Redirect URI [{}]: ", &default))?;
            if redirect_uri.trim().is_empty() {
                redirect_uri = default.to_owned();
            }
            let redirect_uri = redirect_uri.trim();
            if !redirect_uri.is_empty() {
                break redirect_uri.to_owned();
            }
        };

        let mut new_client_data = ClientData {
            code: "".to_owned(),
            sr_to_post_to,
            sr_to_monitor,
            public_key,
            private_key,
            user_agent,
            redirect_uri,
        };

        let mut retrieve_code = || -> Result<String> {
            println!(
                "Open https://www.reddit.com/api/v1/authorize?client_id={}&response_type=code&state=RANDOM_STRING&\
                      redirect_uri={}&duration=permanent&scope=edit+identity+read+submit",
                &new_client_data.public_key, &new_client_data.redirect_uri,
            );
            let line = rl.readline("Copy redirected URL: ")?;
            let query = serde_qs::from_str::<Query>(
                Url::parse(line.trim())?
                    .query()
                    .expect("Expected valid URL"),
            )?;
            Ok(query.code.trim().to_owned())
        };
        new_client_data.code = loop {
            match retrieve_code() {
                Ok(code) => break code,
                Err(err) => error!("{}", err),
            }
        };

        *client_data = new_client_data.clone();
        new_client_data
    };
    state
        .log_changes(&[StateChange::ClientData(Cow::Owned(new_client_data))])
        .await?;

    println!("Successfully logged in as: {}", read_my_name(state).await?);

    state.commit_changes().await?;

    Ok(())
}

pub async fn run_submit_loop<F1, F2, F3, F4>(
    name: &str,
    sleep_seconds: u64,
    max_submit_seconds: u64,
    state: &State,
    queue: &WorkQueue,
    skip_work: F1,
    get_mut_outcome: F2,
    submit: F3,
    state_change: F4,
) -> Result<()>
where
    F1: 'static + Fn(&WorkInner) -> bool,
    F2: 'static + for<'a> Fn(&'a mut WorkInner) -> &'a mut WorkOutcome,
    F3: 'static
        + for<'a> Fn(&'a State, &'a Work) -> Pin<Box<dyn 'a + Future<Output = Result<String>>>>,
    F4: 'static + for<'a> Fn(Cow<'a, RedditPost>, Option<Cow<'a, str>>) -> StateChange<'a>,
{
    let state: &State = &state;
    defer! {
        warn!("{} loop is done. Shutting down …", name);
        state.cancel_token.cancel();
    }
    info!("{} loop running.", name);

    loop {
        loop {
            if state.cancel_token.is_cancelled() {
                return Ok(());
            }

            let work = {
                let mut queue = queue.inner.write().await;
                if let Some(work) = queue.ordered_queue.pop() {
                    queue.unordered_set.remove(&work);

                    let mut data = work.data.write().await;
                    let data: &mut WorkInner = &mut data;

                    if skip_work(data) {
                        continue;
                    }

                    *get_mut_outcome(data) = WorkOutcome::Processing;

                    work.clone()
                } else {
                    break;
                }
            };
            info!("Will submit to {}: {:?}", name, &work.meta);

            let result = with_timeout_and_cancellation_token(
                state,
                max_submit_seconds,
                submit(state, &work),
            )
            .await;
            let do_comment = {
                let mut data = work.data.write().await;
                let data: &mut WorkInner = &mut data;

                match result {
                    Ok(url) => {
                        info!("{} success: {:?} => {:?}", name, &work.meta.url, &url);
                        state
                            .log_changes(&[state_change(
                                Cow::Borrowed(&work.meta),
                                Some(Cow::Borrowed(&url)),
                            )])
                            .await?;
                        *get_mut_outcome(data) = WorkOutcome::Success(url);
                    }
                    Err(err) => {
                        dbg!(err);
                        warn!("{} failure: {:?}", name, &work.meta.url);
                        state
                            .log_changes(&[state_change(Cow::Borrowed(&work.meta), None)])
                            .await?;
                        *get_mut_outcome(data) = WorkOutcome::Failure;
                    }
                }

                data.ready_to_comment()
            };

            if do_comment && state.comment_queue.inner.write().await.insert(work) {
                state.comment_queue.notify.notify_one();
            }

            sleep_with_cancellation_token(state, sleep_seconds).await?;
        }
        await_with_cancellation_token(state, &queue.notify).await?;
    }
}
