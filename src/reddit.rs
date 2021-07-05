use std::borrow::{Borrow, Cow};
use std::cmp::Ordering;
use std::convert::{TryFrom, TryInto, AsRef};
use std::hash::{Hash, Hasher};
use std::io::Cursor;
use std::str::FromStr;
use std::sync::{atomic, Arc};

use anyhow::{Result, bail};
use chrono::{Utc, Duration, DateTime};
use futures::StreamExt;
use hyper::{body, Body, Request, Method, header, Uri};
use log::{debug, info, warn, error};
use memmap::Mmap;
use mime::Mime;
use scopeguard::defer;
use serde::{Serialize, Deserialize};
use serde::de::DeserializeOwned;
use tinystr::TinyStr16;
use tokio::{select, try_join};
use tokio_tungstenite::tungstenite::Message;

use crate::networking::{with_timeout_and_cancellation_token, sleep_with_cancellation_token, parse_url};
use crate::state::{ClientData, StateChange, State, Work};
use crate::timestamp::Timestamp;


#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct RedditToken {
    #[serde(default)] access_token: String,
    #[serde(default)] refresh_token: String,
    #[serde(default)] expires_at: Timestamp,
}


#[derive(Debug, Serialize, Deserialize, Clone, PartialOrd, PartialEq, Ord, Eq, Hash)]
pub struct RedditId(TinyStr16);

impl TryFrom<&str> for RedditId {
    type Error = tinystr::Error;

    fn try_from(s: &str) -> Result<RedditId, Self::Error> {
        Ok(Self(TinyStr16::from_str(s)?))
    }
}

impl Default for RedditId {
    fn default() -> Self {
        Self(TinyStr16::from_str(".").unwrap())
    }
}


#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct RedditPost {
    pub created: Timestamp,
    pub id: RedditId,
    pub url: String,
}

impl Hash for RedditPost {
    fn hash<H>(&self, hasher: &mut H)
    where
        H: Hasher,
    {
        self.id.hash(hasher)
    }
}

impl PartialOrd for RedditPost {
    fn partial_cmp(&self, other: &RedditPost) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RedditPost {
    fn cmp(&self, other: &RedditPost) -> Ordering {
        match self.created.cmp(&other.created) {
            Ordering::Equal => self.id.cmp(&other.id),
            result => result,
        }
    }
}

impl PartialEq for RedditPost {
    fn eq(&self, other: &RedditPost) -> bool {
        self.id.eq(&other.id)
    }
}

impl Eq for RedditPost {}


async fn api_request<O, I>(state: &State, function: &str, input: &I) -> Result<O>
where
    O: DeserializeOwned,
    I: Serialize,
{
    let request: Request<Body> = {
        let mut reddit_token = state.reddit_token.write().await;
        let reddit_token: &mut RedditToken = &mut reddit_token;

        ensure_logged_in(reddit_token, state).await?;
        Request::builder()
            .uri(format!("https://oauth.reddit.com{}.json?raw_json=1", function))
            .method(Method::POST)
            .header(header::AUTHORIZATION, format!("Bearer {}", &reddit_token.access_token))
            .header(header::USER_AGENT, &state.client_data.read().await.user_agent)
            .body(serde_qs::to_string(input)?.into())?
    };

    let response = with_timeout_and_cancellation_token(state, 60, state.https_client.request(request)).await?;
    let response = body::to_bytes(response.into_body()).await?;
    let response = std::str::from_utf8(&response[..])?;
    let response = serde_json::from_str(response)?;

    Ok(response)
}


async fn api_request_get<O>(state: &State, function: &str) -> Result<O>
where
    O: DeserializeOwned,
{
    let request: Request<Body> = {
        let mut reddit_token = state.reddit_token.write().await;
        let reddit_token: &mut RedditToken = &mut reddit_token;

        ensure_logged_in(reddit_token, state).await?;
        Request::builder()
            .uri(format!("https://oauth.reddit.com{}.json?raw_json=1", function))
            .method(Method::GET)
            .header(header::AUTHORIZATION, format!("Bearer {}", &reddit_token.access_token))
            .header(header::USER_AGENT, &state.client_data.read().await.user_agent)
            .body(Body::empty())?
    };

    let response = with_timeout_and_cancellation_token(state, 60, state.https_client.request(request)).await?;
    let response = body::to_bytes(response.into_body()).await?;
    let response = std::str::from_utf8(&response[..])?;
    let response = serde_json::from_str(response)?;

    Ok(response)
}


async fn ensure_logged_in(reddit_token: &mut RedditToken, state: &State) -> Result<()> {
    // https://github.com/reddit-archive/reddit/wiki/OAuth2

    let mut success = Arc::new(atomic::AtomicBool::new(false));
    let success = &mut success;
    defer! {
        if !success.load(atomic::Ordering::SeqCst) {
            error!("Reddit login failed. Cancelling execution …");
            state.cancel_token.cancel();
        }
    }

    #[derive(Debug, Serialize, Default)]
    struct RequestAccessToken<'a> {
        #[serde(default)] grant_type: Cow<'a, str>,
        #[serde(default)] code: Cow<'a, str>,
        #[serde(default)] redirect_uri: Cow<'a, str>,
    }

