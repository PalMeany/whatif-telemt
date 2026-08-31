//! Carrier and admin listeners of the WEB relay.
//!
//! Both listeners are plaintext HTTP/1.1 on a loopback address: the reference
//! deployment terminates TLS and ACME in the front proxy, which is also what
//! gives the bridge a publicly trusted certificate for the public hostname.

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
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::fork::web::http::{Relay, WebBody, request_timeout_response};
use crate::fork::web::manager::Manager;
use crate::fork::web::runtime::WebRuntime;

/// Concurrent admin connections one relay accepts.
const MAX_ADMIN_CONNECTIONS: usize = 64;

/// Deadline for the request head of an admin connection.
const ADMIN_HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// Connections held back to answer an exhausted carrier budget.
///
/// The refusal is itself bounded, so a flood cannot turn "out of connection
/// slots" into unbounded work; past this the socket really is dropped.
const MAX_CARRIER_OVERFLOW_CONNECTIONS: usize = 256;

/// Deadline for reading the head of a connection that is only going to be
/// refused, and for writing that refusal.
const OVERFLOW_DEADLINE: Duration = Duration::from_secs(5);

/// Smallest read buffer hyper accepts.
const MIN_HTTP_BUFFER: usize = 8 * 1024;

/// Accepts carrier connections until the relay is cancelled.
pub(crate) async fn serve_carrier(
    listener: TcpListener,
    relay: Arc<Relay>,
    shutdown: CancellationToken,
) {
    let permits = Arc::new(Semaphore::new(relay.limits.max_carrier_connections));
    let overflow_permits = Arc::new(Semaphore::new(MAX_CARRIER_OVERFLOW_CONNECTIONS));
    // Hyper covers both the request-head deadline and the keep-alive idle wait
    // with one timer, so the larger of the two configured bounds is used: a
    // shorter one would close idle carrier connections the client still owns.
    let header_timeout =
        Duration::from_millis(relay.timeouts.read_header_ms.max(relay.timeouts.idle_ms));
    // Everything after the head has to be bounded here. Hyper disarms its own
    // deadline once the head parses, and a long poll is the only request that
    // legitimately takes a long time, so it sets the floor.
    let request_timeout = relay.request_deadline();
    let buffer = relay.limits.max_header_bytes.max(MIN_HTTP_BUFFER);
    loop {
        let accepted = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("WEB carrier listener stopped accepting");
                return;
            }
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                warn!(error = %error, "WEB carrier accept error");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let permit = match permits.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                // Capacity pressure is answered, not dropped. The bridge page
                // retries only on 503; a closed socket reaches it as the front
                // proxy's 502, which every carrier treats as fatal, so one
                // dropped connection costs the client its whole session rather
                // than a second of delay. This mirrors the reference, which has
                // no accept ceiling at all and answers capacity with 503.
                let Ok(overflow) = overflow_permits.clone().try_acquire_owned() else {
                    debug!(peer = %peer, "Dropping WEB carrier connection: budget exhausted");
                    relay.manager.count_carrier_connection_dropped();
                    continue;
                };
                debug!(peer = %peer, "Refusing WEB carrier connection: budget exhausted");
                relay.manager.count_retry_later();
                let _ = stream.set_nodelay(true);
                tokio::spawn(async move {
                    let _overflow = overflow;
                    let _ = tokio::time::timeout(OVERFLOW_DEADLINE, serve_overflow(stream)).await;
                });
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let relay = relay.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = service_fn(move |request: Request<Incoming>| {
                let relay = relay.clone();
                async move {
                    let served =
                        tokio::time::timeout(request_timeout, relay.handle(request, peer)).await;
                    let response = match served {
                        Ok(response) => response,
                        Err(_) => {
                            debug!(peer = %peer, "WEB carrier request exceeded its deadline");
                            relay.manager.count_request_timeout();
                            request_timeout_response()
                        }
                    };
                    Ok::<Response<WebBody>, Infallible>(response)
                }
            });
            let mut builder = http1::Builder::new();
            builder
                // A timer is mandatory for hyper's header timeout to arm.
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(header_timeout)
                .keep_alive(true)
                .max_buf_size(buffer);
            let connection = builder
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .with_upgrades();
            tokio::pin!(connection);
            let result = tokio::select! {
                result = &mut connection => result,
                _ = shutdown.cancelled() => {
                    // A carrier connection may be an upgraded WebSocket, which
                    // never completes on its own; the shutdown sequence closes
                    // the sessions behind it right after cancelling.
                    connection.as_mut().graceful_shutdown();
                    connection.await
                }
            };
            if let Err(error) = result {
                debug!(peer = %peer, error = %error, "WEB carrier connection ended");
            }
        });
    }
}

/// Accepts admin connections until the relay is cancelled.
pub(crate) async fn serve_admin(
    listener: TcpListener,
    manager: Arc<Manager>,
    runtime: Arc<WebRuntime>,
    shutdown: CancellationToken,
) {
    let permits = Arc::new(Semaphore::new(MAX_ADMIN_CONNECTIONS));
    loop {
        let accepted = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                info!("WEB admin listener stopped accepting");
                return;
            }
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                warn!(error = %error, "WEB admin accept error");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            continue;
        };
        let manager = manager.clone();
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = service_fn(move |request: Request<Incoming>| {
                let manager = manager.clone();
                let runtime = runtime.clone();
                async move {
                    Ok::<Response<WebBody>, Infallible>(
                        crate::fork::web::admin::handle(request, manager, runtime).await,
                    )
                }
            });
            // The same bound the carrier listener gets. Without it, sixty-four
            // loopback connections that never finish a request head hold every
            // admin slot and the health probes stop answering.
            let mut builder = http1::Builder::new();
            builder
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(ADMIN_HEADER_TIMEOUT)
                .keep_alive(true);
            let connection =
                builder.serve_connection(hyper_util::rt::TokioIo::new(stream), service);
            if let Err(error) = connection.await {
                debug!(peer = %peer, error = %error, "WEB admin connection ended");
            }
        });
    }
}

/// Binds one relay listener, returning `None` so the caller can turn the
/// failure into the fatal start-up error it now is.
pub(crate) async fn bind(address: SocketAddr, purpose: &str) -> Option<TcpListener> {
    match TcpListener::bind(address).await {
        Ok(listener) => {
            info!(%address, purpose, "WEB proxy listener bound");
            Some(listener)
        }
        Err(error) => {
            warn!(%address, purpose, error = %error, "Failed to bind WEB proxy listener");
            None
        }
    }
}

/// Answers one connection that arrived past the carrier budget.
///
/// Keep-alive is off: the point is to hand back the retryable answer and free
/// the socket, not to hold a slot the relay does not have.
async fn serve_overflow(stream: tokio::net::TcpStream) {
    let service = service_fn(|_request: Request<Incoming>| async move {
        Ok::<Response<WebBody>, Infallible>(request_timeout_response())
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(OVERFLOW_DEADLINE)
        .keep_alive(false);
    let _ = builder
        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
        .await;
}
