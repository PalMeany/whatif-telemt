//! Loopback admin endpoints of the WEB relay.
//!
//! `/healthz`, `/readyz`, and `/metrics` keep the reference paths and metric
//! names so an existing deployment's probes and dashboards keep working.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use tokio::net::TcpStream;

use crate::config::WebBackend;
use crate::web::http::{WebBody, full, insert};
use crate::web::manager::Manager;
use crate::web::metrics::WebMetricsSource;
use crate::web::runtime::WebRuntime;

/// Serves one admin request.
pub(crate) async fn handle(
    request: Request<Incoming>,
    manager: Arc<Manager>,
    runtime: Arc<WebRuntime>,
) -> Response<WebBody> {
    if request.method() != Method::GET {
        return status_only(StatusCode::METHOD_NOT_ALLOWED);
    }
    match request.uri().path() {
        "/healthz" => text(StatusCode::OK, "ok\n"),
        "/readyz" => {
            if ready(&manager, &runtime).await {
                text(StatusCode::OK, "ready\n")
            } else {
                text(StatusCode::SERVICE_UNAVAILABLE, "backend unavailable\n")
            }
        }
        "/metrics" => {
            let body = manager.snapshot().render("tproxy_");
            let mut response = Response::new(full(Bytes::from(body)));
            insert(
                response.headers_mut(),
                "content-type",
                "text/plain; version=0.0.4",
            );
            response
        }
        _ => status_only(StatusCode::NOT_FOUND),
    }
}

/// True when every profile can currently accept a new stream.
async fn ready(manager: &Arc<Manager>, runtime: &Arc<WebRuntime>) -> bool {
    let dial_timeout = Duration::from_millis(manager.timeouts.backend_dial_ms);
    for profile in &manager.profiles().profiles {
        match profile.backend {
            WebBackend::Internal => {
                if !runtime.admission_open() {
                    return false;
                }
            }
            WebBackend::Loopback(address) => {
                let dialed = tokio::time::timeout(dial_timeout, TcpStream::connect(address)).await;
                if !matches!(dialed, Ok(Ok(_))) {
                    return false;
                }
            }
        }
    }
    true
}

fn text(status: StatusCode, body: &'static str) -> Response<WebBody> {
    let mut response = Response::new(full(Bytes::from_static(body.as_bytes())));
    *response.status_mut() = status;
    insert(
        response.headers_mut(),
        "content-type",
        "text/plain; charset=utf-8",
    );
    response
}

fn status_only(status: StatusCode) -> Response<WebBody> {
    let mut response = Response::new(full(Bytes::new()));
    *response.status_mut() = status;
    response
}
