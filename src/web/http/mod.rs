//! HTTP surface of the WEB relay.
//!
//! Submodules:
//! - `headers`: header and body parsing rules
//! - `api`: session creation, uplink, and downlink endpoints
//! - `ws`: WebSocket carrier endpoints
//!
//! Every request that does not authenticate receives exactly the response an
//! unknown static path receives, so a prober cannot separate the relay from
//! the operator's public site.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::HeaderValue;
use hyper::{HeaderMap, Method, Request, Response, StatusCode, Uri};
use ipnetwork::IpNetwork;

use crate::config::{WebLimits, WebTimeouts};
use crate::crypto::SecureRandom;
use crate::web::bridge::{self, PERMISSIONS_POLICY};
use crate::web::capability::{CAPABILITY_TEXT_LEN, TOKEN_BYTES};
use crate::web::manager::Manager;
use crate::web::site::{SITE_CSP, StaticEntry, StaticSite, cache_control};
use crate::web::upstream::UpstreamProxy;

pub(crate) mod api;
pub(crate) mod headers;
pub(crate) mod ws;

use headers::{client_ip, header, host_matches};

/// Response body used across the relay surface.
pub(crate) type WebBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

/// The four exact paths reserved for the carrier.
pub(crate) const TRANSPORT_PATHS: [&str; 4] = [
    "/api/v1/session",
    "/api/v1/up",
    "/api/v1/down",
    "/api/v1/ws",
];

/// Wraps a complete buffer as a relay response body.
pub(crate) fn full(body: Bytes) -> WebBody {
    Full::new(body).map_err(|never| match never {}).boxed()
}

/// Shared state of one running relay.
pub(crate) struct Relay {
    pub(crate) hostname: String,
    pub(crate) manager: Arc<Manager>,
    pub(crate) limits: WebLimits,
    pub(crate) timeouts: WebTimeouts,
    pub(crate) site: Option<StaticSite>,
    pub(crate) upstream: Option<UpstreamProxy>,
    pub(crate) trusted_proxies: Vec<IpNetwork>,
    pub(crate) rng: Arc<SecureRandom>,
}

impl Relay {
    /// Routes one request arriving from the public front proxy.
    pub(crate) async fn handle(
        self: &Arc<Self>,
        request: Request<Incoming>,
        peer: SocketAddr,
    ) -> Response<WebBody> {
        if !host_matches(request.headers(), &self.hostname) {
            return self.serve_wrong_host(&request);
        }
        if TRANSPORT_PATHS.contains(&request.uri().path()) {
            return self.serve_api(request, peer).await;
        }
        let method = request.method().clone();
        if request.uri().path() == "/" && (method == Method::GET || method == Method::HEAD) {
            return self.serve_root(request, peer).await;
        }
        if method != Method::GET && method != Method::HEAD {
            return self.serve_not_found(request, peer).await;
        }
        self.serve_static(request, peer).await
    }

    /// Serves the one-shot bridge page, or the ordinary public index.
    async fn serve_root(
        self: &Arc<Self>,
        request: Request<Incoming>,
        peer: SocketAddr,
    ) -> Response<WebBody> {
        // The capability lookup runs for every root request, including those
        // without a bridge query, so the work is identical either way.
        let candidate = bridge_candidate(request.uri().query());
        let profile = self
            .manager
            .match_capability(&candidate.value)
            .filter(|_| candidate.valid && request.method() == Method::GET);
        let Some(profile) = profile else {
            return self.serve_public_root(request, peer).await;
        };
        let Some(ip) = client_ip(peer, request.headers(), &self.trusted_proxies) else {
            return self
                .serve_public_root_without_capability(request, peer)
                .await;
        };
        let Ok(token) = self.manager.issue_bootstrap(&profile, ip) else {
            return self
                .serve_public_root_without_capability(request, peer)
                .await;
        };
        let page = bridge::render(
            &self.hostname,
            &token,
            profile.carrier,
            self.limits.carrier_batch_bytes,
            &self.rng,
        );
        let Some(page) = page else {
            return self
                .serve_public_root_without_capability(request, peer)
                .await;
        };
        let mut response = Response::new(full(Bytes::from(page.body)));
        let response_headers = response.headers_mut();
        insert(response_headers, "content-type", "text/html; charset=utf-8");
        insert(response_headers, "content-security-policy", &page.csp);
        insert(response_headers, "cache-control", "no-store");
        insert(response_headers, "referrer-policy", "no-referrer");
        insert(response_headers, "x-content-type-options", "nosniff");
        insert(response_headers, "x-dns-prefetch-control", "off");
        insert(response_headers, "permissions-policy", PERMISSIONS_POLICY);
        response
    }

