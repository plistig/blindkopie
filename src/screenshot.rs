use std::borrow::Cow;
use std::fs::canonicalize;
use std::os::unix::io::AsRawFd;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Result;
use log::{info, warn};
use memfd::MemfdOptions;
use memmap::{Mmap, MmapOptions};
use nix::unistd::getpid;
use scopeguard::defer;
use tokio::process::Command;
use tokio::try_join;

use crate::networking::{
    await_with_cancellation_token, sleep_with_cancellation_token,
    with_timeout_and_cancellation_token, OK,
};
use crate::reddit::{
    fetch_title, get_uploaded_image_url, submit_image, upload_image_preview, ImagePreview,
};
use crate::state::{State, StateChange, WorkInner, WorkOutcome};

struct Screenshot {
    text: String,
    png: Mmap,
}

async fn screenshot(source_url: &str, state: &State) -> Result<Screenshot> {
    let mypid = getpid();

    let png_file = MemfdOptions::new()
        .close_on_exec(true)
        .create("screenshot.png")?
        .into_file();
    let png_path = format!("/proc/{}/fd/{}", mypid, png_file.as_raw_fd());

    let png_index_file = MemfdOptions::new()
        .close_on_exec(true)
        .create("screenshot_index.png")?
        .into_file();
    let png_index_path = format!("/proc/{}/fd/{}", mypid, png_index_file.as_raw_fd());

    let txt_file = MemfdOptions::new()
        .close_on_exec(true)
        .create("text.md")?
        .into_file();
    let txt_path = format!("/proc/{}/fd/{}", mypid, txt_file.as_raw_fd());

    with_timeout_and_cancellation_token(state, 45, async {
        Command::new("/usr/bin/env")
            .arg("./env/bin/python")
            .arg("./readermode_screenshot.py")
            .arg(source_url)
            .arg(&png_path)
            .arg(txt_path)
            .env("VIRTUAL_ENV", canonicalize("env")?)
            .stdin(Stdio::null())
            .spawn()?
            .wait()
            .await?
            .exit_ok()?;
        OK
    })
    .await?;

    with_timeout_and_cancellation_token(state, 15, async {
        Command::new("/usr/bin/env")
            .arg("convert")
            .arg(png_path)
            .arg("-colors")
            .arg("255")
            .arg(png_index_path)
            .stdin(Stdio::null())
            .spawn()?
            .wait()
            .await?
            .exit_ok()?;
        OK
    })
    .await?;

    let text = std::str::from_utf8(&unsafe { MmapOptions::new().map(&txt_file) }?)?.to_owned();
    let png = unsafe { MmapOptions::new().map(&png_index_file) }?;

    Ok(Screenshot { text, png })
}

#[derive(Debug, Clone)]
struct SubmittedScreenshot {
    post_url: String,
    image_url: String,
    text: String,
}

async fn submit(source_url: &str, state: &State) -> Result<SubmittedScreenshot> {
    let (Screenshot { text, png }, title) = try_join!(
        screenshot(source_url, state),
        fetch_title(state, source_url),
    )?;

    let ImagePreview {
        preview_url,
        websocket_url,
    } = upload_image_preview(state, png, "screenshot.png", mime::IMAGE_PNG).await?;
    let sr = state.client_data.read().await.sr_to_post_to.clone();
    let post_url = submit_image(state, &sr, &title, &preview_url, websocket_url).await?;
    let image_url = get_uploaded_image_url(state, &post_url).await?;

    Ok(SubmittedScreenshot {
        post_url,
        image_url,
        text,
    })
}

pub async fn screenshot_loop(state: Arc<State>) -> Result<()> {
    let state: &State = &state;
    defer! {
        warn!("Screenshot loop is done. Shutting down …");
        state.cancel_token.cancel();
    }
    info!("Screenshot loop running.");

    loop {
        loop {
            if state.cancel_token.is_cancelled() {
                return Ok(());
            }

            let work = {
                let mut queue = state.screenshot_queue.inner.write().await;
                if let Some(work) = queue.ordered_queue.pop() {
                    queue.unordered_set.remove(&work);

                    let mut data = work.data.write().await;
                    let data: &mut WorkInner = &mut data;

                    if data.comment.is_done() || data.screenshot.is_done() {
                        continue;
                    }
                    data.screenshot = WorkOutcome::Processing;

                    work.clone()
                } else {
                    break;
                }
            };
            info!("Will screenshot: {:?}", &work.meta);

            let result =
                with_timeout_and_cancellation_token(state, 60, submit(&work.meta.url, &state))
                    .await;
            let do_comment = {
                let mut data = work.data.write().await;
                let data: &mut WorkInner = &mut data;

                match result {
                    Ok(submitted_screenshot) => {
                        info!(
                            "Screenshot success: {:?} => {:?}",
                            &work.meta.url, &submitted_screenshot.image_url
                        );
                        state
                            .log_changes(&[
                                StateChange::ScreenshotOutcome(
                                    Cow::Borrowed(&work.meta),
                                    Some(Cow::Borrowed(&submitted_screenshot.image_url)),
                                ),
                                StateChange::TextOutcome(
                                    Cow::Borrowed(&work.meta),
                                    Some(Cow::Borrowed(&submitted_screenshot.text)),
                                ),
                            ])
                            .await?;
                        data.screenshot = WorkOutcome::Success(submitted_screenshot.image_url);
                        data.text = WorkOutcome::Success(submitted_screenshot.text);
                    }
                    Err(err) => {
                        dbg!(err);
                        warn!("Screenshot failure: {:?}", &work.meta.url);
                        state
                            .log_changes(&[
                                StateChange::ScreenshotOutcome(Cow::Borrowed(&work.meta), None),
                                StateChange::TextOutcome(Cow::Borrowed(&work.meta), None),
                            ])
                            .await?;
                        data.screenshot = WorkOutcome::Failure;
                        data.text = WorkOutcome::Failure;
                    }
                }

                data.ready_to_comment()
            };

            if do_comment && state.comment_queue.inner.write().await.insert(work) {
                state.comment_queue.notify.notify_one();
            }

            sleep_with_cancellation_token(state, 10).await?;
        }
        await_with_cancellation_token(state, &state.screenshot_queue.notify).await?;
    }
}