    #[derive(Debug, Deserialize, Default)]
    struct ResponseAccessToken {
        #[serde(default)] access_token: String,
        #[serde(default)] token_type: String,
        #[serde(default)] expires_in: i64,
        // #[serde(default)] scope: String,
        #[serde(default)] refresh_token: String,
    }

    #[derive(Debug, Serialize, Default)]
    struct RequestRefreshAccessToken {
        #[serde(default)] grant_type: String,
        #[serde(default)] refresh_token: String,
    }

    #[derive(Debug, Deserialize, Default)]
    struct ResponseRefreshAccessToken {
        #[serde(default)] access_token: String,
        #[serde(default)] token_type: String,
        #[serde(default)] expires_in: i64,
        // #[serde(default)] scope: String,
    }

    let mut changed = false;
    let now = Utc::now();
    loop {
        match &reddit_token {
            // EXPIRED?
            RedditToken{ expires_at, access_token, .. } if !access_token.is_empty() => {
                let utc: &DateTime<_> = expires_at.borrow();
                if *utc > now - Duration::minutes(1) {
                    break;
                }

                reddit_token.expires_at = Default::default();
                reddit_token.access_token = Default::default();
            }

            // SUBSEQUENT LOG IN
            RedditToken{ refresh_token, .. } if !refresh_token.is_empty() => {
                let client_data = state.client_data.read().await;
                let client_data: &ClientData = &client_data;

                let auth = format!("Basic {}", base64::encode(format!("{}:{}", &client_data.public_key, &client_data.private_key)));
                let request = Request::builder()
                    .uri("https://www.reddit.com/api/v1/access_token?raw_json=1")
                    .method(Method::POST)
                    .header(header::AUTHORIZATION, auth)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::USER_AGENT, &client_data.user_agent)
                    .body(serde_qs::to_string(&RequestRefreshAccessToken {
                        grant_type: "refresh_token".into(),
                        refresh_token: refresh_token.into(),
                    })?.into())?;
                let response = with_timeout_and_cancellation_token(state, 30, state.https_client.request(request)).await?;
                let response = body::to_bytes(response.into_body()).await?;
                let response = std::str::from_utf8(&response[..])?;
                let response: ResponseRefreshAccessToken = serde_json::from_str(response)?;

                if response.access_token.is_empty() {
                    bail!("response.access_token.is_empty()");
                }

                reddit_token.access_token = response.access_token;
                reddit_token.expires_at = (now + Duration::seconds(response.expires_in)).into();
            }

            // 1st LOG IN
            _ => {
                let client_data = state.client_data.read().await;
                let client_data: &ClientData = &client_data;

                let auth = format!("Basic {}", base64::encode(format!("{}:{}", &client_data.public_key, &client_data.private_key)));
                let request = Request::builder()
                    .uri("https://www.reddit.com/api/v1/access_token?raw_json=1")
                    .method(Method::POST)
                    .header(header::AUTHORIZATION, auth)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::USER_AGENT, &client_data.user_agent)
                    .body(serde_qs::to_string(&RequestAccessToken {
                        grant_type: "authorization_code".into(),
                        code: client_data.code.to_owned().into(),
                        redirect_uri: (&client_data.redirect_uri).into(),
                    })?.into())?;
                let response = with_timeout_and_cancellation_token(state, 30, state.https_client.request(request)).await?;
                let response = body::to_bytes(response.into_body()).await?;
                let response: ResponseAccessToken = serde_json::from_str(std::str::from_utf8(&response[..])?)?;

                if response.refresh_token.is_empty() {
                    bail!("authorization_code is not usable, most likely expired. Run init again.");
                }

                reddit_token.refresh_token = response.refresh_token;
                reddit_token.access_token = response.access_token;
                reddit_token.expires_at = (now + Duration::seconds(response.expires_in)).into();
            }
        }
        changed = true;
    }
    if changed {
        state.log_changes(&[StateChange::RedditToken(Cow::Borrowed(reddit_token))]).await?;
    }

    success.store(true, atomic::Ordering::SeqCst);
    Ok(())
}


