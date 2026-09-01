//! The panel's HTTP surface.
//!
//! One router serves three things on one listener: the embedded application
//! shell, the panel's own JSON API under `/panel/api`, and the signed inbound
//! cluster endpoint under `/cluster/v1`. They share a listener because they
//! share a TLS certificate and a front-proxy configuration; they do not share
//! an authentication model, and the router keeps those apart explicitly.
//!
//! Submodules:
//! - `respond`: response envelope and the hardening headers
//! - `request`: body reading, cookies, client address, and origin checks
//! - `assets`: the embedded single-page application
//! - `auth_routes`: login, logout, and session introspection
//! - `account_routes`: the caller's own password, second factor, and sessions
//! - `operator_routes`: operator account administration
//! - `node_routes`: node linking and probing
//! - `link_token_routes`: the local node's own link token
//! - `control_routes`: Control API relay and the fleet overview
//! - `admin_routes`: audit log and panel settings

use std::net::SocketAddr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use tracing::debug;

pub(crate) mod account_routes;
pub(crate) mod admin_routes;
pub(crate) mod assets;
pub(crate) mod auth_routes;
pub(crate) mod control_routes;
pub(crate) mod link_token_routes;
pub(crate) mod node_routes;
pub(crate) mod operator_routes;
pub(crate) mod request;
pub(crate) mod respond;

use crate::panel::cluster::inbound;
use crate::panel::rbac::Permission;
use crate::panel::session::{self, Lookup, Session};
use crate::panel::state::{PanelState, unix_now};
use crate::panel::store::OperatorRecord;

use request::{HEADER_CLIENT, HEADER_CSRF, SESSION_COOKIE};
use respond::PanelBody;

/// Prefix of the panel's own JSON API.
const API_PREFIX: &str = "/panel/api";

/// The same prefix with its separator, for the sub-path test.
const API_PREFIX_SLASH: &str = "/panel/api/";

/// An authenticated panel request.
pub(crate) struct Caller {
    /// Session cookie value, needed to revoke exactly this session.
    pub(crate) token: String,
    /// Session record.
    pub(crate) session: Session,
    /// Operator the session belongs to.
    pub(crate) operator: OperatorRecord,
    /// Address the request arrived from.
    pub(crate) address: Option<std::net::IpAddr>,
}

impl Caller {
    /// Rejects the request when the caller's role lacks the permission.
    pub(crate) fn require(&self, permission: Permission) -> Result<(), Response<PanelBody>> {
        if self.operator.role.allows(permission) {
            return Ok(());
        }
        Err(respond::error(
            StatusCode::FORBIDDEN,
            "forbidden",
            format!(
                "role '{}' does not carry '{}'",
                self.operator.role.as_str(),
                permission.as_str()
            ),
        ))
    }

    /// Renders the caller's address for an audit record.
    pub(crate) fn address_string(&self) -> String {
        self.address
            .map(|address| address.to_string())
            .unwrap_or_default()
    }
}

/// Serves one panel request.
pub(crate) async fn handle(
    request: Request<Incoming>,
    peer: SocketAddr,
    state: Arc<PanelState>,
    tls: bool,
) -> Response<PanelBody> {
    let response = route(request, peer, state).await;
    if tls {
        respond::with_hsts(response)
    } else {
        response
    }
}

