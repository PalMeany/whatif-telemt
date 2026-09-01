//! Operator account administration.
//!
//! Every route here is administrator-only, and every one of them is checked
//! against the "last active administrator" invariant: an account change that
//! removes the final administrator would leave the panel with no way to manage
//! its own accounts again.

use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use serde::{Deserialize, Serialize};

use crate::panel::audit::AuditEntry;
use crate::panel::crypto::{encode, random_secret};
use crate::panel::password;
use crate::panel::rbac::{Permission, Role};
use crate::panel::state::{PanelState, unix_now};

use super::Caller;
use super::account_routes::{internal, read_json};
use super::respond::{self, PanelBody};

/// Longest accepted login name.
const MAX_USERNAME_LEN: usize = 64;

/// Operator creation request.
#[derive(Deserialize)]
struct CreateOperatorRequest {
    username: String,
    password: String,
    role: String,
    #[serde(default = "default_true")]
    must_change_password: bool,
}

/// Operator update request; every field is optional.
#[derive(Deserialize)]
struct PatchOperatorRequest {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    reset_totp: Option<bool>,
}

/// One operator as the UI lists it.
#[derive(Serialize)]
struct OperatorView {
    id: String,
    username: String,
    role: &'static str,
    disabled: bool,
    must_change_password: bool,
    totp_enabled: bool,
    created_at: u64,
    last_login_at: Option<u64>,
    active_sessions: usize,
}

/// Default for `must_change_password` on a freshly created account.
fn default_true() -> bool {
    true
}

/// Lists every operator account.
pub(crate) async fn list(caller: &Caller, state: &Arc<PanelState>) -> Response<PanelBody> {
    if let Err(response) = caller.require(Permission::ManageOperators) {
        return response;
    }
    let now = unix_now();
    let store = state.store.read().await;
    let operators: Vec<OperatorView> = store
        .operators
        .iter()
        .map(|operator| OperatorView {
            id: operator.id.clone(),
            username: operator.username.clone(),
            role: operator.role.as_str(),
            disabled: operator.disabled,
            must_change_password: operator.must_change_password,
            totp_enabled: operator.has_totp(),
            created_at: operator.created_at,
            last_login_at: operator.last_login_at,
            active_sessions: state.sessions.list_for_operator(&operator.id, now).len(),
        })
        .collect();
    respond::json(StatusCode::OK, serde_json::json!({"operators": operators}))
}

/// Creates one operator account.
pub(crate) async fn create(
    request: Request<Incoming>,
    caller: Caller,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    if let Err(response) = caller.require(Permission::ManageOperators) {
        return response;
    }
    let payload = match read_json::<CreateOperatorRequest>(request, &state).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    if !is_valid_username(&payload.username) {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "bad_username",
            format!("username must match [A-Za-z0-9_.-] and be 1..{MAX_USERNAME_LEN} chars"),
        );
    }
    let Some(role) = Role::parse(&payload.role) else {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "bad_role",
            "role must be viewer, operator, or admin",
        );
    };
    if let Err(reason) = password::check_policy(&payload.password, state.config.password_min_length)
    {
        return respond::error(StatusCode::BAD_REQUEST, "weak_password", reason);
    }
    {
        let store = state.store.read().await;
        if store.operator_by_username(&payload.username).is_some() {
            return respond::error(
                StatusCode::CONFLICT,
                "username_taken",
                "An operator with this name already exists",
            );
        }
    }

    let now = unix_now();
    let record = match password::hash(
        &payload.password,
        state.config.password_hash_iterations,
        &state.random,
        now,
    )
    .await
    {
        Ok(record) => record,
        Err(error) => return internal(error.to_string()),
    };
    let id = format!("op-{}", &encode(&random_secret(&state.random))[..16]);
    {
        let mut store = state.store.write().await;
        store.operators.push(crate::panel::store::OperatorRecord {
            id: id.clone(),
            username: payload.username.clone(),
            role,
            password: record,
            must_change_password: payload.must_change_password,
            totp: None,
            disabled: false,
            created_at: now,
            last_login_at: None,
        });
    }
    if let Err(error) = state.persist().await {
        return internal(error.to_string());
    }
    record_audit(
        &state,
        &caller,
        "operator.create",
        &payload.username,
        "ok",
        format!("role={}", role.as_str()),
    )
    .await;
    respond::json(
        StatusCode::CREATED,
        serde_json::json!({"id": id, "username": payload.username, "role": role.as_str()}),
    )
}

/// Routes the by-identifier operator verbs.
pub(crate) async fn by_id(
    request: Request<Incoming>,
    method: Method,
    id: String,
    caller: Caller,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    if let Err(response) = caller.require(Permission::ManageOperators) {
        return response;
    }
    if id.is_empty() || id.contains('/') {
        return respond::error(StatusCode::NOT_FOUND, "not_found", "Route not found");
    }
    match method.as_str() {
        "PATCH" => patch(request, id, caller, state).await,
        "DELETE" => delete(id, caller, state).await,
        _ => respond::error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "Only PATCH and DELETE are served here",
        ),
    }
}