async fn collect_new_posts(state: &State) -> Result<Vec<RedditPost>> {
    #[derive(Debug, Deserialize, Default)]
    struct Listing {
        #[serde(default)] data: ListingData,
    }

    #[derive(Debug, Deserialize, Default)]
    struct ListingData {
        // #[serde(default)] after: Option<Cow<'a, str>>,
        // #[serde(default)] before: Option<Cow<'a, str>>,
        #[serde(default)] children: Vec<Child>,
    }

    #[derive(Debug, Deserialize, Default)]
    struct Child {
        #[serde(default)] kind: ChildKind,
        #[serde(default)] data: ChildData,
    }

    #[allow(non_camel_case_types)]
    #[derive(Debug, Deserialize)]
    enum ChildKind {
        None,
        t1, t2, t3, t4, t5, t6,
    }

    impl Default for ChildKind {
        fn default() -> Self {
            Self::None
        }
    }

    #[derive(Debug, Deserialize, Default)]
    struct ChildData {
        #[serde(default)] id: String,
        #[serde(default)] is_self: bool,  // false
        #[serde(default)] locked: bool,  // false
        #[serde(default)] post_hint: Option<String>,  // link
        #[serde(default)] created_utc: f64,
        #[serde(default)] url: Option<String>,
        #[serde(default)] url_overridden_by_dest: Option<String>,
    }

    let sr_new = format!("/r/{}/new/", state.client_data.read().await.sr_to_monitor);
    let response: Listing = api_request_get(state, &sr_new).await?;
    let response = response.data.children.into_iter().filter_map(|child| match child {
        Child { kind: ChildKind::t3, data, .. } if
            !data.is_self && !data.locked && data.post_hint.as_ref().map(|s| &s[..]) == Some("link")
        => match (data.url_overridden_by_dest, data.url) {
            (Some(url), _) | (_, Some(url)) => {
                let created = data.created_utc.try_into().ok()?;
                // TODO: check age
                let id = data.id.as_str().try_into().ok()?;
                Some(RedditPost { created, id, url })
            }
            _ => {
                None
            }
        }
        _ => {
            None
        }
    }).collect();

    Ok(response)
}


pub async fn collect_loop(state: Arc<State>) -> Result<()> {
    let state: &State = state.borrow();
    defer! {
        warn!("Reddit collect loop is done. Shutting down …");
        state.cancel_token.cancel();
    }
    info!("Reddit collect loop is running.");

    loop {
        let new_posts = select! {
            _ = state.cancel_token.cancelled() => return Ok(()),
            new_posts = collect_new_posts(state) => new_posts,
        };
        let new_posts = match new_posts {
            Ok(new_posts) => {
                debug!("Collected posts: {:?}", &new_posts);
                new_posts
            }
            Err(err) => {
                dbg!(err);
                warn!("Could not collect new reddit posts.");
                Default::default()
            }
        };

        let mut inserted = false;
        if !new_posts.is_empty() {
            let mut seen_posts = state.seen_posts.write().await;
            let mut screenshot_queue = state.screenshot_queue.inner.write().await;
            let mut outline_com_queue = state.outline_com_queue.inner.write().await;
            let mut wikiwix_queue = state.wikiwix_queue.inner.write().await;
            let mut google_cache_queue = state.google_cache_queue.inner.write().await;

            for post in new_posts {
                let new_work = Work::new(post);
                if !seen_posts.contains(&new_work) {
                    info!("New post: {:?}", &new_work.meta);
                    state.log_changes(&[StateChange::NewPost(Cow::Borrowed(&new_work.meta))]).await?;

                    seen_posts.insert(new_work.clone());
                    screenshot_queue.insert(new_work.clone());
                    outline_com_queue.insert(new_work.clone());
                    wikiwix_queue.insert(new_work.clone());
                    google_cache_queue.insert(new_work.clone());

                    inserted = true;
                }
            }
        }
        if inserted {
            state.screenshot_queue.notify.notify_one();
            state.outline_com_queue.notify.notify_one();
            state.wikiwix_queue.notify.notify_one();
            state.google_cache_queue.notify.notify_one();
        }

        sleep_with_cancellation_token(state, 60).await?;
    }
}


