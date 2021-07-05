use std::sync::Arc;

use anyhow::{bail, ensure, Result};
use hyper::{body, Body, Method, Request, StatusCode};
use serde::Serialize;

use crate::networking::with_timeout_and_cancellation_token;
use crate::state::{run_submit_loop, State, StateChange};

async fn submit(state: &State, url: &str) -> Result<String> {
    #[derive(Serialize)]
    struct RequestQuery<'a> {
        url: &'a str,
        apiresponse: &'a str,
    }

    #[derive(Serialize)]
    struct ReadQuery<'a> {
        url: &'a str,
    }

    #[derive(Serialize)]
    struct CacheQuery<'a> {
        rev_t: &'a str,
        url: &'a str,
    }

    {
        let request = Request::builder()
            .uri(format!(
                "http://archive.wikiwix.com/cache/?{}",
                serde_qs::to_string(&RequestQuery {
                    url,
                    apiresponse: "1"
                })?,
            ))
            .method(Method::GET)
            .body(Body::empty())?;
        let response =
            with_timeout_and_cancellation_token(state, 30, state.https_client.request(request))
                .await?;
        ensure!(
            response.status() == StatusCode::FOUND,
            "Unexpected status code != 302"
        );

        let redirect = match response.headers().get(hyper::header::LOCATION) {
            Some(value) if !value.is_empty() => value,
            _ => bail!("No redirect found"),
        };

        match redirect.to_str()?.split_once("index2.php?url=") {
            Some(("", s)) if !s.is_empty() => (),
            _ => bail!("Unexpected redirect format"),
        }
    }

    let result = {
        let request = Request::builder()
            .uri(format!(
                "http://archive.wikiwix.com/cache/index2.php?{}",
                serde_qs::to_string(&ReadQuery { url })?,
            ))
            .method(Method::GET)
            .body(Body::empty())?;
        let response =
            with_timeout_and_cancellation_token(state, 30, state.https_client.request(request))
                .await?;
        let response = body::to_bytes(response.into_body()).await?;
        let response = String::from_utf8_lossy(&response[..]);

        let rev_t = match response
            .rsplit_once("?rev_t=")
            .and_then(|t| t.1.split_once('&'))
            .map(|t| t.0)
        {
            Some(rev_t) if !rev_t.is_empty() => rev_t,
            _ => bail!("No rev_t found"),
        };
        format!(
            "http://archive.wikiwix.com/cache/index2.php?{}",
            serde_qs::to_string(&CacheQuery { rev_t, url })?,
        )
    };

    Ok(result)
}

pub async fn submit_loop(state: Arc<State>) -> Result<()> {
    let state: &State = &state;
    run_submit_loop(
        "Wikiwix",
        30,
        45,
        state,
        &state.wikiwix_queue,
        |work| work.comment.is_done() || work.wikiwix.is_done(),
        |work| &mut work.wikiwix,
        |state, work| Box::pin(submit(state, &work.meta.url)),
        |meta, outcome| StateChange::WikiwixOutcome(meta, outcome),
    )
    .await
}
