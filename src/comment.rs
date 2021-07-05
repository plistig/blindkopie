use std::sync::Arc;

use anyhow::Result;
use log::{info, warn};
use scopeguard::defer;

use crate::networking::{await_with_cancellation_token, sleep_with_cancellation_token};
use crate::state::{State, WorkInner, WorkOutcome};

pub async fn comment_loop(state: Arc<State>) -> Result<()> {
    let state: &State = &state;
    defer! {
        warn!("Comment loop is done. Shutting down …");
        state.cancel_token.cancel();
    }
    info!("Comment loop running.");

    loop {
        loop {
            if state.cancel_token.is_cancelled() {
                return Ok(());
            }

            let work = {
                let mut queue = state.comment_queue.inner.write().await;
                if let Some(work) = queue.ordered_queue.pop() {
                    queue.unordered_set.remove(&work);

                    let mut data = work.data.write().await;
                    let data: &mut WorkInner = &mut data;

                    if data.comment.is_done() || data.is_failure() {
                        continue;
                    }

                    data.outline_com = WorkOutcome::Processing;

                    work.clone()
                } else {
                    break;
                }
            };
            info!("Will comment: {:?}", &work.meta);

            // TODO

            sleep_with_cancellation_token(state, 60).await?;
        }
        await_with_cancellation_token(state, &state.comment_queue.notify).await?;
    }
}
