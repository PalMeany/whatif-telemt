// Route handlers return a fully built `Response` on the error path so a refusal
// carries the same hardening headers and envelope as any other answer. Boxing it
// to shrink the `Result` would buy nothing here: these are control-plane calls,
// one per operator action, never a hot path.
#![allow(clippy::result_large_err)]

//! Built-in web panel.
//!
//! The panel is a control-plane surface bolted onto the same process as the
//! proxy, and it never touches the data plane. It serves an embedded
//! single-page application, a JSON API that authenticates operators, and a
//! signed node-to-node endpoint that lets one panel drive a fleet.
//!
//! Everything the panel shows or changes about a node goes through that node's
//! Control API (`[server.api]`). Nothing is cached across requests, so a
//! configuration reload or a runtime generation swap is visible immediately and
//! the panel cannot render a view of a retired generation.
//!
//! Submodules:
//! - `audit`: hash-chained audit log
//! - `cluster`: node federation, signing, and the inbound endpoint
//! - `control`: routing one Control API request to the node that serves it
//! - `crypto`: password, token, and encoding primitives
//! - `http`: the HTTP surface and its routes
//! - `httpclient`: the outbound HTTP/1.1 client, with certificate pinning
//! - `listener`: listener binding and the accept loop
//! - `password`: password hashing policy and verification
//! - `ratelimit`: login throttling
//! - `rbac`: roles and the permission gate
//! - `session`: the operator session registry
//! - `state`: process-scoped panel state and bootstrap
//! - `store`: the persisted store
//! - `tls`: TLS termination and the served certificate's fingerprint
//! - `totp`: RFC 6238 second factors

use std::path::Path;

use hyper::{Response, StatusCode};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::ProxyConfig;
use crate::error::{ProxyError, Result};

pub(crate) mod audit;
pub(crate) mod cluster;
pub(crate) mod control;
pub(crate) mod crypto;
pub(crate) mod http;
pub(crate) mod httpclient;
pub(crate) mod listener;
pub(crate) mod password;
pub(crate) mod ratelimit;
pub(crate) mod rbac;
pub(crate) mod session;
pub(crate) mod state;
pub(crate) mod store;
#[cfg(test)]
mod tests;
pub(crate) mod tls;
pub(crate) mod totp;

use http::respond::{self, PanelBody};
use state::PanelState;

/// Cancellation token of the running panel, published once its listener binds.
static ACTIVE_PANEL: Mutex<Option<CancellationToken>> = Mutex::new(None);

/// Checks everything about `[panel]` that can be checked without binding.
///
/// Runs before the privilege drop, for the same reason the WEB relay's does:
/// after the drop the process may no longer be able to read the certificate it
/// was told to serve, and after the listeners are live a failure is a restart
/// loop.
pub(crate) fn preflight(config: &ProxyConfig) -> Result<()> {
    if !config.panel.enabled {
        return Ok(());
    }
    config.panel.validate()?;
    if config.panel.tls.enabled {
        for (field, path) in [
            ("panel.tls.cert_path", &config.panel.tls.cert_path),
            ("panel.tls.key_path", &config.panel.tls.key_path),
        ] {
            if !Path::new(path).is_file() {
                return Err(ProxyError::Config(format!(
                    "{field} is not a readable file"
                )));
            }
        }
    }
    Ok(())
}

/// Starts the panel listener when `[panel]` enables it.
pub(crate) async fn start(config: &ProxyConfig, config_dir: Option<&Path>) -> Result<()> {
    if !config.panel.enabled {
        return Ok(());
    }
    let state = PanelState::bootstrap(config, config_dir).await?;
    let tls = if config.panel.tls.enabled {
        Some(tls::server_config(&config.panel.tls).await?)
    } else {
        None
    };
    let Some(listener) = listener::bind(state.listen).await else {
        return Err(ProxyError::Config(format!(
            "panel listener could not bind {}",
            state.listen
        )));
    };

    let shutdown = CancellationToken::new();
    if !http::assets::is_bundled() {
        warn!(
            "Panel UI bundle is not compiled into this binary; the panel API works but the \
             interface will not load"
        );
    }
    info!(
        listen = %state.listen,
        tls = tls.is_some(),
        cluster = state.config.cluster.enabled,
        role = state.cluster_role().as_str(),
        "Panel endpoint: {}://{}/",
        if tls.is_some() { "https" } else { "http" },
        state.listen
    );

    tokio::spawn(listener::serve(
        listener,
        state.clone(),
        tls,
        shutdown.clone(),
    ));
    if state.cluster_role().is_master() {
        tokio::spawn(cluster::poll::run(state.clone(), shutdown.clone()));
    }
    *ACTIVE_PANEL.lock() = Some(shutdown);
    Ok(())
}

/// Stops the panel listener and its background tasks.
pub(crate) fn shutdown() {
    let token = ACTIVE_PANEL.lock().take();
    if let Some(token) = token {
        token.cancel();
        info!("Panel stopped");
    }
}

/// Answer served when a panel request exceeds its deadline.
pub(crate) fn respond_timeout(tls: bool) -> Response<PanelBody> {
    let response = respond::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "request_timeout",
        "The request exceeded its deadline",
    );
    if tls {
        respond::with_hsts(response)
    } else {
        response
    }
}