/// Applies a sparse update to one operator account.
async fn patch(
    request: Request<Incoming>,
    id: String,
    caller: Caller,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    let payload = match read_json::<PatchOperatorRequest>(request, &state).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let role = match payload.role.as_deref().map(Role::parse) {
        Some(Some(role)) => Some(role),
        Some(None) => {
            return respond::error(
                StatusCode::BAD_REQUEST,
                "bad_role",
                "role must be viewer, operator, or admin",
            );
        }
        None => None,
    };
    if let Some(new_password) = payload.password.as_deref()
        && let Err(reason) = password::check_policy(new_password, state.config.password_min_length)
    {
        return respond::error(StatusCode::BAD_REQUEST, "weak_password", reason);
    }

    let now = unix_now();
    let hashed = match payload.password.as_deref() {
        Some(new_password) => match password::hash(
            new_password,
            state.config.password_hash_iterations,
            &state.random,
            now,
        )
        .await
        {
            Ok(record) => Some(record),
            Err(error) => return internal(error.to_string()),
        },
        None => None,
    };

    let username;
    let mut changed = Vec::new();
    {
        let mut store = state.store.write().await;
        let Some(existing) = store.operator_by_id(&id).cloned() else {
            return respond::error(StatusCode::NOT_FOUND, "not_found", "Operator not found");
        };
        let losing_admin = existing.role == Role::Admin
            && (matches!(role, Some(new_role) if new_role != Role::Admin)
                || payload.disabled == Some(true));
        if losing_admin && !store.has_other_active_admin(&id) {
            return respond::error(
                StatusCode::CONFLICT,
                "last_admin",
                "The last active administrator cannot be demoted or disabled",
            );
        }
        let Some(operator) = store.operator_by_id_mut(&id) else {
            return respond::error(StatusCode::NOT_FOUND, "not_found", "Operator not found");
        };
        if let Some(role) = role {
            operator.role = role;
            changed.push(format!("role={}", role.as_str()));
        }
        if let Some(disabled) = payload.disabled {
            operator.disabled = disabled;
            changed.push(format!("disabled={disabled}"));
        }
        if let Some(record) = hashed {
            operator.password = record;
            operator.must_change_password = true;
            changed.push("password".to_string());
        }
        if payload.reset_totp == Some(true) {
            operator.totp = None;
            changed.push("totp_reset".to_string());
        }
        username = operator.username.clone();
    }
    if let Err(error) = state.persist().await {
        return internal(error.to_string());
    }
    // Any of these changes invalidates what an existing session was granted on,
    // so the account's sessions go with them.
    if !changed.is_empty() {
        state.sessions.revoke_operator(&id);
    }
    record_audit(
        &state,
        &caller,
        "operator.patch",
        &username,
        "ok",
        changed.join(","),
    )
    .await;
    respond::json(
        StatusCode::OK,
        serde_json::json!({"id": id, "username": username, "changed": changed}),
    )
}

/// Deletes one operator account.
async fn delete(id: String, caller: Caller, state: Arc<PanelState>) -> Response<PanelBody> {
    if id == caller.operator.id {
        return respond::error(
            StatusCode::CONFLICT,
            "self_delete",
            "An operator cannot delete their own account",
        );
    }
    let username;
    {
        let mut store = state.store.write().await;
        let Some(existing) = store.operator_by_id(&id).cloned() else {
            return respond::error(StatusCode::NOT_FOUND, "not_found", "Operator not found");
        };
        if existing.role == Role::Admin && !store.has_other_active_admin(&id) {
            return respond::error(
                StatusCode::CONFLICT,
                "last_admin",
                "The last active administrator cannot be deleted",
            );
        }
        username = existing.username.clone();
        store.operators.retain(|operator| operator.id != id);
    }
    if let Err(error) = state.persist().await {
        return internal(error.to_string());
    }
    state.sessions.revoke_operator(&id);
    record_audit(
        &state,
        &caller,
        "operator.delete",
        &username,
        "ok",
        String::new(),
    )
    .await;
    respond::json(StatusCode::OK, serde_json::json!({"deleted": username}))
}

/// True when the login name is one the panel accepts.
fn is_valid_username(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_USERNAME_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// Appends one audit record for an operator action.
async fn record_audit(
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

    #[test]
    fn login_names_are_restricted_to_a_safe_alphabet() {
        assert!(is_valid_username("root"));
        assert!(is_valid_username("ops.team-1_a"));
        assert!(!is_valid_username(""));
        assert!(!is_valid_username("with space"));
        assert!(!is_valid_username("with/slash"));
        assert!(!is_valid_username(&"a".repeat(MAX_USERNAME_LEN + 1)));
    }
}
