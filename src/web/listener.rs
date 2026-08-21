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
use tracing::{debug, info, warn};

use crate::web::http::{Relay, WebBody};
use crate::web::manager::Manager;
use crate::web::runtime::WebRuntime;

/// Concurrent carrier connections one relay accepts.
const MAX_CARRIER_CONNECTIONS: usize = 4096;

/// Concurrent admin connections one relay accepts.
const MAX_ADMIN_CONNECTIONS: usize = 64;

/// Smallest read buffer hyper accepts.
const MIN_HTTP_BUFFER: usize = 8 * 1024;

/// Accepts carrier connections until the process shuts down.
pub(crate) async fn serve_carrier(listener: TcpListener, relay: Arc<Relay>) {
    let permits = Arc::new(Semaphore::new(MAX_CARRIER_CONNECTIONS));
    // Hyper covers both the request-head deadline and the keep-alive idle wait
    // with one timer, so the larger of the two configured bounds is used: a
    // shorter one would close idle carrier connections the client still owns.
    let header_timeout =
        Duration::from_millis(relay.timeouts.read_header_ms.max(relay.timeouts.idle_ms));
    let buffer = relay.limits.max_header_bytes.max(MIN_HTTP_BUFFER);
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                warn!(error = %error, "WEB carrier accept error");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            debug!(peer = %peer, "Dropping WEB carrier connection: budget exhausted");
            continue;
        };
        let _ = stream.set_nodelay(true);
        let relay = relay.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = service_fn(move |request: Request<Incoming>| {
                let relay = relay.clone();
                async move { Ok::<Response<WebBody>, Infallible>(relay.handle(request, peer).await) }
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
            if let Err(error) = connection.await {
                debug!(peer = %peer, error = %error, "WEB carrier connection ended");
            }
        });
    }
}

/// Accepts admin connections until the process shuts down.
pub(crate) async fn serve_admin(
    listener: TcpListener,
    manager: Arc<Manager>,
    runtime: Arc<WebRuntime>,
) {
    let permits = Arc::new(Semaphore::new(MAX_ADMIN_CONNECTIONS));
    loop {
        let (stream, peer) = match listener.accept().await {
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
                        crate::web::admin::handle(request, manager, runtime).await,
                    )
                }
            });
            let connection = http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service);
            if let Err(error) = connection.await {
                debug!(peer = %peer, error = %error, "WEB admin connection ended");
            }
        });
    }
}

/// Binds one relay listener, reporting the failure without aborting start-up.
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
