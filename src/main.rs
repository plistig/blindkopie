#![feature(control_flow_enum)]
#![feature(exit_status_error)]
#![feature(try_trait_v2)]

use anyhow::{Result, Context, bail};
use clap::clap_app;
use log::{info, warn};
use tokio::runtime::Builder;
use tokio::join;

mod comment;
mod google_cache;
mod networking;
mod outline_com;
mod reddit;
mod screenshot;
mod state;
mod timestamp;
mod wikiwix;


fn main() -> Result<()> {
    pretty_env_logger::init();

    let matches = clap_app!(blindkopie =>
        (version: "0.0.1")
        (author: "Peter Listig </u/plistig>")
        (about: "Screenshot reddit posts")
        (@subcommand run =>
            (about: "Start service")
        )
        (@subcommand init =>
            (about: "Setup basic service information")
        )
    ).get_matches();
    let (subcommand, _) = matches.subcommand();
    if subcommand.is_empty() {
        bail!("No subcommand provided.");
    }

    Builder::new_multi_thread().enable_all().build().context("Setup Tokio runtime")?.block_on(async {
        match subcommand {
            "init" => state::subcommand_init().await,
            "run" => subcommand_run().await,
            _ => bail!("Impossible"),
        }
    })
}


async fn subcommand_run() -> Result<()> {
    info!("Starting up …");

    let state_record = state::State::new().await?;

    {
        let state_record = state_record.clone();
        ctrlc::set_handler(move || {
            warn!("Trapped Ctrl+C. Cancelling execution …");
            state_record.cancel_token.cancel();
        })?;
    }

    info!("Starting up asynchronous loops …");
    let outcome = join!(
        state::changelog_loop(state_record.clone()),
        reddit::collect_loop(state_record.clone()),
        outline_com::submit_loop(state_record.clone()),
        wikiwix::submit_loop(state_record.clone()),
        google_cache::submit_loop(state_record.clone()),
        screenshot::screenshot_loop(state_record.clone()),
        comment::comment_loop(state_record.clone()),
    );
    dbg!(&outcome);
    info!("Bye!");

    Ok(())
}
