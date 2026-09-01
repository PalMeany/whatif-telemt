//! Outbound half of the federation: a master calling a linked node.
//!
//! Every call is one signed HTTP request. There is no persistent channel and no
//! agent process to keep alive: the linked node already runs a panel, and that
//! panel's `/cluster/v1` endpoint is the whole remote surface.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::panel::crypto::{decode, random_token};
use crate::panel::httpclient::{self, HttpRequest, HttpResponse};
use crate::panel::state::{PanelState, unix_now_ms};
use crate::panel::store::LinkedNode;

use super::sign::{HEADER_NODE, HEADER_NONCE, HEADER_SIGNATURE, HEADER_TIMESTAMP, SignedRequest};

/// Path prefix of the inbound cluster endpoint.
pub(crate) const CLUSTER_PREFIX: &str = "/cluster/v1";

/// Largest response body accepted from a linked node.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// What a linked node reports about itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HelloResponse {
    /// The node's own identifier.
    pub(crate) node_id: String,
    /// The node's display name.
    pub(crate) node_name: String,
    /// telemt version running on the node.
    pub(crate) version: String,
    /// Federation role the node was configured with.
    pub(crate) role: String,
    /// Unix seconds on the node, used to surface clock drift.
    pub(crate) time: u64,
}

/// Failure of one call to a linked node.
#[derive(Debug)]
pub(crate) enum ClusterError {
    /// The node's stored link key is unusable.
    BadLinkKey,
    /// The transport failed.
    Transport(String),
    /// The node answered, but not with what the caller expected.
    Protocol(String),
}

impl std::fmt::Display for ClusterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClusterError::BadLinkKey => write!(formatter, "stored link key is not usable"),
            ClusterError::Transport(detail) => write!(formatter, "{detail}"),
            ClusterError::Protocol(detail) => write!(formatter, "{detail}"),
        }
    }
}

/// Probes a linked node and returns what it reports about itself.
pub(crate) async fn hello(
    state: &PanelState,
    node: &LinkedNode,
) -> Result<(HelloResponse, u64), ClusterError> {
    let started = Instant::now();
    let response = call(state, node, "GET", "/hello", Vec::new(), None).await?;
    let latency_ms = started.elapsed().as_millis() as u64;
    if !response.status.is_success() {
        return Err(ClusterError::Protocol(format!(
            "node answered {}",
            response.status.as_u16()
        )));
    }
    let hello: HelloResponse = serde_json::from_slice(&response.body)
        .map_err(|error| ClusterError::Protocol(format!("malformed hello payload: {error}")))?;
    Ok((hello, latency_ms))
}

/// Forwards one Control API request to a linked node.
///
/// `control_path` keeps its `/v1` prefix and its query string; the remote panel
/// re-applies it against its own Control API unchanged.
pub(crate) async fn forward_control(
    state: &PanelState,
    node: &LinkedNode,
    method: &str,
    control_path: &str,
    body: Vec<u8>,
    content_type: Option<&str>,
) -> Result<HttpResponse, ClusterError> {
    let path = format!("/control{control_path}");
    call(state, node, method, &path, body, content_type).await
}

/// Signs and performs one request against a linked node's cluster endpoint.
async fn call(
    state: &PanelState,
    node: &LinkedNode,
    method: &str,
    suffix: &str,
    body: Vec<u8>,
    content_type: Option<&str>,
) -> Result<HttpResponse, ClusterError> {
    let link_key = decode(&node.link_key).ok_or(ClusterError::BadLinkKey)?;
    let path = format!("{CLUSTER_PREFIX}{suffix}");
    let nonce = random_token(&state.random);
    let timestamp_ms = unix_now_ms();
    let signature = SignedRequest {
        method,
        path: &path,
        node_id: &node.id,
        timestamp_ms,
        nonce: &nonce,
        body: &body,
    }
    .sign(&link_key);

    let mut headers = vec![
        (HEADER_NODE.to_string(), node.id.clone()),
        (HEADER_TIMESTAMP.to_string(), timestamp_ms.to_string()),
        (HEADER_NONCE.to_string(), nonce),
        (HEADER_SIGNATURE.to_string(), signature),
        ("accept".to_string(), "application/json".to_string()),
    ];
    if let Some(content_type) = content_type {
        headers.push(("content-type".to_string(), content_type.to_string()));
    }
    if !body.is_empty() && content_type.is_none() {
        headers.push((
            "content-type".to_string(),
            "application/json; charset=utf-8".to_string(),
        ));
    }

    let url = format!("{}{path}", node.url.trim_end_matches('/'));
    httpclient::send(HttpRequest {
        url: &url,
        method,
        headers,
        body,
        timeout: Duration::from_millis(state.config.cluster.request_timeout_ms),
        max_response_bytes: MAX_RESPONSE_BYTES,
        pin_sha256: node.fingerprint.clone(),
    })
    .await
    .map_err(|error| ClusterError::Transport(error.to_string()))
}
