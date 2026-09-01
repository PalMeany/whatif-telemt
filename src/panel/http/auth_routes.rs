//! Login, logout, and session introspection.

use std::net::IpAddr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::crypto::sha256;
use crate::panel::audit::AuditEntry;
use crate::panel::crypto::secure_eq;
use crate::panel::password::{self, Verification};
use crate::panel::ratelimit::Gate;
use crate::panel::state::{PanelState, unix_now};
use crate::panel::store::OperatorRecord;
use crate::panel::totp;

use super::Caller;
use super::request::{self, SESSION_COOKIE};
use super::respond::{self, PanelBody};

/// Actor recorded for a login attempt against a name that is not an account.
const UNKNOWN_ACTOR: &str = "<unknown>";

/// Credentials submitted to the login route.
#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
    #[serde(default)]
    totp: Option<String>,
    #[serde(default)]
    recovery_code: Option<String>,
}

/// What the browser learns about its own session.
#[derive(Serialize)]
struct SessionView {
    operator_id: String,
    username: String,
    role: &'static str,
    csrf_token: String,
    must_change_password: bool,
    totp_enabled: bool,
    totp_required: bool,
    created_at: u64,
    expires_at: u64,
}

/// Panel metadata the application shell reads once at start-up.
#[derive(Serialize)]
struct BootstrapView {
    version: &'static str,
    started_at: u64,
    bundled_ui: bool,
    node: NodeView,
    operator: SessionView,
    default_node_id: Option<String>,
    audit_enabled: bool,
}

/// This node's own identity as the UI shows it.
#[derive(Serialize)]
struct NodeView {
    id: String,
    name: String,
    cluster_enabled: bool,
    role: &'static str,
    is_master: bool,
    is_agent: bool,
    linked_nodes: usize,
}

/// Authenticates an operator and issues a session.
pub(crate) async fn login(
    request: Request<Incoming>,
    address: IpAddr,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    let user_agent = request::header(request.headers(), "user-agent")
        .unwrap_or_default()
        .to_string();
    let body = match request::read_body(request.into_body(), state.config.request_body_limit_bytes)
        .await
    {
        Ok(body) => body,
        Err(_) => {
            return respond::error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "Body could not be read",
            );
        }
    };
    let Ok(payload) = serde_json::from_slice::<LoginRequest>(&body) else {
        return respond::error(StatusCode::BAD_REQUEST, "bad_request", "Invalid JSON body");
    };

    let now = unix_now();
    // Resolved before the throttle gate purely so every audit record below can
    // say whether the submitted name is an account at all. The lookup is an
    // in-memory read; the expensive derivation stays behind the gate.
    let operator = {
        let store = state.store.read().await;
        store.operator_by_username(&payload.username).cloned()
    };
    let known = operator.is_some();

    if let Gate::Locked(remaining) = state.throttle.check(&payload.username, Some(address), now) {
        record_login(&state, &payload.username, known, address, "locked_out").await;
        return respond::error(
            StatusCode::TOO_MANY_REQUESTS,
            "locked_out",
            format!("Too many failed attempts; retry in {remaining} seconds"),
        );
    }

    // An unknown account still pays for a password derivation. Answering it
    // immediately would turn login latency into an account-existence oracle.
    let Some(operator) = operator else {
        let _ = password::hash(
            &payload.password,
            state.config.password_hash_iterations,
            &state.random,
            now,
        )
        .await;
        state
            .throttle
            .record_failure(&payload.username, Some(address), now);
        record_login(&state, &payload.username, known, address, "unknown_account").await;
        return invalid_credentials();
    };

    let verification = match password::verify(
        &operator.password,
        &payload.password,
        state.config.password_hash_iterations,
    )
    .await
    {
        Ok(verification) => verification,
        Err(error) => {
            return respond::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                error.to_string(),
            );
        }
    };
    if verification == Verification::Mismatch || operator.disabled {
        state
            .throttle
            .record_failure(&payload.username, Some(address), now);
        let reason = if operator.disabled {
            "account_disabled"
        } else {
            "bad_password"
        };
        record_login(&state, &payload.username, known, address, reason).await;
        return invalid_credentials();
    }

    if let Some(second_factor) = operator.totp.as_ref().filter(|totp| totp.confirmed) {
        match check_second_factor(second_factor, &payload, now) {
            SecondFactor::Missing => {
                return respond::error(
                    StatusCode::UNAUTHORIZED,
                    "totp_required",
                    "A one-time code is required",
                );
            }
            SecondFactor::Invalid => {
                state
                    .throttle
                    .record_failure(&payload.username, Some(address), now);
                record_login(&state, &payload.username, known, address, "bad_totp").await;
                return respond::error(
                    StatusCode::UNAUTHORIZED,
                    "bad_totp",
                    "One-time code is not valid",
                );
            }
            SecondFactor::Accepted { spent_recovery } => {
                if let Some(spent) = spent_recovery {
                    let mut store = state.store.write().await;
                    if let Some(record) = store.operator_by_id_mut(&operator.id)
                        && let Some(totp) = record.totp.as_mut()
                    {
                        totp.recovery_hashes.retain(|hash| hash != &spent);
                    }
                }
            }
        }
    }

    // Everything the credential proved is now recorded in one write: the login
    // timestamp, and the rehash when the stored work factor is behind.
    {
        let mut store = state.store.write().await;
        if let Some(record) = store.operator_by_id_mut(&operator.id) {
            record.last_login_at = Some(now);
        }
    }
    if verification == Verification::MatchNeedsRehash
        && let Ok(rehashed) = password::hash(
            &payload.password,
            state.config.password_hash_iterations,
            &state.random,
            now,
        )
        .await
    {
        let mut store = state.store.write().await;
        if let Some(record) = store.operator_by_id_mut(&operator.id) {
            record.password = rehashed;
        }
    }
    if let Err(error) = state.persist().await {
        return respond::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            error.to_string(),
        );
    }

    state
        .throttle
        .record_success(&payload.username, Some(address));
    let (token, session) = state
        .sessions
        .create(&operator.id, now, Some(address), &user_agent);
    record_login(&state, &payload.username, known, address, "ok").await;
    info!(
        username = %operator.username,
        role = operator.role.as_str(),
        "Panel login accepted"
    );

    let view = session_view(&operator, &session, &state);
    let response = respond::json(StatusCode::OK, view);
    respond::with_cookie(
        response,
        &session_cookie(&token, state.config.session_ttl_secs),
    )
}

