use std::convert::{TryInto, TryFrom};
use std::ops::{Try, ControlFlow, FromResidual};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Result};
use futures::future::Future;
use hyper::{Body, Uri, Response, Request};
use hyper::client::{Client, HttpConnector};
use hyper::service::Service;
use hyper_rustls::{HttpsConnector, MaybeHttpsStream};
use hyper_trust_dns_connector::AsyncHyperResolver;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use tokio::net::TcpStream;
use tokio::select;
use tokio::sync::{Notify, Mutex};
use tokio::time::sleep;
use tokio_tungstenite::{client_async_with_config, WebSocketStream};
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};

use crate::state::State;


type SafeConnector = HttpsConnector<HttpConnector<AsyncHyperResolver>>;

#[derive(Debug)]
pub struct SafeHttpsClient {
    https_connector: Mutex<SafeConnector>,
    https_client: Client<SafeConnector, Body>,
}

impl Default for SafeHttpsClient {
    fn default() -> Self {
        let resolver = AsyncHyperResolver::new(ResolverConfig::cloudflare_https(), ResolverOpts {
            edns0: true,
            cache_size: 32,
            use_hosts_file: false,
            num_concurrent_reqs: 8,
            preserve_intermediates: true,
            ..Default::default()
        }).unwrap();

        let mut http = HttpConnector::new_with_resolver(resolver);
        http.enforce_http(false);

        let mut tls = rustls::ClientConfig::new();
        tls.root_store.add_server_trust_anchors(&webpki_roots::TLS_SERVER_ROOTS);
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        tls.ct_logs = Some(&ct_logs::LOGS);

        let https_connector = HttpsConnector::from((http, tls));
        let https_client = Client::builder().build(https_connector.clone());
        let https_connector = Mutex::new(https_connector);

        Self { https_connector, https_client }
    }
}

impl SafeHttpsClient {
    pub async fn request(&self, req: hyper::Request<Body>) -> hyper::Result<hyper::Response<hyper::Body>> {
        Ok(self.https_client.request(req).await?)
    }

    pub async fn connect(&self, uri: Uri) -> Result<<SafeConnector as Service<Uri>>::Response> {
        let outcome = {
            let mut guard = self.https_connector.lock().await;
            guard.call(uri)
        }.await;
        let cnx = match outcome {
            Ok(cnx) => cnx,
            Err(err) => bail!("Could not connect: {}", err),
        };
        Ok(cnx)
    }

    pub async fn websocket(&self, ws_uri: Uri) -> Result<(WebSocketStream<MaybeHttpsStream<TcpStream>>, Response<()>)> {
        let mut http_uri = ws_uri.clone().into_parts();

        let mut ws_uri = ws_uri.into_parts();
        match ws_uri.scheme {
            Some(s) if s.as_str().eq_ignore_ascii_case("http") || s.as_str().eq_ignore_ascii_case("ws") => {
                ws_uri.scheme = Some("ws".parse()?);
            }
            Some(s) if s.as_str().eq_ignore_ascii_case("https") || s.as_str().eq_ignore_ascii_case("wss") => {
                ws_uri.scheme = Some("wss".parse()?)
            }
            _ => bail!("Wrong scheme"),
        };
        let ws_uri = Uri::try_from(ws_uri)?;

        match http_uri.scheme {
            Some(s) if s.as_str().eq_ignore_ascii_case("http") || s.as_str().eq_ignore_ascii_case("ws") => {
                http_uri.scheme = Some("http".parse()?);
            }
            Some(s) if s.as_str().eq_ignore_ascii_case("https") || s.as_str().eq_ignore_ascii_case("wss") => {
                http_uri.scheme = Some("https".parse()?)
            }
            _ => bail!("Wrong scheme"),
        };
        http_uri.path_and_query = Some("/".try_into()?);
        let http_uri = Uri::try_from(http_uri)?;

        let ws = client_async_with_config(
            Request::get(ws_uri).body(())?,
            self.connect(http_uri).await?,
            None,
        ).await?;

        Ok(ws)
    }
}


pub async fn with_timeout_and_cancellation_token<T, E, F>(state: &State, timeout_seconds: u64, future: F) ->
    Result<T>
where
    anyhow::Error: From<E>,
    F: Future<Output = Result<T, E>>,
{
    select! {
        _ = state.cancel_token.cancelled() => {
            bail!("Cancelled");
        }
        _ = sleep(Duration::from_secs(timeout_seconds)) => {
            bail!("Timeout");
        }
        result = future => {
            let result = result?;
            Ok(result)
        }
    }
}


pub const OK: Result<(), anyhow::Error> = Ok(());


#[derive(Debug, Clone, Copy)]
pub enum AwaitWithCancellationTokenOutcome {
    Cancelled,
    Continue,
}

#[derive(Debug, Clone, Copy)]
pub struct AwaitWithCancellationTokenCancelled;

impl Try for AwaitWithCancellationTokenOutcome {
    type Output = ();
    type Residual = AwaitWithCancellationTokenCancelled;

    fn branch(self) -> ControlFlow<Self::Residual, Self::Output> {
        match self {
            AwaitWithCancellationTokenOutcome::Cancelled => ControlFlow::Break(AwaitWithCancellationTokenCancelled),
            AwaitWithCancellationTokenOutcome::Continue => ControlFlow::Continue(()),
        }
    }

    fn from_output(_: Self::Output) -> Self {
        AwaitWithCancellationTokenOutcome::Continue
    }
}

impl FromResidual<AwaitWithCancellationTokenCancelled> for AwaitWithCancellationTokenOutcome {
    fn from_residual(_: AwaitWithCancellationTokenCancelled) -> Self {
        AwaitWithCancellationTokenOutcome::Cancelled
    }
}

impl<E> FromResidual<AwaitWithCancellationTokenCancelled> for Result<(), E> {
    fn from_residual(_: AwaitWithCancellationTokenCancelled) -> Self {
        Ok(())
    }
}

pub async fn await_with_cancellation_token(state: &State, notify: &Notify) -> AwaitWithCancellationTokenOutcome {
    select! {
        _ = state.cancel_token.cancelled() => AwaitWithCancellationTokenOutcome::Cancelled,
        _ = notify.notified() => AwaitWithCancellationTokenOutcome::Continue,
    }
}

pub async fn sleep_with_cancellation_token(state: &State, secs:u64) -> AwaitWithCancellationTokenOutcome {
    select! {
        _ = state.cancel_token.cancelled() => AwaitWithCancellationTokenOutcome::Cancelled,
        _ = sleep(Duration::from_secs(secs)) => AwaitWithCancellationTokenOutcome::Continue,
    }
}


pub fn parse_url(url: &str) -> Result<Uri> {
    const NEEDS_ESCAPING: AsciiSet = CONTROLS.add(b' ').add(b'\x7f').add(b'(').add(b')');
    Ok(Uri::from_str(&utf8_percent_encode(url, &NEEDS_ESCAPING).to_string())?)
}