    /// Serves a static path, or delegates it to the public application.
    async fn serve_static(
        self: &Arc<Self>,
        request: Request<Incoming>,
        peer: SocketAddr,
    ) -> Response<WebBody> {
        if let Some(upstream) = &self.upstream {
            let ip = client_ip(peer, request.headers(), &self.trusted_proxies);
            return upstream.forward(request, ip).await;
        }
        let Some(site) = self.site.as_ref() else {
            return plain_not_found();
        };
        let resolved = site.resolve(request.uri().path()).cloned();
        match resolved {
            Some(entry) => self.serve_entry(site, &request_head(&request), entry, StatusCode::OK),
            None => self.serve_not_found(request, peer).await,
        }
    }

    /// The response shape used for every unauthenticated site request.
    pub(crate) async fn serve_not_found(
        self: &Arc<Self>,
        request: Request<Incoming>,
        peer: SocketAddr,
    ) -> Response<WebBody> {
        if let Some(upstream) = &self.upstream {
            let ip = client_ip(peer, request.headers(), &self.trusted_proxies);
            return upstream.forward(request, ip).await;
        }
        let Some(site) = self.site.as_ref() else {
            return plain_not_found();
        };
        let entry = site.not_found().clone();
        self.serve_entry(site, &request_head(&request), entry, StatusCode::NOT_FOUND)
    }

    /// The response shape used for every unauthenticated transport request.
    ///
    /// In application mode the request is handed to the operator's site with
    /// its relay headers and body removed, so a reserved path answers exactly
    /// like any other unknown path of that site.
    pub(crate) async fn transport_not_found(
        self: &Arc<Self>,
        head: &RequestHead,
        peer: SocketAddr,
    ) -> Response<WebBody> {
        if let Some(upstream) = &self.upstream {
            let ip = client_ip(peer, &head.headers, &self.trusted_proxies);
            return upstream
                .forward_sanitized(
                    head.method.clone(),
                    head.uri.clone(),
                    head.headers.clone(),
                    ip,
                )
                .await;
        }
        let Some(site) = self.site.as_ref() else {
            return plain_not_found();
        };
        let entry = site.not_found().clone();
        self.serve_entry(site, head, entry, StatusCode::NOT_FOUND)
    }

    async fn serve_public_root(
        self: &Arc<Self>,
        request: Request<Incoming>,
        peer: SocketAddr,
    ) -> Response<WebBody> {
        if let Some(upstream) = &self.upstream {
            let ip = client_ip(peer, request.headers(), &self.trusted_proxies);
            return upstream.forward(request, ip).await;
        }
        let Some(site) = self.site.as_ref() else {
            return plain_not_found();
        };
        let entry = site.index().clone();
        self.serve_entry(site, &request_head(&request), entry, StatusCode::OK)
    }

    /// Answers a valid capability that could not be served, without leaking it.
    ///
    /// The query is stripped before delegation so the operator's application
    /// never observes a bridge capability.
    async fn serve_public_root_without_capability(
        self: &Arc<Self>,
        request: Request<Incoming>,
        peer: SocketAddr,
    ) -> Response<WebBody> {
        if let Some(upstream) = &self.upstream {
            let ip = client_ip(peer, request.headers(), &self.trusted_proxies);
            let (mut parts, body) = request.into_parts();
            let mut uri_parts = parts.uri.clone().into_parts();
            uri_parts.path_and_query = Some(hyper::http::uri::PathAndQuery::from_static("/"));
            if let Ok(uri) = Uri::from_parts(uri_parts) {
                parts.uri = uri;
            }
            return upstream.forward(Request::from_parts(parts, body), ip).await;
        }
        let Some(site) = self.site.as_ref() else {
            return plain_not_found();
        };
        let entry = site.index().clone();
        self.serve_entry(site, &request_head(&request), entry, StatusCode::OK)
    }

    fn serve_wrong_host(self: &Arc<Self>, request: &Request<Incoming>) -> Response<WebBody> {
        match &self.site {
            Some(site) => {
                let entry = site.not_found().clone();
                self.serve_entry(site, &request_head(request), entry, StatusCode::NOT_FOUND)
            }
            None => plain_not_found(),
        }
    }

