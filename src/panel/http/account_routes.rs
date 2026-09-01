//! The caller's own credentials, second factor, and sessions.

use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::{Deserialize, Serialize};

use crate::crypto::sha256;
use crate::panel::audit::AuditEntry;
use crate::panel::crypto::random_password;
use crate::panel::password::{self, Verification};
use crate::panel::state::{PanelState, unix_now};
use crate::panel::totp;

use super::respond::{self, PanelBody};
use super::{Caller, request};

/// Recovery codes minted when a second factor is confirmed.
const RECOVERY_CODE_COUNT: usize = 10;

/// Characters per recovery code half.
const RECOVERY_CODE_HALF: usize = 5;

/// Password change request.
#[derive(Deserialize)]
struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

/// Second-factor confirmation request.
#[derive(Deserialize)]
struct TotpConfirmRequest {
    code: String,
}

/// Second-factor removal request.
#[derive(Deserialize)]
struct TotpDisableRequest {
    password: String,
}

/// Second-factor enrolment state.
#[derive(Serialize)]
struct TotpStateView {
    enrolled: bool,
    confirmed: bool,
    recovery_remaining: usize,
    required: bool,
}

/// One live session as the UI lists it.
#[derive(Serialize)]
struct SessionSummary {
    created_at: u64,
    last_seen: u64,
    address: Option<String>,
    user_agent: String,
    current: bool,
}

/// Changes the caller's own password.
pub(crate) async fn change_password(
    request: Request<Incoming>,
    caller: Caller,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    let payload = match read_json::<ChangePasswordRequest>(request, &state).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    if let Err(reason) =
        password::check_policy(&payload.new_password, state.config.password_min_length)
    {
        return respond::error(StatusCode::BAD_REQUEST, "weak_password", reason);
    }
    let verification = match password::verify(
        &caller.operator.password,
        &payload.current_password,
        state.config.password_hash_iterations,
    )
    .await
    {
        Ok(verification) => verification,
        Err(error) => return internal(error.to_string()),
    };
    if verification == Verification::Mismatch {
        audit(
            &state,
            &caller,
            "account.password.change",
            "bad_password",
            String::new(),
        )
        .await;
        return respond::error(
            StatusCode::FORBIDDEN,
            "bad_password",
            "Current password is not valid",
        );
    }
    if payload.new_password == payload.current_password {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "password_unchanged",
            "The new password must differ from the current one",
        );
    }

    let now = unix_now();
    let record = match password::hash(
        &payload.new_password,
        state.config.password_hash_iterations,
        &state.random,
        now,
    )
    .await
    {
        Ok(record) => record,
        Err(error) => return internal(error.to_string()),
    };
    {
        let mut store = state.store.write().await;
        let Some(operator) = store.operator_by_id_mut(&caller.operator.id) else {
            return internal("account no longer exists".to_string());
        };
        operator.password = record;
        operator.must_change_password = false;
    }
    if let Err(error) = state.persist().await {
        return internal(error.to_string());
    }
    // Every other session was opened with the old password. Keeping them alive
    // would mean a stolen session survives the response to the theft.
    let revoked = revoke_other_sessions_of(&state, &caller);
    audit(
        &state,
        &caller,
        "account.password.change",
        "ok",
        format!("revoked_sessions={revoked}"),
    )
    .await;
    respond::json(
        StatusCode::OK,
        serde_json::json!({"changed": true, "revoked_sessions": revoked}),
    )
}

/// Reports the caller's second-factor state.
pub(crate) async fn totp_state(caller: &Caller, state: &Arc<PanelState>) -> Response<PanelBody> {
    let record = caller.operator.totp.as_ref();
    respond::json(
        StatusCode::OK,
        TotpStateView {
            enrolled: record.is_some(),
            confirmed: record.is_some_and(|totp| totp.confirmed),
            recovery_remaining: record.map(|totp| totp.recovery_hashes.len()).unwrap_or(0),
            required: state.config.require_totp,
        },
    )
}

