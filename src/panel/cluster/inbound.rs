//! Inbound half of the federation: an agent answering its master.
//!
//! The endpoint is deliberately narrow. It reports the node's identity, and it
//! replays a Control API request against this node's own loopback Control API.
//! Nothing else is reachable, so a master that is compromised gains exactly the
//! authority the Control API already grants an operator on that host.

use std::net::IpAddr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};

use crate::panel::control;
use crate::panel::crypto::decode;
use crate::panel::http::request;
use crate::panel::http::respond::{self, PanelBody};
use crate::panel::state::{PanelState, unix_now, unix_now_ms};

use super::client::{CLUSTER_PREFIX, HelloResponse};
use super::sign::{
    HEADER_NODE, HEADER_NONCE, HEADER_SIGNATURE, HEADER_TIMESTAMP, SignedRequest, verify_inbound,
};

/// Serves one inbound cluster request.
pub(crate) async fn handle(
    request: Request<Incoming>,
    address: Option<IpAddr>,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    if !state.cluster_role().is_agent() {
        return respond::error(
            StatusCode::NOT_FOUND,
            "not_found",
            "This node does not accept cluster requests",
        );
    }
    let Some(address) = address else {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "bad_forwarded_for",
            "Forwarded client address could not be parsed",
        );
    };
    if !state.cluster_address_allowed(address) {
        return respond::error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Source address is not allowed to reach the cluster endpoint",
        );
    }

    let method = request.method().clone();
    let Some(path_and_query) = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_string())
    else {
        return respond::error(StatusCode::BAD_REQUEST, "bad_request", "Malformed target");
    };

    let headers = request.headers().clone();
    let (Some(node_id), Some(timestamp), Some(nonce), Some(signature)) = (
        request::header(&headers, HEADER_NODE),
        request::header(&headers, HEADER_TIMESTAMP).and_then(|value| value.parse::<u64>().ok()),
        request::header(&headers, HEADER_NONCE),
        request::header(&headers, HEADER_SIGNATURE),
    ) else {
        return refused("malformed_signature_headers");
    };

    let (expected_node_id, link_key) = {
        let store = state.store.read().await;
        (store.node.id.clone(), store.node.link_key.clone())
    };
    if node_id != expected_node_id {
        // The identifier is signed, so a mismatch is either a stale link or a
        // request aimed at a different node. Both answer the same way.
        return refused("unknown_node");
    }
    let Some(link_key) = decode(&link_key) else {
        return respond::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "This node's link key is unusable",
        );
    };

    let body = match request::read_body(request.into_body(), state.config.request_body_limit_bytes)
        .await
    {
        Ok(body) => body,
        Err(request::BodyError::TooLarge) => {
            return respond::error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "Request body is too large",
            );
        }
        Err(request::BodyError::Transport) => {
            return respond::error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "Body could not be read",
            );
        }
    };

    let signed = SignedRequest {
        method: method.as_str(),
        path: &path_and_query,
        node_id,
        timestamp_ms: timestamp,
        nonce,
        body: &body,
    };
    if let Err(error) = verify_inbound(
        &signed,
        &link_key,
        signature,
        unix_now_ms(),
        state.config.cluster.clock_skew_secs,
        &state.nonce_window,
    ) {
        return refused(error.as_str());
    }

    let suffix = path_and_query
        .split('?')
        .next()
        .unwrap_or_default()
        .strip_prefix(CLUSTER_PREFIX)
        .unwrap_or_default()
        .to_string();

    if suffix == "/hello" && method == hyper::Method::GET {
        return hello(&state).await;
    }
    if let Some(control_path) = suffix.strip_prefix("/control") {
        let query = path_and_query.split_once('?').map(|(_, query)| query);
        return relay(
            &state,
            method.as_str(),
            control_path,
            query,
            body,
            request::header(&headers, "content-type"),
        )
        .await;
    }
    respond::error(StatusCode::NOT_FOUND, "not_found", "Route not found")
}

/// Reports this node's identity to its master.
async fn hello(state: &Arc<PanelState>) -> Response<PanelBody> {
    let store = state.store.read().await;
    let payload = HelloResponse {
        node_id: store.node.id.clone(),
        node_name: store.node.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        role: state.cluster_role().as_str().to_string(),
        time: unix_now(),
    };
    // The cluster protocol is not the panel API, so the answer is the bare
    // payload rather than the browser envelope.
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    respond::build(StatusCode::OK, "application/json; charset=utf-8", body)
}

/// Replays one Control API request against this node's own Control API.
async fn relay(
    state: &Arc<PanelState>,
    method: &str,
    control_path: &str,
    query: Option<&str>,
    body: Vec<u8>,
    content_type: Option<&str>,
) -> Response<PanelBody> {
    if !control::is_relayable(control_path) {
        return respond::error(
            StatusCode::NOT_FOUND,
            "not_found",
            "Only the /v1 Control API surface is relayed",
        );
    }
    let upstream_path = match query {
        Some(query) if !query.is_empty() => format!("{control_path}?{query}"),
        _ => control_path.to_string(),
    };
    match control::call_local(state, method, &upstream_path, body, content_type).await {
        Ok(response) => respond::passthrough(
            response.status,
            response.content_type.as_deref(),
            response.body,
        ),
        Err(error) => respond::error(error.status(), error.code(), error.to_string()),
    }
}

/// The single answer every signature failure produces.
///
/// The reason is reported because both ends are operator-controlled and a
/// silent 403 makes a clock-skew problem indistinguishable from a wrong key.
fn refused(code: &str) -> Response<PanelBody> {
    respond::error(
        StatusCode::UNAUTHORIZED,
        code,
        "Cluster request was not accepted",
    )
}
