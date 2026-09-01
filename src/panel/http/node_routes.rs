//! Node linking, probing, and the local node's own link token.
//!
//! A link is symmetric in exactly one place: the HMAC key. The agent mints it,
//! the operator carries it to the master inside one opaque token, and from then
//! on the master proves possession of it on every request. There is no
//! enrolment handshake and no certificate authority, because both would add a
//! second trust root to keep synchronised across a fleet.

use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use serde::{Deserialize, Serialize};

use crate::panel::audit::AuditEntry;
use crate::panel::cluster::client;
use crate::panel::cluster::link::{self, LinkToken};
use crate::panel::control::LOCAL_NODE_ID;
use crate::panel::rbac::Permission;
use crate::panel::state::{NodeHealth, PanelState, unix_now};
use crate::panel::store::LinkedNode;

use super::Caller;
use super::account_routes::{internal, read_json};
use super::respond::{self, PanelBody};

/// Longest accepted node display name.
const MAX_NODE_NAME_LEN: usize = 64;

/// Tags one node may carry.
const MAX_NODE_TAGS: usize = 16;

/// Link request: either an opaque token or its expanded fields.
#[derive(Deserialize)]
struct LinkRequest {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    fingerprint: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

/// Node update request.
#[derive(Deserialize)]
struct PatchNodeRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    fingerprint: Option<String>,
}

/// One node as the UI lists it.
#[derive(Serialize)]
struct NodeView {
    id: String,
    name: String,
    kind: &'static str,
    url: Option<String>,
    tags: Vec<String>,
    pinned: bool,
    added_at: u64,
    reachable: bool,
    checked_at: u64,
    latency_ms: Option<u64>,
    version: Option<String>,
    error: Option<String>,
}

/// Lists the local node and every linked node.
pub(crate) async fn list(caller: &Caller, state: &Arc<PanelState>) -> Response<PanelBody> {
    if let Err(response) = caller.require(Permission::ViewNode) {
        return response;
    }
    let store = state.store.read().await;
    let mut nodes = vec![NodeView {
        id: LOCAL_NODE_ID.to_string(),
        name: store.node.name.clone(),
        kind: "local",
        url: None,
        tags: Vec::new(),
        pinned: false,
        added_at: store.node.created_at,
        reachable: true,
        checked_at: unix_now(),
        latency_ms: None,
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        error: None,
    }];
    for node in &store.nodes {
        let health = state.node_health_of(&node.id).unwrap_or_default();
        nodes.push(NodeView {
            id: node.id.clone(),
            name: node.name.clone(),
            kind: "linked",
            url: Some(node.url.clone()),
            tags: node.tags.clone(),
            pinned: node.fingerprint.is_some(),
            added_at: node.added_at,
            reachable: health.reachable,
            checked_at: health.checked_at,
            latency_ms: health.latency_ms,
            version: health.version,
            error: health.error,
        });
    }
    respond::json(StatusCode::OK, serde_json::json!({"nodes": nodes}))
}

/// Links one node into this panel.
pub(crate) async fn link(
    request: Request<Incoming>,
    caller: Caller,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    if let Err(response) = caller.require(Permission::ManageNodes) {
        return response;
    }
    if !state.cluster_role().is_master() {
        return respond::error(
            StatusCode::CONFLICT,
            "federation_disabled",
            "Set panel.cluster.enabled and a master role before linking nodes",
        );
    }
    let payload = match read_json::<LinkRequest>(request, &state).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let token = match resolve_token(&payload) {
        Ok(token) => token,
        Err(message) => return respond::error(StatusCode::BAD_REQUEST, "bad_link", message),
    };

    let candidate = LinkedNode {
        id: token.id.clone(),
        name: payload
            .name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| {
                if token.name.is_empty() {
                    token.id.clone()
                } else {
                    token.name.clone()
                }
            }),
        url: token.url.clone(),
        link_key: token.key.clone(),
        fingerprint: token.fp.clone(),
        tags: normalize_tags(payload.tags.unwrap_or_default()),
        added_at: unix_now(),
    };
    if candidate.name.len() > MAX_NODE_NAME_LEN {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "bad_name",
            format!("node name must contain at most {MAX_NODE_NAME_LEN} characters"),
        );
    }
    {
        let store = state.store.read().await;
        if candidate.id == store.node.id {
            return respond::error(
                StatusCode::CONFLICT,
                "self_link",
                "A node cannot be linked into itself",
            );
        }
        if store.node_by_id(&candidate.id).is_some() {
            return respond::error(
                StatusCode::CONFLICT,
                "already_linked",
                "This node is already linked",
            );
        }
    }

    // The link is proven before it is stored: a token that cannot reach its
    // node, or whose key the node does not accept, is an operator error worth
    // reporting at paste time rather than at first use.
    let (hello, latency_ms) = match client::hello(&state, &candidate).await {
        Ok(result) => result,
        Err(error) => {
            record_audit(
                &state,
                &caller,
                "node.link",
                &candidate.id,
                "unreachable",
                error.to_string(),
            )
            .await;
            return respond::error(
                StatusCode::BAD_GATEWAY,
                "node_unreachable",
                error.to_string(),
            );
        }
    };
    if hello.node_id != candidate.id {
        return respond::error(
            StatusCode::CONFLICT,
            "identity_mismatch",
            format!(
                "node answered as '{}' but the token names '{}'",
                hello.node_id, candidate.id
            ),
        );
    }

    state.record_node_health(
        &candidate.id,
        NodeHealth {
            reachable: true,
            checked_at: unix_now(),
            latency_ms: Some(latency_ms),
            error: None,
            version: Some(hello.version.clone()),
        },
    );
    {
        let mut store = state.store.write().await;
        store.nodes.push(candidate.clone());
    }
    if let Err(error) = state.persist().await {
        return internal(error.to_string());
    }
    record_audit(
        &state,
        &caller,
        "node.link",
        &candidate.id,
        "ok",
        format!("url={} version={}", candidate.url, hello.version),
    )
    .await;
    respond::json(
        StatusCode::CREATED,
        serde_json::json!({
            "id": candidate.id,
            "name": candidate.name,
            "url": candidate.url,
            "version": hello.version,
            "role": hello.role,
            "clock_skew_secs": (hello.time as i64 - unix_now() as i64).abs(),
        }),
    )
}