/// Starts second-factor enrolment and returns the secret to scan.
pub(crate) async fn totp_begin(caller: Caller, state: Arc<PanelState>) -> Response<PanelBody> {
    if caller.operator.has_totp() {
        return respond::error(
            StatusCode::CONFLICT,
            "totp_already_enrolled",
            "Disable the current second factor before enrolling another",
        );
    }
    let secret = totp::generate_secret(&state.random);
    let node_name = {
        let store = state.store.read().await;
        store.node.name.clone()
    };
    let uri = totp::provisioning_uri(
        &secret,
        &caller.operator.username,
        &format!("telemt {node_name}"),
    );
    {
        let mut store = state.store.write().await;
        let Some(operator) = store.operator_by_id_mut(&caller.operator.id) else {
            return internal("account no longer exists".to_string());
        };
        operator.totp = Some(crate::panel::store::TotpRecord {
            secret: secret.clone(),
            confirmed: false,
            recovery_hashes: Vec::new(),
        });
    }
    if let Err(error) = state.persist().await {
        return internal(error.to_string());
    }
    audit(&state, &caller, "account.totp.begin", "ok", String::new()).await;
    respond::json(
        StatusCode::OK,
        serde_json::json!({"secret": secret, "uri": uri, "digits": totp::DIGITS, "period": totp::STEP_SECS}),
    )
}

/// Confirms second-factor enrolment and mints recovery codes.
pub(crate) async fn totp_confirm(
    request: Request<Incoming>,
    caller: Caller,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    let payload = match read_json::<TotpConfirmRequest>(request, &state).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let Some(record) = caller.operator.totp.as_ref() else {
        return respond::error(
            StatusCode::CONFLICT,
            "totp_not_started",
            "Start enrolment before confirming it",
        );
    };
    if !totp::verify(&record.secret, &payload.code, unix_now()) {
        audit(
            &state,
            &caller,
            "account.totp.confirm",
            "bad_code",
            String::new(),
        )
        .await;
        return respond::error(StatusCode::BAD_REQUEST, "bad_totp", "Code is not valid");
    }

    let codes: Vec<String> = (0..RECOVERY_CODE_COUNT)
        .map(|_| {
            format!(
                "{}-{}",
                random_password(&state.random, RECOVERY_CODE_HALF).to_lowercase(),
                random_password(&state.random, RECOVERY_CODE_HALF).to_lowercase()
            )
        })
        .collect();
    let hashes: Vec<String> = codes
        .iter()
        .map(|code| hex::encode(sha256(code.as_bytes())))
        .collect();
    {
        let mut store = state.store.write().await;
        let Some(operator) = store.operator_by_id_mut(&caller.operator.id) else {
            return internal("account no longer exists".to_string());
        };
        let Some(totp) = operator.totp.as_mut() else {
            return internal("enrolment disappeared".to_string());
        };
        totp.confirmed = true;
        totp.recovery_hashes = hashes;
    }
    if let Err(error) = state.persist().await {
        return internal(error.to_string());
    }
    audit(&state, &caller, "account.totp.confirm", "ok", String::new()).await;
    // The plaintext codes exist only in this response; only their hashes are
    // stored, so a lost list cannot be recovered and has to be regenerated.
    respond::json(StatusCode::OK, serde_json::json!({"recovery_codes": codes}))
}

