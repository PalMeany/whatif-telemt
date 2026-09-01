//! Panel listener and accept loop.
//!
//! One listener serves the application shell, the panel API, and the inbound
//! cluster endpoint. It is bounded the same way the WEB carrier is: a
//! connection budget, a header deadline, and a per-request deadline, so neither
//! a slow client nor a flood can hold the control plane open.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_rustls::TlsAcceptor;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::http;
use super::respond_timeout;
use super::state::PanelState;

/// Smallest read buffer hyper accepts.
const MIN_HTTP_BUFFER: usize = 8 * 1024;

/// Largest request head the panel reads.
const MAX_HEADER_BYTES: usize = 32 * 1024;

/// Accepts panel connections until the panel is cancelled.
pub(crate) async fn serve(
    listener: TcpListener,
    state: Arc<PanelState>,
    tls: Option<Arc<rustls::ServerConfig>>,
    shutdown: CancellationToken,
) {
    let permits = Arc::new(Semaphore::new(state.config.max_connections));
    let header_timeout = Duration::from_millis(state.config.header_read_timeout_ms);
    let request_timeout = Duration::from_millis(state.config.request_timeout_ms);
    let acceptor = tls.clone().map(TlsAcceptor::from);
    loop {
        let accepted = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("Panel listener stopped accepting");
                return;
            }
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                warn!(error = %error, "Panel accept error");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            debug!(peer = %peer, "Dropping panel connection: connection budget exhausted");
            continue;
        };
        let _ = stream.set_nodelay(true);
        let state = state.clone();
        let acceptor = acceptor.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match acceptor {
                Some(acceptor) => {
                    // The TLS handshake gets the header deadline: a client that
                    // opens a socket and never negotiates must not hold a slot.
                    let handshake =
                        tokio::time::timeout(header_timeout, acceptor.accept(stream)).await;
                    match handshake {
                        Ok(Ok(stream)) => {
                            serve_connection(
                                hyper_util::rt::TokioIo::new(stream),
                                peer,
                                state,
                                true,
                                header_timeout,
                                request_timeout,
                                shutdown,
                            )
                            .await;
                        }
                        Ok(Err(error)) => {
                            debug!(peer = %peer, error = %error, "Panel TLS handshake failed");
                        }
                        Err(_) => {
                            debug!(peer = %peer, "Panel TLS handshake timed out");
                        }
                    }
                }
                None => {
                    serve_connection(
                        hyper_util::rt::TokioIo::new(stream),
                        peer,
                        state,
                        false,
                        header_timeout,
                        request_timeout,
                        shutdown,
                    )
                    .await;
                }
            }
        });
    }
}

/// Serves one established connection.
async fn serve_connection<T>(
    io: T,
    peer: SocketAddr,
    state: Arc<PanelState>,
    tls: bool,
    header_timeout: Duration,
    request_timeout: Duration,
    shutdown: CancellationToken,
) where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let service = service_fn(move |request: Request<Incoming>| {
        let state = state.clone();
        async move {
            let served =
                tokio::time::timeout(request_timeout, http::handle(request, peer, state, tls))
                    .await;
            let response = match served {
                Ok(response) => response,
                Err(_) => {
                    debug!(peer = %peer, "Panel request exceeded its deadline");
                    respond_timeout(tls)
                }
            };
            Ok::<Response<http::respond::PanelBody>, Infallible>(response)
        }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(header_timeout)
        .keep_alive(true)
        .max_buf_size(MAX_HEADER_BYTES.max(MIN_HTTP_BUFFER));
    let connection = builder.serve_connection(io, service);
    tokio::pin!(connection);
    let result = tokio::select! {
        result = &mut connection => result,
        _ = shutdown.cancelled() => {
            connection.as_mut().graceful_shutdown();
            connection.await
        }
    };
    if let Err(error) = result {
        debug!(peer = %peer, error = %error, "Panel connection ended");
    }
}

/// Binds the panel listener, reporting a failure without aborting start-up.
pub(crate) async fn bind(address: SocketAddr) -> Option<TcpListener> {
    match TcpListener::bind(address).await {
        Ok(listener) => {
            info!(%address, "Panel listener bound");
            Some(listener)
        }
        Err(error) => {
            warn!(%address, error = %error, "Failed to bind panel listener");
            None
        }
    }
}
