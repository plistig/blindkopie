use std::borrow::Cow;
use std::sync::Arc;

use anyhow::{bail, Result};
use hyper::{body, Body, Method, Request};
use serde::{Deserialize, Serialize};

use crate::networking::with_timeout_and_cancellation_token;
use crate::state::{run_submit_loop, State, StateChange};

async fn submit(state: &State, source_url: &str) -> Result<String> {
    #[derive(Debug, Serialize, Default)]
    struct RequestQuery<'a> {
        source_url: &'a str,
    }

    #[derive(Debug, Deserialize, Default)]
    struct Response<'a> {
        #[serde(default)]
        data: Data<'a>,
        #[serde(default)]
        success: bool,
    }

    #[derive(Debug, Deserialize, Default)]
    struct Data<'a> {
        #[serde(default)]
        short_code: Cow<'a, str>,
    }

    let request = Request::builder()
        .uri(format!(
            "https://api.outline.com/v3/parse_article?{}",
            serde_qs::to_string(&RequestQuery { source_url })?,
        ))
        .method(Method::GET)
        .body(Body::empty())?;
    let response =
        with_timeout_and_cancellation_token(state, 30, state.https_client.request(request)).await?;
    let response = body::to_bytes(response.into_body()).await?;
    let response = std::str::from_utf8(&response[..])?;
    let response: Response = serde_json::from_str(response)?;

    if !response.success {
        bail!("!response.success");
    }

    let short_code = response.data.short_code;
    if short_code.is_empty() {
        bail!("short_code.is_empty()");
    }

    Ok(format!("https://outline.com/{}", short_code))
}

pub async fn submit_loop(state: Arc<State>) -> Result<()> {
    let state: &State = &state;
    run_submit_loop(
        "Outline.com",
        30,
        45,
        state,
        &state.outline_com_queue,
        |data| data.comment.is_done() || data.outline_com.is_done(),
        |data| &mut data.outline_com,
        |state, work| Box::pin(submit(state, &work.meta.url)),
        |meta, outcome| StateChange::OutlineComOutcome(meta, outcome),
    )
    .await
}
