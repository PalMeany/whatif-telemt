//! Routing one Control API request to the node that should serve it.
//!
//! The panel has exactly one way to learn or change anything about a node: that
//! node's Control API. For this node it is a loopback call; for a linked node it
//! is a signed cluster call that the remote panel replays against its own
//! loopback Control API. Both return the Control API's own JSON envelope
//! untouched, so the browser sees one contract regardless of which node served
//! it.

use std::time::Duration;

use hyper::StatusCode;

use super::cluster::client::{self, ClusterError};
use super::httpclient::{self, HttpRequest, HttpResponse};
use super::state::PanelState;
use super::store::LinkedNode;

/// Identifier the UI uses for the node the panel runs on.
pub(crate) const LOCAL_NODE_ID: &str = "local";

/// Largest Control API response the panel will relay.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Which node a request is addressed to.
#[derive(Debug, Clone)]
pub(crate) enum NodeRef {
    /// The node this panel runs on.
    Local,
    /// A node linked into this panel.
    Linked(Box<LinkedNode>),
}

impl NodeRef {
    /// Identifier used in audit records and error payloads.
    pub(crate) fn id(&self) -> &str {
        match self {
            NodeRef::Local => LOCAL_NODE_ID,
            NodeRef::Linked(node) => &node.id,
        }
    }
}

/// Why a Control API call could not be completed.
#[derive(Debug)]
pub(crate) enum ControlError {
    /// The named node is not linked into this panel.
    UnknownNode(String),
    /// The node is linked but this panel is not a master.
    FederationDisabled,
    /// The call failed before an answer was produced.
    Unreachable(String),
}

impl ControlError {
    /// Machine-readable code returned to the browser.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            ControlError::UnknownNode(_) => "unknown_node",
            ControlError::FederationDisabled => "federation_disabled",
            ControlError::Unreachable(_) => "node_unreachable",
        }
    }

    /// HTTP status returned to the browser.
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            ControlError::UnknownNode(_) => StatusCode::NOT_FOUND,
            ControlError::FederationDisabled => StatusCode::CONFLICT,
            ControlError::Unreachable(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::UnknownNode(id) => write!(formatter, "node '{id}' is not linked"),
            ControlError::FederationDisabled => {
                write!(
                    formatter,
                    "this panel is not configured as a cluster master"
                )
            }
            ControlError::Unreachable(detail) => write!(formatter, "{detail}"),
        }
    }
}

/// Resolves a node identifier from the request into a target.
pub(crate) async fn resolve(
    state: &PanelState,
    node_id: Option<&str>,
) -> Result<NodeRef, ControlError> {
    let Some(node_id) = node_id.filter(|id| !id.is_empty()) else {
        return Ok(NodeRef::Local);
    };
    if node_id == LOCAL_NODE_ID {
        return Ok(NodeRef::Local);
    }
    {
        let store = state.store.read().await;
        if node_id == store.node.id {
            return Ok(NodeRef::Local);
        }
    }
    if !state.cluster_role().is_master() {
        return Err(ControlError::FederationDisabled);
    }
    let store = state.store.read().await;
    store
        .node_by_id(node_id)
        .cloned()
        .map(|node| NodeRef::Linked(Box::new(node)))
        .ok_or_else(|| ControlError::UnknownNode(node_id.to_string()))
}

/// Performs one Control API request against the resolved node.
pub(crate) async fn forward(
    state: &PanelState,
    target: &NodeRef,
    method: &str,
    control_path: &str,
    body: Vec<u8>,
    content_type: Option<&str>,
) -> Result<HttpResponse, ControlError> {
    match target {
        NodeRef::Local => call_local(state, method, control_path, body, content_type).await,
        NodeRef::Linked(node) => {
            client::forward_control(state, node, method, control_path, body, content_type)
                .await
                .map_err(|error| match error {
                    ClusterError::BadLinkKey => ControlError::Unreachable(
                        "the stored link key for this node is unusable".to_string(),
                    ),
                    other => ControlError::Unreachable(other.to_string()),
                })
        }
    }
}

/// Calls this node's own Control API over loopback.
pub(crate) async fn call_local(
    state: &PanelState,
    method: &str,
    control_path: &str,
    body: Vec<u8>,
    content_type: Option<&str>,
) -> Result<HttpResponse, ControlError> {
    let url = format!("{}{control_path}", state.control.url);
    let mut headers = vec![("accept".to_string(), "application/json".to_string())];
    if !state.control.auth_header.is_empty() {
        headers.push((
            "authorization".to_string(),
            state.control.auth_header.clone(),
        ));
    }
    if let Some(content_type) = content_type {
        headers.push(("content-type".to_string(), content_type.to_string()));
    } else if !body.is_empty() {
        headers.push((
            "content-type".to_string(),
            "application/json; charset=utf-8".to_string(),
        ));
    }
    httpclient::send(HttpRequest {
        url: &url,
        method,
        headers,
        body,
        timeout: Duration::from_millis(state.config.request_timeout_ms),
        max_response_bytes: MAX_RESPONSE_BYTES,
        pin_sha256: None,
    })
    .await
    .map_err(|error| ControlError::Unreachable(error.to_string()))
}

/// True when the path is a Control API route the panel is willing to relay.
///
/// The panel relays the documented `/v1` surface and nothing else. Refusing
/// anything outside it keeps the browser from being turned into a generic
/// request forwarder aimed at whatever else happens to answer on the Control
/// API's address.
pub(crate) fn is_relayable(path: &str) -> bool {
    if !path.starts_with("/v1/") && path != "/v1" {
        return false;
    }
    // A traversal segment could climb out of `/v1` once the string reaches the
    // upstream, which parses it as a path rather than as an opaque token.
    !path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_versioned_control_surface_is_relayable() {
        assert!(is_relayable("/v1/health"));
        assert!(is_relayable("/v1/users/alice"));
        assert!(!is_relayable("/metrics"));
        assert!(!is_relayable("/v2/health"));
        assert!(!is_relayable("/v1/../metrics"));
        assert!(!is_relayable("v1/health"));
    }

    #[test]
    fn error_codes_map_onto_distinct_statuses() {
        assert_eq!(
            ControlError::UnknownNode("x".into()).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ControlError::FederationDisabled.status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            ControlError::Unreachable("x".into()).status(),
            StatusCode::BAD_GATEWAY
        );
    }
}