/// Routes the by-identifier node verbs.
pub(crate) async fn by_id(
    request: Request<Incoming>,
    method: Method,
    rest: String,
    caller: Caller,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    let (id, action) = match rest.split_once('/') {
        Some((id, action)) => (id.to_string(), Some(action.to_string())),
        None => (rest, None),
    };
    if id.is_empty() {
        return respond::error(StatusCode::NOT_FOUND, "not_found", "Route not found");
    }
    match (method.as_str(), action.as_deref()) {
        ("POST", Some("probe")) => probe(id, caller, state).await,
        ("PATCH", None) => patch(request, id, caller, state).await,
        ("DELETE", None) => unlink(id, caller, state).await,
        _ => respond::error(StatusCode::NOT_FOUND, "not_found", "Route not found"),
    }
}

/// Probes one linked node and records the outcome.
async fn probe(id: String, caller: Caller, state: Arc<PanelState>) -> Response<PanelBody> {
    if let Err(response) = caller.require(Permission::ViewNode) {
        return response;
    }
    let node = {
        let store = state.store.read().await;
        store.node_by_id(&id).cloned()
    };
    let Some(node) = node else {
        return respond::error(StatusCode::NOT_FOUND, "not_found", "Node is not linked");
    };
    let now = unix_now();
    match client::hello(&state, &node).await {
        Ok((hello, latency_ms)) => {
            state.record_node_health(
                &id,
                NodeHealth {
                    reachable: true,
                    checked_at: now,
                    latency_ms: Some(latency_ms),
                    error: None,
                    version: Some(hello.version.clone()),
                },
            );
            respond::json(
                StatusCode::OK,
                serde_json::json!({
                    "reachable": true,
                    "latency_ms": latency_ms,
                    "version": hello.version,
                    "role": hello.role,
                    "node_name": hello.node_name,
                    "clock_skew_secs": (hello.time as i64 - now as i64).abs(),
                }),
            )
        }
        Err(error) => {
            let detail = error.to_string();
            state.record_node_health(
                &id,
                NodeHealth {
                    reachable: false,
                    checked_at: now,
                    latency_ms: None,
                    error: Some(detail.clone()),
                    version: None,
                },
            );
            respond::json(
                StatusCode::OK,
                serde_json::json!({"reachable": false, "error": detail}),
            )
        }
    }
}

/// Applies a sparse update to one linked node.
async fn patch(
    request: Request<Incoming>,
    id: String,
    caller: Caller,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    if let Err(response) = caller.require(Permission::ManageNodes) {
        return response;
    }
    let payload = match read_json::<PatchNodeRequest>(request, &state).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    if let Some(url) = payload.url.as_deref()
        && let Err(reason) = link::validate_url(url)
    {
        return respond::error(StatusCode::BAD_REQUEST, "bad_url", reason);
    }
    if let Some(fingerprint) = payload.fingerprint.as_deref()
        && !fingerprint.is_empty()
        && (fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "bad_fingerprint",
            "fingerprint must be a SHA-256 hex digest",
        );
    }
    let mut changed = Vec::new();
    {
        let mut store = state.store.write().await;
        let Some(node) = store.nodes.iter_mut().find(|node| node.id == id) else {
            return respond::error(StatusCode::NOT_FOUND, "not_found", "Node is not linked");
        };
        if let Some(name) = payload.name.filter(|name| !name.is_empty()) {
            if name.len() > MAX_NODE_NAME_LEN {
                return respond::error(
                    StatusCode::BAD_REQUEST,
                    "bad_name",
                    format!("node name must contain at most {MAX_NODE_NAME_LEN} characters"),
                );
            }
            node.name = name;
            changed.push("name".to_string());
        }
        if let Some(tags) = payload.tags {
            node.tags = normalize_tags(tags);
            changed.push("tags".to_string());
        }
        if let Some(url) = payload.url {
            node.url = url.trim_end_matches('/').to_string();
            changed.push("url".to_string());
        }
        if let Some(fingerprint) = payload.fingerprint {
            node.fingerprint = (!fingerprint.is_empty()).then_some(fingerprint);
            changed.push("fingerprint".to_string());
        }
    }
    if let Err(error) = state.persist().await {
        return internal(error.to_string());
    }
    record_audit(&state, &caller, "node.patch", &id, "ok", changed.join(",")).await;
    respond::json(
        StatusCode::OK,
        serde_json::json!({"id": id, "changed": changed}),
    )
}