pub async fn read_my_name(state: &State) -> Result<String> {
    #[derive(Debug, Deserialize, Default)]
    struct ResponseIdentity {
        name: String,
    }

    let response: ResponseIdentity = api_request_get(state, "/api/v1/me").await?;
    Ok(response.name)
}


pub struct ImagePreview {
    pub preview_url: String,
    pub websocket_url: Uri,
}

pub async fn upload_image_preview(state: &State, data: Mmap, filename: &str, mimetype: Mime) -> Result<ImagePreview> {
    // https://github.com/praw-dev/praw/blob/c80b92c2c636f077109629acbbd5e3f39ef1ea78/praw/models/reddit/subreddit.py#L643

    // ////////////////////////////////////////////////////////////////////////////////////////////
    // REQUEST UPLOAD ACTION
    // ////////////////////////////////////////////////////////////////////////////////////////////

    #[derive(Debug, Serialize)]
    struct RequestMediaAsset<'a> {
        filepath: &'a str,
        mimetype: &'a str,
    }

    #[derive(Debug, Deserialize, Default)]
    struct ResponseMediaAsset {
        args: ResponseMediaAssetArgs,
        asset: ResponseMediaAssetAsset,
    }

    #[derive(Debug, Deserialize, Default)]
    struct ResponseMediaAssetArgs {
        action: String,
        fields: Vec<ResponseMediaAssetArgsField>,
    }

    #[derive(Debug, Deserialize, Default)]
    struct ResponseMediaAssetArgsField {
        name: String,
        value: String,
    }

    #[derive(Debug, Deserialize, Default)]
    struct ResponseMediaAssetAsset {
        #[serde(default)] asset_id: String,
        #[serde(default)] processing_state: String,
        #[serde(default)] payload: ResponseMediaAssetAssetPayload,
        #[serde(default)] websocket_url: String,
    }

    #[derive(Debug, Deserialize, Default)]
    struct ResponseMediaAssetAssetPayload {
        #[serde(default)] filepath: String,
    }

    let asset_response: ResponseMediaAsset = api_request(state, "/api/media/asset", &RequestMediaAsset {
        filepath: &filename,
        mimetype: mimetype.as_ref(),
    }).await?;

    // ////////////////////////////////////////////////////////////////////////////////////////////
    // DO UPLOAD
    // ////////////////////////////////////////////////////////////////////////////////////////////

    #[derive(Debug, Deserialize, Default)]
    struct ResponseUpload {
        #[serde(rename="Location")] location: String,
    }

    let mut form = hyper_multipart_rfc7578::client::multipart::Form::default();
    for field in asset_response.args.fields.into_iter() {
        form.add_text(field.name, field.value);
    }
    form.add_reader_file_with_mime("file", Cursor::new(data), filename, mimetype);
    let content_type = form.content_type();

    let body: common_multipart_rfc7578::client::multipart::Body = form.into();
    let body: hyper_multipart_rfc7578::client::multipart::Body = body.into();
    let body: hyper::body::Body = body.into();
    let body = hyper::body::to_bytes(body).await?;
    let content_length = body.len();

    let request = Request::builder()
        .uri(&format!("https:{}", asset_response.args.action))
        .method(Method::POST)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, content_length)
        .body(body.into())?;

    let upload_response = with_timeout_and_cancellation_token(state, 60, state.https_client.request(request)).await?;
    let upload_response = body::to_bytes(upload_response.into_body()).await?;
    let upload_response = std::str::from_utf8(&upload_response[..])?;
    let upload_response: ResponseUpload = serde_xml_rs::de::from_str(upload_response)?;

    Ok(ImagePreview {
        preview_url: upload_response.location,
        websocket_url: parse_url(&asset_response.asset.websocket_url)?,
    })
}