/// Removes the caller's second factor.
pub(crate) async fn totp_disable(
    request: Request<Incoming>,
    caller: Caller,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    if state.config.require_totp {
        return respond::error(
            StatusCode::FORBIDDEN,
            "totp_required",
            "panel.require_totp is set, so a second factor cannot be removed",
        );
    }
    let payload = match read_json::<TotpDisableRequest>(request, &state).await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let verification = match password::verify(
        &caller.operator.password,
        &payload.password,
        state.config.password_hash_iterations,
    )
    .await
    {
        Ok(verification) => verification,
        Err(error) => return internal(error.to_string()),
    };
    if verification == Verification::Mismatch {
        audit(
            &state,
            &caller,
            "account.totp.disable",
            "bad_password",
            String::new(),
        )
        .await;
        return respond::error(
            StatusCode::FORBIDDEN,
            "bad_password",
            "Password is not valid",
        );
    }
    {
        let mut store = state.store.write().await;
        let Some(operator) = store.operator_by_id_mut(&caller.operator.id) else {
            return internal("account no longer exists".to_string());
        };
        operator.totp = None;
    }
    if let Err(error) = state.persist().await {
        return internal(error.to_string());
    }
    audit(&state, &caller, "account.totp.disable", "ok", String::new()).await;
    respond::json(StatusCode::OK, serde_json::json!({"enrolled": false}))
}

/// Lists the caller's live sessions.
pub(crate) async fn list_sessions(caller: &Caller, state: &Arc<PanelState>) -> Response<PanelBody> {
    let now = unix_now();
    let sessions: Vec<SessionSummary> = state
        .sessions
        .list_for_operator(&caller.operator.id, now)
        .into_iter()
        .map(|session| SessionSummary {
            current: session.created_at == caller.session.created_at
                && session.csrf_token == caller.session.csrf_token,
            created_at: session.created_at,
            last_seen: session.last_seen,
            address: session.address.map(|address| address.to_string()),
            user_agent: session.user_agent,
        })
        .collect();
    respond::json(StatusCode::OK, serde_json::json!({"sessions": sessions}))
}

/// Revokes every session of the caller except the current one.
pub(crate) async fn revoke_other_sessions(
    caller: &Caller,
    state: &Arc<PanelState>,
) -> Response<PanelBody> {
    let revoked = revoke_other_sessions_of(state, caller);
    audit(
        state,
        caller,
        "account.sessions.revoke",
        "ok",
        format!("revoked={revoked}"),
    )
    .await;
    respond::json(StatusCode::OK, serde_json::json!({"revoked": revoked}))
}

/// Drops every session of the caller's operator but the current one.
fn revoke_other_sessions_of(state: &Arc<PanelState>, caller: &Caller) -> usize {
    let dropped = state.sessions.revoke_operator(&caller.operator.id);
    let now = unix_now();
    // The caller's own session is reissued under the same cookie value, so the
    // browser keeps working while every other session is gone.
    state
        .sessions
        .reinstate(&caller.token, caller.session.clone(), now);
    dropped.saturating_sub(1)
}

/// Reads and decodes a JSON request body.
pub(crate) async fn read_json<T: serde::de::DeserializeOwned>(
    request: Request<Incoming>,
    state: &Arc<PanelState>,
) -> Result<T, Response<PanelBody>> {
    let body = match request::read_body(request.into_body(), state.config.request_body_limit_bytes)
        .await
    {
        Ok(body) => body,
        Err(request::BodyError::TooLarge) => {
            return Err(respond::error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "Request body is too large",
            ));
        }
        Err(request::BodyError::Transport) => {
            return Err(respond::error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "Body could not be read",
            ));
        }
    };
    serde_json::from_slice(&body)
        .map_err(|_| respond::error(StatusCode::BAD_REQUEST, "bad_request", "Invalid JSON body"))
}

/// Builds the standard internal-error answer.
pub(crate) fn internal(detail: String) -> Response<PanelBody> {
    respond::error(StatusCode::INTERNAL_SERVER_ERROR, "internal", detail)
}

/// Appends one audit record for an account action.
pub(crate) async fn audit(
    state: &Arc<PanelState>,
    caller: &Caller,
    action: &str,
    result: &str,
    detail: String,
) {
    state
        .record(AuditEntry {
            actor: caller.operator.username.clone(),
            actor_id: caller.operator.id.clone(),
            action: action.to_string(),
            target: caller.operator.username.clone(),
            result: result.to_string(),
            address: caller.address_string(),
            detail,
            ..AuditEntry::default()
        })
        .await;
}