/// Dispatches one request to the surface that owns it.
async fn route(
    request: Request<Incoming>,
    peer: SocketAddr,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    let (path, query) = request::split_target(&request);
    let normalized = normalize(&path);

    if normalized == "/healthz" {
        return respond::build(
            StatusCode::OK,
            "text/plain; charset=utf-8",
            b"ok\n".to_vec(),
        );
    }

    let address = request::client_ip(peer, request.headers(), &state.config.trusted_proxies);

    if normalized == "/cluster/v1" || normalized.starts_with("/cluster/v1/") {
        return inbound::handle(request, address, state).await;
    }

    let Some(address) = address else {
        // A trusted proxy that forwards an unparsable address is misconfigured;
        // answering it would account the request to the wrong client.
        return respond::error(
            StatusCode::BAD_REQUEST,
            "bad_forwarded_for",
            "Forwarded client address could not be parsed",
        );
    };
    if !state.address_allowed(address) {
        return respond::error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Source address is not allowed",
        );
    }

    if normalized == API_PREFIX || normalized.starts_with(API_PREFIX_SLASH) {
        return api(request, normalized, query, address, state).await;
    }

    if request.method() != Method::GET && request.method() != Method::HEAD {
        return respond::error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "Only GET is served here",
        );
    }
    assets::serve(&normalized)
}

/// Serves one `/panel/api` request.
async fn api(
    request: Request<Incoming>,
    path: String,
    query: Option<String>,
    address: std::net::IpAddr,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    if request::header(request.headers(), HEADER_CLIENT).is_none() {
        // Nothing but the panel's own client sets this, and a cross-origin
        // caller cannot add it without a preflight the panel never answers.
        return respond::error(
            StatusCode::BAD_REQUEST,
            "missing_client_header",
            "Requests must carry the panel client header",
        );
    }
    if !request::origin_is_same(request.headers()) {
        return respond::error(
            StatusCode::FORBIDDEN,
            "cross_origin",
            "Cross-origin requests are refused",
        );
    }

    let method = request.method().clone();
    let route = path
        .strip_prefix(API_PREFIX)
        .map(|rest| if rest.is_empty() { "/" } else { rest })
        .unwrap_or("/")
        .to_string();

    // The login route is the one place a request arrives without a session, so
    // it is dispatched before authentication rather than inside it.
    if route == "/session" && method == Method::POST {
        return auth_routes::login(request, address, state).await;
    }

    let now = unix_now();
    let token = request::cookie(request.headers(), SESSION_COOKIE).map(str::to_string);
    let caller = match resolve_caller(&state, token, now, address).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };

    if request::is_mutating(&method) {
        let submitted = request::header(request.headers(), HEADER_CSRF).unwrap_or_default();
        if !session::csrf_matches(&caller.session, submitted) {
            return respond::error(
                StatusCode::FORBIDDEN,
                "bad_csrf",
                "Missing or invalid CSRF token",
            );
        }
    }

    // A forced password change blocks everything except reading the session and
    // setting a new password: an operator holding a credential the panel has
    // already decided is provisional must not be able to act with it.
    if caller.operator.must_change_password
        && !matches!(
            (method.as_str(), route.as_str()),
            ("GET", "/session") | ("DELETE", "/session") | ("POST", "/account/password")
        )
    {
        return respond::error(
            StatusCode::FORBIDDEN,
            "password_change_required",
            "The current password must be changed before anything else",
        );
    }
    if state.config.require_totp
        && !caller.operator.has_totp()
        && !matches!(
            (method.as_str(), route.as_str()),
            ("GET", "/session")
                | ("DELETE", "/session")
                | ("GET", "/account/totp")
                | ("POST", "/account/totp")
                | ("PUT", "/account/totp")
        )
    {
        return respond::error(
            StatusCode::FORBIDDEN,
            "totp_enrolment_required",
            "A second factor must be enrolled before anything else",
        );
    }

    dispatch(request, method, route, query, caller, state).await
}

