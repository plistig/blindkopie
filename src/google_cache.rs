use std::sync::Arc;

use anyhow::{Result, ensure};
use hyper::{Body, Request, Method, StatusCode, header};
use serde::Serialize;

use crate::networking::{with_timeout_and_cancellation_token};
use crate::state::{State, StateChange, run_submit_loop};


async fn submit_sub(state: &State, source_url: &str) -> Result<String> {
    #[derive(Serialize)]
    struct RequestQuery {
        q: String,
    }

    let url = format!(
        "https://webcache.googleusercontent.com/search?{}",
        serde_qs::to_string(&RequestQuery { q: format!("cache:{}", source_url) })?,
    );
    let request = Request::builder()
        .uri(&url)
        .method(Method::GET)
        .header(header::DNT, "1")
        .header(header::COOKIE, "CONSENT=YES+GB.de+V13+BX")
        .body(Body::empty())?;
    let response = with_timeout_and_cancellation_token(state, 30, state.https_client.request(request)).await?;
    ensure!(response.status() == StatusCode::OK, "Unexpected status code != 200");

    Ok(url)
}


async fn submit(state: &State, source_url: &str) -> Result<String> {
    match submit_sub(state, source_url).await {
        Ok(url) => Ok(url),
        Err(err) => match source_url.split_once('?') {
            Some((source_url, _)) => submit_sub(state, source_url).await,
            None => Err(err),
        }
    }
}


pub async fn submit_loop(state: Arc<State>) -> Result<()> {
    let state: &State = &state;
    run_submit_loop(
        "Google cache", 10, 45, state, &state.google_cache_queue,
        |work| { work.comment.is_done() || work.google_cache.is_done() },
        |work| { &mut work.google_cache },
        |state, work| { Box::pin(submit(state, &work.meta.url)) },
        |meta, outcome| StateChange::GoogleCacheOutcome(meta, outcome),
    ).await
}
