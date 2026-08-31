//! The optional dedicated panel listener.
//!
//! With `[fork.prometheus] listen` empty the panel is answered on the existing
//! metrics listener and inherits its whitelist and connection budget. An
//! operator who wants the panel reachable from a workstation without also
//! exposing `/metrics` and `/beobachten` to that audience sets `listen`, and
//! this listener serves the panel and nothing else.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::maestro::generation::RuntimeGeneration;
use crate::transport::{ListenOptions, create_listener};

/// Concurrent connections this listener will hold.
const MAX_CONNECTIONS: usize = 64;

/// Budget for one whole connection, matching the metrics listener's.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(15);

/// Serves the panel on its own address until the process ends.
///
/// Returns immediately when the panel has no dedicated listener configured.
pub(crate) async fn serve(active_runtime: Arc<ArcSwap<RuntimeGeneration>>, listen_backlog: u32) {
    let config = active_runtime.load().config();
    if !config.fork.prometheus_enabled() || config.fork.prometheus.listen.is_empty() {
        return;
    }
    let Ok(addr) = config.fork.prometheus.listen.parse::<SocketAddr>() else {
        warn!(
            listen = %config.fork.prometheus.listen,
            "Invalid fork.prometheus.listen; the panel listener is disabled"
        );
        return;
    };

    let ipv6_only = addr.is_ipv6() && !addr.ip().is_unspecified();
    let options = ListenOptions {
        reuse_port: false,
        ipv6_only,
        backlog: listen_backlog,
        ..Default::default()
    };
    let listener = match create_listener(addr, &options)
        .and_then(|socket| TcpListener::from_std(socket.into()))
    {
        Ok(listener) => listener,
        Err(error) => {
            warn!(error = %error, %addr, "Failed to bind the Prometheus panel listener");
            return;
        }
    };
    info!(
        "Prometheus panel: http://{}{}",
        addr, config.fork.prometheus.path
    );
    accept_loop(listener, active_runtime).await;
}

async fn accept_loop(listener: TcpListener, active_runtime: Arc<ArcSwap<RuntimeGeneration>>) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                warn!(error = %error, "Prometheus panel accept error");
                continue;
            }
        };

        let runtime = active_runtime.load_full();
        let config = runtime.config();
        // Checked before any HTTP is parsed, exactly as the metrics listener
        // does: a refused peer learns nothing but a closed connection.
        let whitelist = &config.fork.prometheus.whitelist;
        if !whitelist.is_empty() && !whitelist.iter().any(|net| net.contains(peer.ip())) {
            debug!(peer = %peer, "Prometheus panel request denied by whitelist");
            continue;
        }
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            debug!(peer = %peer, "Dropping panel connection: connection budget exhausted");
            continue;
        };

        let active_runtime = active_runtime.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                let runtime = active_runtime.load_full();
                async move { answer(request, runtime).await }
            });
            match timeout(
                CONNECTION_TIMEOUT,
                http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => debug!(error = %error, "Prometheus panel connection error"),
                Err(_) => debug!(peer = %peer, "Prometheus panel connection timed out"),
            }
        });
    }
}

/// Answers one request on the dedicated listener.
///
/// Two paths only. `/metrics` is not optional here: the panel document scrapes
/// it from its own origin under `connect-src 'self'`, so a listener that served
/// the page alone would render a shell that can never load data. `/beobachten`
/// is deliberately absent — it is per-IP forensic data the page never reads,
/// and it is the reason an operator gives the panel its own address.
async fn answer<B>(
    request: Request<B>,
    runtime: Arc<RuntimeGeneration>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let config = runtime.config();
    if super::is_panel_path(&config, request.uri().path()) {
        return Ok(super::render(&config, &runtime.rng));
    }
    if request.uri().path() == super::METRICS_PATH {
        let body = crate::metrics::render_for_runtime(&runtime).await;
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
            .header("cache-control", "no-store")
            .body(Full::new(Bytes::from(body)))
            .expect("a rendered metrics response is always well formed"));
    }
    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from_static(b"Not Found\n")))
        .expect("a static error response is always well formed"))
}