/// Routes one authenticated request to its handler.
async fn dispatch(
    request: Request<Incoming>,
    method: Method,
    route: String,
    query: Option<String>,
    caller: Caller,
    state: Arc<PanelState>,
) -> Response<PanelBody> {
    if let Some(rest) = route.strip_prefix("/control") {
        return control_routes::relay(request, method, rest.to_string(), query, caller, state)
            .await;
    }
    match (method.as_str(), route.as_str()) {
        ("GET", "/session") => auth_routes::current(&caller, &state).await,
        ("DELETE", "/session") => auth_routes::logout(&caller, &state).await,
        ("GET", "/bootstrap") => auth_routes::bootstrap(&caller, &state).await,

        ("POST", "/account/password") => {
            account_routes::change_password(request, caller, state).await
        }
        ("GET", "/account/totp") => account_routes::totp_state(&caller, &state).await,
        ("POST", "/account/totp") => account_routes::totp_begin(caller, state).await,
        ("PUT", "/account/totp") => account_routes::totp_confirm(request, caller, state).await,
        ("DELETE", "/account/totp") => account_routes::totp_disable(request, caller, state).await,
        ("GET", "/account/sessions") => account_routes::list_sessions(&caller, &state).await,
        ("DELETE", "/account/sessions") => {
            account_routes::revoke_other_sessions(&caller, &state).await
        }

        ("GET", "/operators") => operator_routes::list(&caller, &state).await,
        ("POST", "/operators") => operator_routes::create(request, caller, state).await,

        ("GET", "/nodes") => node_routes::list(&caller, &state).await,
        ("POST", "/nodes") => node_routes::link(request, caller, state).await,
        ("GET", "/nodes/link-token") => link_token_routes::link_token(&caller, &state).await,
        ("POST", "/nodes/link-token/rotate") => {
            link_token_routes::rotate_link_key(caller, state).await
        }

        ("GET", "/overview") => control_routes::overview(&caller, &state).await,

        ("GET", "/audit") => admin_routes::audit_tail(&caller, &state, query.as_deref()).await,
        ("GET", "/audit/verify") => admin_routes::audit_verify(&caller, &state).await,
        ("GET", "/settings") => admin_routes::settings(&caller, &state).await,
        ("PATCH", "/settings") => admin_routes::update_settings(request, caller, state).await,

        _ => {
            if let Some(id) = route.strip_prefix("/operators/") {
                return operator_routes::by_id(request, method, id.to_string(), caller, state)
                    .await;
            }
            if let Some(rest) = route.strip_prefix("/nodes/") {
                return node_routes::by_id(request, method, rest.to_string(), caller, state).await;
            }
            debug!(route = %route, method = %method, "Panel route not found");
            respond::error(StatusCode::NOT_FOUND, "not_found", "Route not found")
        }
    }
}

/// Resolves the session cookie into an authenticated caller.
async fn resolve_caller(
    state: &Arc<PanelState>,
    token: Option<String>,
    now: u64,
    address: std::net::IpAddr,
) -> Result<Caller, Response<PanelBody>> {
    let Some(token) = token else {
        return Err(unauthorized("No session"));
    };
    let session = match state.sessions.touch(&token, now) {
        Lookup::Live(session) => session,
        Lookup::Unknown => return Err(unauthorized("Session is not valid")),
    };
    let store = state.store.read().await;
    let Some(operator) = store.operator_by_id(&session.operator_id).cloned() else {
        drop(store);
        state.sessions.revoke(&token);
        return Err(unauthorized("Session owner no longer exists"));
    };
    if operator.disabled {
        drop(store);
        state.sessions.revoke_operator(&operator.id);
        return Err(unauthorized("Account is disabled"));
    }
    Ok(Caller {
        token,
        session,
        operator,
        address: Some(address),
    })
}

/// Builds the standard unauthenticated answer.
fn unauthorized(message: &str) -> Response<PanelBody> {
    respond::error(StatusCode::UNAUTHORIZED, "unauthorized", message)
}

/// Strips a trailing slash from a path longer than the root.
fn normalize(path: &str) -> String {
    if path.len() > 1 {
        path.trim_end_matches('/').to_string()
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_api_prefix_constants_stay_in_step() {
        assert_eq!(API_PREFIX_SLASH, format!("{API_PREFIX}/"));
    }

    #[test]
    fn trailing_slashes_are_normalised_away() {
        assert_eq!(normalize("/panel/api/nodes/"), "/panel/api/nodes");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("/nodes"), "/nodes");
    }
}