/// Reports the caller's own session.
pub(crate) async fn current(caller: &Caller, state: &Arc<PanelState>) -> Response<PanelBody> {
    respond::json(
        StatusCode::OK,
        session_view(&caller.operator, &caller.session, state),
    )
}

/// Revokes the caller's session and clears the cookie.
pub(crate) async fn logout(caller: &Caller, state: &Arc<PanelState>) -> Response<PanelBody> {
    state.sessions.revoke(&caller.token);
    state
        .record(AuditEntry {
            actor: caller.operator.username.clone(),
            actor_id: caller.operator.id.clone(),
            action: "auth.logout".to_string(),
            result: "ok".to_string(),
            address: caller.address_string(),
            ..AuditEntry::default()
        })
        .await;
    let response = respond::json(StatusCode::OK, serde_json::json!({"revoked": true}));
    respond::with_cookie(response, &cleared_cookie())
}

/// Reports the metadata the application shell needs at start-up.
pub(crate) async fn bootstrap(caller: &Caller, state: &Arc<PanelState>) -> Response<PanelBody> {
    let store = state.store.read().await;
    let view = BootstrapView {
        version: env!("CARGO_PKG_VERSION"),
        started_at: state.started_at,
        bundled_ui: super::assets::is_bundled(),
        node: NodeView {
            id: store.node.id.clone(),
            name: store.node.name.clone(),
            cluster_enabled: state.config.cluster.enabled,
            role: state.cluster_role().as_str(),
            is_master: state.cluster_role().is_master(),
            is_agent: state.cluster_role().is_agent(),
            linked_nodes: store.nodes.len(),
        },
        operator: session_view(&caller.operator, &caller.session, state),
        default_node_id: store.settings.default_node_id.clone(),
        audit_enabled: state.config.audit_enabled,
    };
    respond::json(StatusCode::OK, view)
}

/// Outcome of checking a submitted second factor.
enum SecondFactor {
    /// No code was submitted at all.
    Missing,
    /// A code was submitted and did not match.
    Invalid,
    /// The code matched; a spent recovery hash is returned for removal.
    Accepted { spent_recovery: Option<String> },
}