pub async fn submit_image(state: &State, sr: &str, title: &str, url: &str, websocket: Uri) -> Result<String> {
    // https://github.com/praw-dev/praw/blob/c80b92c2c636f077109629acbbd5e3f39ef1ea78/praw/models/reddit/subreddit.py#L1094

    #[derive(Debug, Serialize)]
    struct RequestSubmit<'a> {
        sr: &'a str,
        title: &'a str,
        url: &'a str,
        kind: &'a str,
        resubmit: bool,
        sendreplies: bool,
        nsfw: bool,
        spoiler: bool,
        validate_on_submit: bool,
    }

    let (websocket, _) = state.https_client.websocket(websocket).await?;
    let (_, reader) = websocket.split();

    let read_websocket = async {
        #[derive(Debug, Deserialize)]
        struct ResponseWebsocket {
            payload: ResponseWebsocketPayload,
        }

        #[derive(Debug, Deserialize)]
        struct ResponseWebsocketPayload {
            redirect: String,
        }

        let mut reader = reader;
        let msg = match reader.next().await {
            Some(Err(err)) => return Err(err.into()),
            Some(Ok(Message::Text(msg))) => msg,
            Some(_) => bail!("Wrong kind of message received"),
            None => bail!("No message received"),
        };
        
        let msg: ResponseWebsocket = serde_json::from_str(&msg)?;
        match msg.payload.redirect {
            redirect if redirect.len() >= 11 => Ok(redirect),
            redirect => bail!("Redirect too short: {:?}", redirect),
        }
    };

    let submit_image = async {
        #[derive(Debug, Deserialize)]
        struct ResponseSubmit {
            success: bool,
        }

        let response: ResponseSubmit = api_request(state, "/api/submit", &RequestSubmit {
            sr,
            title,
            url,
            kind: "image",
            resubmit: false,
            sendreplies: false,
            nsfw: true,
            spoiler: false,
            validate_on_submit: true,
        }).await?;
        if !response.success {
            warn!("Upload may have failed for: {}", url);
        }
        Ok(response.success)
    };

    let (redirect, _) = with_timeout_and_cancellation_token(state, 60, async {
        try_join!(read_websocket, submit_image)
    }).await?;

    Ok(redirect)
}


pub async fn get_uploaded_image_url(state: &State, url: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Response {
        data: ResponseData,
    }

    #[derive(Deserialize)]
    struct ResponseData {
        children: Vec<ResponseChild>,
    }

    #[derive(Deserialize)]
    struct ResponseChild {
        data: ResponseChildData,
    }

    #[derive(Deserialize)]
    struct ResponseChildData {
        url_overridden_by_dest: Option<String>,
        url: Option<String>,
    }

    let url = parse_url(url)?;
    let url = match url.path_and_query() {
        Some(p) => p.path(),
        None => bail!("No path found"),
    };

    let mut results: Vec<Response> = api_request_get(state, url).await?;
    results.pop();
    if let Some(mut result) = results.pop() {
        if let Some(child) = result.data.children.pop() {
            match (child.data.url_overridden_by_dest, child.data.url) {
                (Some(url), _) if !url.is_empty() => return Ok(url),
                (_, Some(url)) if !url.is_empty() => return Ok(url),
                _ => ()
            }
        }
    }
    bail!("No url found");
}


pub async fn fetch_title(state: &State, url: &str) -> Result<String> {
    #[derive(Debug, Serialize)]
    struct RequestFetchTitle<'a> {
        url: &'a str,
        api_type: &'a str,
    }

    #[derive(Debug, Deserialize)]
    struct ResponseFetchTitle {
        json: ResponseFetchTitleJson,
    }

    #[derive(Debug, Deserialize)]
    struct ResponseFetchTitleJson {
        data: ResponseFetchTitleData,
    }

    #[derive(Debug, Deserialize)]
    struct ResponseFetchTitleData {
        title: String,
    }

    let result: ResponseFetchTitle = api_request(state, "/api/fetch_title", &RequestFetchTitle {
        url,
        api_type: "json"
    }).await?;
    Ok(result.json.data.title)
}