    /// Writes one in-memory entry with the site-wide header set.
    fn serve_entry(
        &self,
        site: &StaticSite,
        head: &RequestHead,
        entry: Arc<StaticEntry>,
        status: StatusCode,
    ) -> Response<WebBody> {
        let mut response = Response::new(full(Bytes::new()));
        *response.status_mut() = status;
        {
            let response_headers = response.headers_mut();
            insert(
                response_headers,
                "cache-control",
                cache_control(head.uri.query(), status.as_u16()),
            );
            insert(response_headers, "content-security-policy", SITE_CSP);
            insert(
                response_headers,
                "referrer-policy",
                "strict-origin-when-cross-origin",
            );
            insert(response_headers, "x-content-type-options", "nosniff");
            insert(response_headers, "x-frame-options", "DENY");
            insert(
                response_headers,
                "permissions-policy",
                "camera=(), microphone=(), geolocation=()",
            );
            insert(response_headers, "content-type", entry.content_type);
            insert(response_headers, "last-modified", site.last_modified());
        }
        let conditional = header(&head.headers, "if-modified-since");
        if status == StatusCode::OK && site.not_modified(conditional) {
            *response.status_mut() = StatusCode::NOT_MODIFIED;
            return response;
        }
        if head.method != Method::HEAD {
            *response.body_mut() = full(entry.body.clone());
        }
        response
            .headers_mut()
            .insert("content-length", HeaderValue::from(entry.body.len()));
        response
    }

    /// Body read deadline shared by every body-bearing endpoint.
    pub(crate) fn body_deadline(&self) -> Duration {
        Duration::from_millis(self.timeouts.body_read_ms)
    }
}

/// Request head kept after the body has been detached.
pub(crate) struct RequestHead {
    pub(crate) method: Method,
    pub(crate) uri: Uri,
    pub(crate) headers: HeaderMap<HeaderValue>,
}

/// Copies the head of a request that is still intact.
pub(crate) fn request_head(request: &Request<Incoming>) -> RequestHead {
    RequestHead {
        method: request.method().clone(),
        uri: request.uri().clone(),
        headers: request.headers().clone(),
    }
}

/// Minimal 404 used when no static site is loaded.
pub(crate) fn plain_not_found() -> Response<WebBody> {
    let mut response = Response::new(full(Bytes::from_static(b"404 page not found\n")));
    *response.status_mut() = StatusCode::NOT_FOUND;
    insert(
        response.headers_mut(),
        "content-type",
        "text/plain; charset=utf-8",
    );
    response
}

/// Sets a header, ignoring a value the HTTP layer would reject.
pub(crate) fn insert(headers: &mut HeaderMap<HeaderValue>, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

/// Reads a bounded request body under a deadline.
pub(crate) async fn read_body(body: Incoming, limit: usize, deadline: Duration) -> Option<Bytes> {
    tokio::time::timeout(deadline, async move {
        let mut body = body;
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.ok()?;
            if let Some(chunk) = frame.data_ref() {
                if buffer.len().saturating_add(chunk.len()) > limit {
                    return None;
                }
                buffer.extend_from_slice(chunk);
            }
        }
        Some(Bytes::from(buffer))
    })
    .await
    .ok()
    .flatten()
}

/// True when the request carries no body at all.
pub(crate) async fn empty_body(body: Incoming, deadline: Duration) -> bool {
    read_body(body, 0, deadline).await.is_some()
}

/// A decoded bridge capability candidate.
struct BridgeCandidate {
    value: [u8; TOKEN_BYTES],
    valid: bool,
}

/// Decodes the `bridge` query into a fixed-size candidate.
///
/// A malformed or absent query yields a zeroed candidate that is still looked
/// up, so the lookup cost does not reveal whether a capability was present.
fn bridge_candidate(query: Option<&str>) -> BridgeCandidate {
    let mut candidate = BridgeCandidate {
        value: [0u8; TOKEN_BYTES],
        valid: false,
    };
    let Some(query) = query else {
        return candidate;
    };
    let Some(value) = query.strip_prefix("bridge=") else {
        return candidate;
    };
    if value.len() != CAPABILITY_TEXT_LEN {
        return candidate;
    }
    if let Some(decoded) = crate::web::capability::decode_token(value) {
        candidate.value = decoded;
        candidate.valid = true;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_candidate_requires_the_exact_query_shape() {
        let token = crate::web::capability::encode_token(&[9u8; TOKEN_BYTES]);
        let query = format!("bridge={token}");
        let candidate = bridge_candidate(Some(&query));
        assert!(candidate.valid);
        assert_eq!(candidate.value, [9u8; TOKEN_BYTES]);

        assert!(!bridge_candidate(None).valid);
        assert!(!bridge_candidate(Some("bridge=short")).valid);
        assert!(!bridge_candidate(Some(&format!("x=1&bridge={token}"))).valid);
        assert!(!bridge_candidate(Some(&format!("bridge={token}&x=1"))).valid);
    }
}
