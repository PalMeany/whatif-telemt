//! Control API relay and the fleet overview.
//!
//! The relay is the panel's whole reason to exist: the browser speaks to the
//! Control API of any node in the fleet through one authenticated, audited,
//! role-checked path. Nothing about the Control API's contract is rewritten on
//! the way through, so the browser client and the documented API stay in step.

use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use serde::Serialize;
use tokio::task::JoinSet;

use crate::panel::audit::AuditEntry;
use crate::panel::control::{self, ControlError, LOCAL_NODE_ID, NodeRef};
use crate::panel::rbac::control_api_permission;
use crate::panel::state::PanelState;

use super::Caller;
use super::request::{self, HEADER_NODE};
use super::respond::{self, PanelBody};

/// Nodes probed concurrently while building the overview.
const OVERVIEW_CONCURRENCY: usize = 8;

/// One node's line in the fleet overview.
#[derive(Serialize)]
struct OverviewRow {
    node_id: String,
    node_name: String,
    reachable: bool,
    error: Option<String>,
    summary: Option<serde_json::Value>,
    ready: Option<serde_json::Value>,
}

/// Relays one Control API request to the selected node.
pub(crate) async fn relay(
    request: Request<Incoming>,
    method: Method,
    control_path: String,
    query: Option<String>,
    caller: Caller,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    if !control::is_relayable(&control_path) {
        return respond::error(
            StatusCode::NOT_FOUND,
            "not_found",
            "Only the /v1 Control API surface is relayed",
        );
    }
    let permission = control_api_permission(method.as_str(), &control_path);
    if let Err(response) = caller.require(permission) {
        return response;
    }

    let node_id = request::query_param(query.as_deref(), "node")
        .or_else(|| request::header(request.headers(), HEADER_NODE).map(str::to_string));
    let target = match control::resolve(&state, node_id.as_deref()).await {
        Ok(target) => target,
        Err(error) => return control_error(error),
    };

    let content_type = request::header(request.headers(), "content-type").map(str::to_string);
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

    let upstream_path = build_upstream_path(&control_path, query.as_deref());
    let result = control::forward(
        &state,
        &target,
        method.as_str(),
        &upstream_path,
        body,
        content_type.as_deref(),
    )
    .await;

    // Reads are not audited: they would drown the log the mutations live in,
    // and the panel already records every session that could issue them.
    let mutating = request::is_mutating(&method);
    match result {
        Ok(response) => {
            if mutating {
                audit(
                    &state,
                    &caller,
                    &target,
                    &method,
                    &control_path,
                    &response.status.as_u16().to_string(),
                )
                .await;
            }
            respond::passthrough(
                response.status,
                response.content_type.as_deref(),
                response.body,
            )
        }
        Err(error) => {
            if mutating {
                audit(
                    &state,
                    &caller,
                    &target,
                    &method,
                    &control_path,
                    error.code(),
                )
                .await;
            }
            control_error(error)
        }
    }
}

/// Builds a compact cross-node summary for the dashboard.
pub(crate) async fn overview(caller: &Caller, state: &Arc<PanelState>) -> Response<PanelBody> {
    if let Err(response) = caller.require(crate::panel::rbac::Permission::ViewNode) {
        return response;
    }
    let mut targets: Vec<(String, String, NodeRef)> = Vec::new();
    {
        let store = state.store.read().await;
        targets.push((
            LOCAL_NODE_ID.to_string(),
            store.node.name.clone(),
            NodeRef::Local,
        ));
        if state.cluster_role().is_master() {
            for node in &store.nodes {
                targets.push((
                    node.id.clone(),
                    node.name.clone(),
                    NodeRef::Linked(Box::new(node.clone())),
                ));
            }
        }
    }

    let mut rows = Vec::with_capacity(targets.len());
    for chunk in targets.chunks(OVERVIEW_CONCURRENCY) {
        let mut set = JoinSet::new();
        for (id, name, target) in chunk {
            let state = state.clone();
            let id = id.clone();
            let name = name.clone();
            let target = target.clone();
            set.spawn(async move { probe_node(state, id, name, target).await });
        }
        while let Some(joined) = set.join_next().await {
            if let Ok(row) = joined {
                rows.push(row);
            }
        }
    }
    rows.sort_by(|left, right| left.node_id.cmp(&right.node_id));
    respond::json(StatusCode::OK, serde_json::json!({"nodes": rows}))
}

/// Reads the two summary endpoints of one node.
async fn probe_node(
    state: Arc<PanelState>,
    node_id: String,
    node_name: String,
    target: NodeRef,
) -> OverviewRow {
    let summary = control::forward(
        &state,
        &target,
        "GET",
        "/v1/stats/summary",
        Vec::new(),
        None,
    )
    .await;
    let summary = match summary {
        Ok(response) => response,
        Err(error) => {
            return OverviewRow {
                node_id,
                node_name,
                reachable: false,
                error: Some(error.to_string()),
                summary: None,
                ready: None,
            };
        }
    };
    let ready = control::forward(&state, &target, "GET", "/v1/health/ready", Vec::new(), None)
        .await
        .ok()
        .and_then(|response| unwrap_envelope(&response.body));
    OverviewRow {
        node_id,
        node_name,
        reachable: true,
        error: None,
        summary: unwrap_envelope(&summary.body),
        ready,
    }
}

/// Unwraps the Control API envelope down to its `data` payload.
///
/// The relay hands the envelope through untouched, but the overview inlines two
/// payloads into one row of its own, so the wrappers would otherwise nest.
fn unwrap_envelope(body: &[u8]) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    match value.get("data") {
        Some(data) => Some(data.clone()),
        None => Some(value),
    }
}

/// Rebuilds the upstream query string without the panel's own `node` selector.
fn build_upstream_path(control_path: &str, query: Option<&str>) -> String {
    let Some(query) = query else {
        return control_path.to_string();
    };
    let forwarded: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.starts_with("node=") && *pair != "node")
        .collect();
    if forwarded.is_empty() {
        control_path.to_string()
    } else {
        format!("{control_path}?{}", forwarded.join("&"))
    }
}

/// Renders a routing failure.
fn control_error(error: ControlError) -> Response<PanelBody> {
    respond::error(error.status(), error.code(), error.to_string())
}

/// Appends one audit record for a relayed mutation.
async fn audit(
    state: &Arc<PanelState>,
    caller: &Caller,
    target: &NodeRef,
    method: &Method,
    control_path: &str,
    result: &str,
) {
    state
        .record(AuditEntry {
            actor: caller.operator.username.clone(),
            actor_id: caller.operator.id.clone(),
            action: format!("control.{}", method.as_str().to_lowercase()),
            target: control_path.to_string(),
            node: target.id().to_string(),
            result: result.to_string(),
            address: caller.address_string(),
            ..AuditEntry::default()
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_node_selector_is_stripped_from_the_upstream_query() {
        assert_eq!(
            build_upstream_path("/v1/runtime/events/recent", Some("node=edge-1&limit=50")),
            "/v1/runtime/events/recent?limit=50"
        );
        assert_eq!(
            build_upstream_path("/v1/stats/summary", Some("node=edge-1")),
            "/v1/stats/summary"
        );
        assert_eq!(
            build_upstream_path("/v1/stats/summary", None),
            "/v1/stats/summary"
        );
        // A parameter that merely starts with the same letters is preserved.
        assert_eq!(
            build_upstream_path("/v1/x", Some("nodes=3")),
            "/v1/x?nodes=3"
        );
    }
}
