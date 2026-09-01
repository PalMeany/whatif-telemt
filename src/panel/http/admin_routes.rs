//! Audit log access and panel settings.

use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::Deserialize;

use crate::panel::audit::AuditEntry;
use crate::panel::rbac::Permission;
use crate::panel::state::PanelState;

use super::Caller;
use super::account_routes::{internal, read_json};
use super::request;
use super::respond::{self, PanelBody};

/// Records returned when the caller names no limit.
const DEFAULT_AUDIT_LIMIT: usize = 100;

/// Largest audit page the panel will render.
const MAX_AUDIT_LIMIT: usize = 1_000;

/// Settings update request.
#[derive(Deserialize)]
struct PatchSettingsRequest {
    #[serde(default)]
    default_node_id: Option<String>,
    #[serde(default)]
    appearance: Option<String>,
}

/// Returns the most recent audit records.
pub(crate) async fn audit_tail(
    caller: &Caller,
    state: &Arc<PanelState>,
    query: Option<&str>,
) -> Response<PanelBody> {
    if let Err(response) = caller.require(Permission::ManageOperators) {
        return response;
    }
    let limit = request::query_param(query, "limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_AUDIT_LIMIT)
        .clamp(1, MAX_AUDIT_LIMIT);
    let records = state.audit.tail(limit).await;
    respond::json(
        StatusCode::OK,
        serde_json::json!({"records": records, "enabled": state.config.audit_enabled}),
    )
}

/// Recomputes the audit chain and reports where it breaks.
pub(crate) async fn audit_verify(caller: &Caller, state: &Arc<PanelState>) -> Response<PanelBody> {
    if let Err(response) = caller.require(Permission::ManageOperators) {
        return response;
    }
    respond::json(StatusCode::OK, state.audit.verify().await)
}

/// Returns the panel's runtime-editable settings and effective bounds.
pub(crate) async fn settings(caller: &Caller, state: &Arc<PanelState>) -> Response<PanelBody> {
    let store = state.store.read().await;
    respond::json(
        StatusCode::OK,
        serde_json::json!({
            "default_node_id": store.settings.default_node_id,
            "appearance": store
                .settings
                .appearance
                .get(&caller.operator.id)
                .cloned()
                .unwrap_or_else(|| "dark".to_string()),
            "limits": {
                "session_ttl_secs": state.config.session_ttl_secs,
                "session_idle_timeout_secs": state.config.session_idle_timeout_secs,
                "max_sessions_per_operator": state.config.max_sessions_per_operator,
                "login_max_attempts": state.config.login_max_attempts,
                "login_lockout_secs": state.config.login_lockout_secs,
                "password_min_length": state.config.password_min_length,
                "require_totp": state.config.require_totp,
                "audit_enabled": state.config.audit_enabled,
                "audit_retention_days": state.config.audit_retention_days,
            },
            "cluster": {
                "enabled": state.config.cluster.enabled,
                "role": state.cluster_role().as_str(),
                "advertise_url": state.config.cluster.advertise_url,
                "poll_interval_secs": state.config.cluster.poll_interval_secs,
            }
        }),
    )
}

/// Updates the runtime-editable settings.
///
/// Appearance is the caller's own preference and needs no privilege; the
/// default node is fleet-wide and does.
pub(crate) async fn update_settings(
    request: Request<Incoming>,
    caller: Caller,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    let payload = match read_json::<PatchSettingsRequest>(request, &state).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let mut changed = Vec::new();
    {
        let mut store = state.store.write().await;
        if let Some(node_id) = payload.default_node_id.clone() {
            if !caller.operator.role.allows(Permission::ManageNodes) {
                return respond::error(
                    StatusCode::FORBIDDEN,
                    "forbidden",
                    "Changing the default node requires the admin role",
                );
            }
            let known = node_id.is_empty()
                || node_id == crate::panel::control::LOCAL_NODE_ID
                || store.node_by_id(&node_id).is_some();
            if !known {
                return respond::error(
                    StatusCode::BAD_REQUEST,
                    "unknown_node",
                    "The named node is not linked",
                );
            }
            store.settings.default_node_id = (!node_id.is_empty()).then_some(node_id);
            changed.push("default_node_id".to_string());
        }
        if let Some(appearance) = payload.appearance {
            if !matches!(appearance.as_str(), "dark" | "light" | "system") {
                return respond::error(
                    StatusCode::BAD_REQUEST,
                    "bad_appearance",
                    "appearance must be dark, light, or system",
                );
            }
            store
                .settings
                .appearance
                .insert(caller.operator.id.clone(), appearance);
            changed.push("appearance".to_string());
        }
    }
    if let Err(error) = state.persist().await {
        return internal(error.to_string());
    }
    if changed.iter().any(|field| field == "default_node_id") {
        state
            .record(AuditEntry {
                actor: caller.operator.username.clone(),
                actor_id: caller.operator.id.clone(),
                action: "settings.patch".to_string(),
                result: "ok".to_string(),
                address: caller.address_string(),
                detail: changed.join(","),
                ..AuditEntry::default()
            })
            .await;
    }
    respond::json(StatusCode::OK, serde_json::json!({"changed": changed}))
}