/// Removes one linked node.
async fn unlink(id: String, caller: Caller, state: Arc<PanelState>) -> Response<PanelBody> {
    if let Err(response) = caller.require(Permission::ManageNodes) {
        return response;
    }
    let removed = {
        let mut store = state.store.write().await;
        let before = store.nodes.len();
        store.nodes.retain(|node| node.id != id);
        if store.settings.default_node_id.as_deref() == Some(id.as_str()) {
            store.settings.default_node_id = None;
        }
        before != store.nodes.len()
    };
    if !removed {
        return respond::error(StatusCode::NOT_FOUND, "not_found", "Node is not linked");
    }
    if let Err(error) = state.persist().await {
        return internal(error.to_string());
    }
    state.node_health.lock().remove(&id);
    record_audit(&state, &caller, "node.unlink", &id, "ok", String::new()).await;
    respond::json(StatusCode::OK, serde_json::json!({"unlinked": id}))
}

/// Resolves the submitted link material into one token.
fn resolve_token(payload: &LinkRequest) -> Result<LinkToken, String> {
    if let Some(raw) = payload.token.as_deref().filter(|token| !token.is_empty()) {
        return LinkToken::parse(raw).map_err(str::to_string);
    }
    let (Some(url), Some(key)) = (payload.url.as_deref(), payload.key.as_deref()) else {
        return Err("either a link token or a url and key are required".to_string());
    };
    // The expanded form is reconstructed into a token and validated by exactly
    // the same rules, so the two entry points cannot drift apart.
    let token = LinkToken::new(
        payload
            .name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "node".to_string()),
        payload.name.clone().unwrap_or_default(),
        url.to_string(),
        key.to_string(),
        payload
            .fingerprint
            .clone()
            .filter(|fingerprint| !fingerprint.is_empty()),
    );
    LinkToken::parse(&token.render()).map_err(str::to_string)
}

/// Trims, deduplicates, and bounds the tag list.
fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut normalized: Vec<String> = tags
        .into_iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty() && tag.len() <= 32)
        .collect();
    normalized.sort();
    normalized.dedup();
    normalized.truncate(MAX_NODE_TAGS);
    normalized
}

/// Appends one audit record for a node action.
pub(super) async fn record_audit(
    state: &Arc<PanelState>,
    caller: &Caller,
    action: &str,
    target: &str,
    result: &str,
    detail: String,
) {
    state
        .record(AuditEntry {
            actor: caller.operator.username.clone(),
            actor_id: caller.operator.id.clone(),
            action: action.to_string(),
            target: target.to_string(),
            result: result.to_string(),
            address: caller.address_string(),
            detail,
            ..AuditEntry::default()
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::crypto::encode;

    #[test]
    fn tags_are_normalised_deduplicated_and_bounded() {
        let tags = normalize_tags(vec![
            "  EU ".to_string(),
            "eu".to_string(),
            String::new(),
            "a".repeat(64),
            "asia".to_string(),
        ]);
        assert_eq!(tags, vec!["asia".to_string(), "eu".to_string()]);
        let many: Vec<String> = (0..40).map(|index| format!("tag{index}")).collect();
        assert_eq!(normalize_tags(many).len(), MAX_NODE_TAGS);
    }

    #[test]
    fn the_expanded_link_form_is_validated_like_a_token() {
        let payload = LinkRequest {
            token: None,
            url: Some("https://node.example.com".to_string()),
            key: Some(encode(&[3u8; 32])),
            fingerprint: None,
            name: Some("edge".to_string()),
            tags: None,
        };
        assert!(resolve_token(&payload).is_ok());

        let bad_key = LinkRequest {
            key: Some(encode(&[3u8; 4])),
            ..LinkRequest {
                token: None,
                url: Some("https://node.example.com".to_string()),
                key: None,
                fingerprint: None,
                name: None,
                tags: None,
            }
        };
        assert!(resolve_token(&bad_key).is_err());
    }
}