/// Checks a TOTP code or a single-use recovery code.
fn check_second_factor(
    record: &crate::panel::store::TotpRecord,
    payload: &LoginRequest,
    now: u64,
) -> SecondFactor {
    if let Some(code) = payload.recovery_code.as_deref().filter(|c| !c.is_empty()) {
        let submitted = hex::encode(sha256(code.trim().as_bytes()));
        let matched = record
            .recovery_hashes
            .iter()
            .find(|stored| secure_eq(stored.as_bytes(), submitted.as_bytes()));
        return match matched {
            Some(stored) => SecondFactor::Accepted {
                spent_recovery: Some(stored.clone()),
            },
            None => SecondFactor::Invalid,
        };
    }
    let Some(code) = payload.totp.as_deref().filter(|c| !c.is_empty()) else {
        return SecondFactor::Missing;
    };
    if totp::verify(&record.secret, code, now) {
        SecondFactor::Accepted {
            spent_recovery: None,
        }
    } else {
        SecondFactor::Invalid
    }
}

/// Renders the session view returned to the browser.
fn session_view(
    operator: &OperatorRecord,
    session: &crate::panel::session::Session,
    state: &Arc<PanelState>,
) -> SessionView {
    SessionView {
        operator_id: operator.id.clone(),
        username: operator.username.clone(),
        role: operator.role.as_str(),
        csrf_token: session.csrf_token.clone(),
        must_change_password: operator.must_change_password,
        totp_enabled: operator.has_totp(),
        totp_required: state.config.require_totp,
        created_at: session.created_at,
        expires_at: session.created_at + state.config.session_ttl_secs,
    }
}

/// Renders the `Set-Cookie` value for a new session.
///
/// `Secure` is unconditional. A panel reachable over plaintext off-host is
/// refused by configuration validation, and browsers treat `http://localhost`
/// as a secure context, so the flag costs nothing and closes the case where a
/// front proxy is later reconfigured to serve plaintext.
fn session_cookie(token: &str, ttl_secs: u64) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={ttl_secs}; HttpOnly; Secure; SameSite=Strict"
    )
}

/// Renders the `Set-Cookie` value that clears the session.
fn cleared_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Strict")
}

/// The single answer every credential failure produces.
fn invalid_credentials() -> Response<PanelBody> {
    respond::error(
        StatusCode::UNAUTHORIZED,
        "invalid_credentials",
        "Username or password is not valid",
    )
}

/// Records one login attempt.
///
/// A name that matches no account is **not** written to the log. Operators
/// routinely paste a password into the account field, and an audit log that
/// records whatever was submitted turns every such slip into a stored
/// credential that anyone who can read the log can use. Only a short digest is
/// kept, which is still enough to correlate repeated attempts against the same
/// non-existent name.
async fn record_login(
    state: &Arc<PanelState>,
    username: &str,
    known: bool,
    address: IpAddr,
    result: &str,
) {
    let (actor, detail) = if known {
        (username.to_string(), String::new())
    } else {
        (
            UNKNOWN_ACTOR.to_string(),
            format!("submitted_name_digest={}", name_digest(username)),
        )
    };
    state
        .record(AuditEntry {
            actor,
            action: "auth.login".to_string(),
            result: result.to_string(),
            address: address.to_string(),
            detail,
            ..AuditEntry::default()
        })
        .await;
}

/// Short, non-reversible fingerprint of a submitted account name.
fn name_digest(username: &str) -> String {
    hex::encode(sha256(username.as_bytes()))[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_cookie_is_locked_down() {
        let cookie = session_cookie("abc", 3_600);
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=3600"));
        assert!(cookie.starts_with("telemt_panel_session=abc;"));
    }

    #[test]
    fn clearing_the_cookie_expires_it_immediately() {
        assert!(cleared_cookie().contains("Max-Age=0"));
    }

    #[test]
    fn a_recovery_code_is_matched_by_its_hash_only() {
        let code = "abcd-efgh";
        let record = crate::panel::store::TotpRecord {
            secret: "AAAA".to_string(),
            confirmed: true,
            recovery_hashes: vec![hex::encode(sha256(code.as_bytes()))],
        };
        let payload = LoginRequest {
            username: "root".to_string(),
            password: String::new(),
            totp: None,
            recovery_code: Some(code.to_string()),
        };
        assert!(matches!(
            check_second_factor(&record, &payload, 0),
            SecondFactor::Accepted {
                spent_recovery: Some(_)
            }
        ));
        let wrong = LoginRequest {
            recovery_code: Some("nope".to_string()),
            ..payload
        };
        assert!(matches!(
            check_second_factor(&record, &wrong, 0),
            SecondFactor::Invalid
        ));
    }
}
