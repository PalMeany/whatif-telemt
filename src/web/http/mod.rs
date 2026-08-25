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
use tracing::debug;

/// Response body used across the relay surface.
pub(crate) type WebBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

/// The four exact paths reserved for the carrier.
pub(crate) const TRANSPORT_PATHS: [&str; 4] = [
    "/api/v1/session",
    "/api/v1/up",
    "/api/v1/down",
    "/api/v1/ws",
];

/// Bytes of a refused request body consumed before the connection is closed.
///
/// Matches what Go's `net/http` discards on behalf of a handler that ignored
/// the body, so a refusal here is framed the way the reference frames one.
const REFUSAL_DRAIN_LIMIT: usize = 256 * 1024;

/// Deadline for consuming a refused request body.
///
/// Deliberately not `body_read_ms`: that is the budget for a body an endpoint
/// is going to parse, and stretching a refusal to it would hold a connection
/// for thirty seconds on every unknown path.
const REFUSAL_DRAIN_DEADLINE: Duration = Duration::from_secs(5);

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
        // The request target is classified on its decoded form, exactly as the
        // reference does through `net/url`. Matching the raw target instead
        // would let `/api/v1/%75p` miss the reserved set and fall through to
        // the operator's application with its carrier headers intact.
        let path = decoded_path(request.uri().path());
        let transport = TRANSPORT_PATHS.contains(&path.as_str());
        let matches_host = host_matches(request.headers(), &self.hostname);
        if !matches_host {
            // Without this the symptom is a blanket 404 with no explanation,
            // which is indistinguishable from a broken site configuration.
            debug!(
                peer = %peer,
                received = headers::request_host(request.headers()).unwrap_or("<absent>"),
                expected = %self.hostname,
                "WEB request rejected: Host does not match web.hostname"
            );
        }

        if transport {
            // A mismatched `Host` is a refusal like any other, and it has to
            // reach the sanitising fallback: the plain forwarder strips only
            // hop-by-hop headers, so answering it there would hand the
            // operator's application a live session bearer, the carrier
            // headers, and the opaque MTProto body.
            if !matches_host {
                let head = request_head(&request);
                refusal_drain(request.into_body()).await;
                return self.transport_not_found(&head, peer).await;
            }
            return self.serve_api(request, peer, path).await;
        }

        // Application mode streams the request to the operator's site, body
        // and all, so there is nothing to consume here.
        if self.upstream.is_some() {
            return self
                .serve_public_upstream(request, peer, &path, matches_host)
                .await;
        }
        // Static mode never reads a request body. Dropping an unread `Incoming`
        // makes hyper abort the connection in the middle of the body, and that
        // abort is measurable: it is the same oracle a refused reserved path
        // would create, reached from the other side. Every static-mode answer
        // consumes the body through the one shared helper the reserved paths
        // use, so the two classes are framed identically.
        let head = request_head(&request);
        refusal_drain(request.into_body()).await;
        self.serve_public_static(&head, peer, &path, matches_host)
    }

    /// Serves a non-reserved path from the in-memory static site.
    ///
    /// The body has already been consumed by the caller, so every branch here
    /// is framed the same way.
    fn serve_public_static(
        self: &Arc<Self>,
        head: &RequestHead,
        peer: SocketAddr,
        path: &str,
        matches_host: bool,
    ) -> Response<WebBody> {
        let Some(site) = self.site.as_ref() else {
            return plain_not_found();
        };
        let not_found =
            || self.serve_entry(site, head, site.not_found().clone(), StatusCode::NOT_FOUND);
        if !matches_host {
            return not_found();
        }
        let get_or_head = head.method == Method::GET || head.method == Method::HEAD;
        if path == "/" && get_or_head {
            return match self.root_outcome(head, peer) {
                RootOutcome::Bridge(response) => response,
                // A suppressed capability and an ordinary root request are the
                // same response here, which is what keeps a capability that
                // could not be served from being distinguishable.
                RootOutcome::Suppressed | RootOutcome::Public => {
                    self.serve_entry(site, head, site.index().clone(), StatusCode::OK)
                }
            };
        }
        if !get_or_head {
            return not_found();
        }
        // `resolve` percent-decodes internally, so it is given the raw target
        // rather than the already-decoded classification path.
        match site.resolve(head.uri.path()).cloned() {
            Some(entry) => self.serve_entry(site, head, entry, StatusCode::OK),
            None => not_found(),
        }
    }

    /// Serves a non-reserved path by delegating to the operator's application.
    async fn serve_public_upstream(
        self: &Arc<Self>,
        request: Request<Incoming>,
        peer: SocketAddr,
        path: &str,
        matches_host: bool,
    ) -> Response<WebBody> {
        let Some(upstream) = self.upstream.as_ref() else {
            return plain_not_found();
        };
        let ip = client_ip(peer, request.headers(), &self.trusted_proxies);
        if matches_host && path == "/" && request.method() == Method::GET {
            let head = request_head(&request);
            match self.root_outcome(&head, peer) {
                RootOutcome::Bridge(response) => {
                    // The relay answers this one itself, so the body it is not
                    // going to read still has to be consumed.
                    refusal_drain(request.into_body()).await;
                    return response;
                }
                RootOutcome::Suppressed => {
                    // The query is stripped before delegation so the operator's
                    // application never observes a bridge capability.
                    let (mut parts, body) = request.into_parts();
                    let mut uri_parts = parts.uri.clone().into_parts();
                    uri_parts.path_and_query =
                        Some(hyper::http::uri::PathAndQuery::from_static("/"));
                    if let Ok(uri) = Uri::from_parts(uri_parts) {
                        parts.uri = uri;
                    }
                    return upstream.forward(Request::from_parts(parts, body), ip).await;
                }
                RootOutcome::Public => {}
            }
        }
        upstream.forward(request, ip).await
    }

    /// Decides what a root request with a `bridge` query is entitled to.
    fn root_outcome(self: &Arc<Self>, head: &RequestHead, peer: SocketAddr) -> RootOutcome {
        // The lookup runs for every root request, including those with no bridge
        // query at all, so a malformed query is not separable from an absent
        // one by whether the relay looked. It is a randomly keyed hash probe,
        // not the reference's constant-time scan over every profile: the bucket
        // a candidate lands in is not steerable by a remote peer, a match still
        // needs all 256 bits, and hit-versus-miss is already fully visible in
        // the response body. A full scan would make every unauthenticated
        // `GET /` cost one HMAC comparison per configured profile.
        let candidate = bridge_candidate(head.uri.query());
        let profile = self
            .manager
            .match_capability(&candidate.value)
            .filter(|_| candidate.valid && head.method == Method::GET);
        let Some(profile) = profile else {
            return RootOutcome::Public;
        };
        let Some(ip) = client_ip(peer, &head.headers, &self.trusted_proxies) else {
            return RootOutcome::Suppressed;
        };
        let Ok(token) = self.manager.issue_bootstrap(&profile, ip) else {
            return RootOutcome::Suppressed;
        };
        let page = bridge::render(
            &self.hostname,
            &token,
            profile.carrier,
            self.limits.carrier_batch_bytes,
            &self.rng,
        );
        let Some(page) = page else {
            return RootOutcome::Suppressed;
        };
        self.manager.count_bridge_page();
        let mut response = Response::new(full(Bytes::from(page.body)));
        let response_headers = response.headers_mut();
        insert(response_headers, "content-type", "text/html; charset=utf-8");
        insert(response_headers, "content-security-policy", &page.csp);
        insert(response_headers, "cache-control", "no-store");
        insert(response_headers, "referrer-policy", "no-referrer");
        insert(response_headers, "x-content-type-options", "nosniff");
        insert(response_headers, "x-dns-prefetch-control", "off");
        insert(response_headers, "permissions-policy", PERMISSIONS_POLICY);
        RootOutcome::Bridge(response)
    }

    /// Answers a refused transport request whose body is still unread.
    ///
    /// Refused reserved paths and refused ordinary paths must consume a body
    /// the same way, or `POST /api/v1/up` and `POST /api/v1/upx` are separable
    /// by connection framing and latency alone, with no credential at all.
    /// Both classes therefore go through [`refusal_drain`].
    pub(crate) async fn transport_not_found_draining(
        self: &Arc<Self>,
        head: &RequestHead,
        body: Incoming,
        peer: SocketAddr,
    ) -> Response<WebBody> {
        refusal_drain(body).await;
        self.transport_not_found(head, peer).await
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

    /// Wall-clock ceiling for handling one request end to end.
    ///
    /// A parked long poll is the slowest legitimate request, and the reverse
    /// proxy adds the application's own response time on top, so the deadline
    /// is derived from both rather than fixed. Without it, a slow body, a
    /// wedged application, or a client that stops reading holds a connection
    /// and its accept-loop permit indefinitely, and the front-proxy templates
    /// bound neither hop.
    pub(crate) fn request_deadline(&self) -> Duration {
        let poll = self.timeouts.long_poll_ms;
        let body = self.timeouts.body_read_ms;
        Duration::from_millis(poll.saturating_add(body).saturating_add(poll / 2))
    }
}

/// The answer for a request that overran the relay's own deadline.
///
/// It is the retryable answer rather than the site's 404: the client did
/// nothing wrong, and every carrier already treats 503 as "come back".
pub(crate) fn request_timeout_response() -> Response<WebBody> {
    let mut response = Response::new(full(Bytes::new()));
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    let response_headers = response.headers_mut();
    response_headers.insert("content-length", HeaderValue::from_static("0"));
    insert(response_headers, "cache-control", "no-store");
    insert(response_headers, "retry-after", "1");
    response
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

/// Minimal 404 used when neither a static site nor an application is loaded.
///
/// Configuration validation requires exactly one of `web.public_dir` and
/// `web.public_upstream`, so this is only ever reached defensively. It carries
/// no body and no media type on purpose: any fixed banner here — Go's
/// `404 page not found` most of all — is a fingerprint that identifies the
/// origin from a single unauthenticated request.
pub(crate) fn plain_not_found() -> Response<WebBody> {
    let mut response = Response::new(full(Bytes::new()));
    *response.status_mut() = StatusCode::NOT_FOUND;
    let response_headers = response.headers_mut();
    response_headers.insert("content-length", HeaderValue::from_static("0"));
    insert(response_headers, "cache-control", "no-store");
    response
}

/// Sets a header, ignoring a value the HTTP layer would reject.
pub(crate) fn insert(headers: &mut HeaderMap<HeaderValue>, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

/// Reads a bounded request body under a deadline.
///
/// A body that overruns `limit` is still read to its end before the refusal is
/// returned. Stopping early would make hyper abort the connection, and that
/// abort is visible to the client: it is the same oracle that draining a
/// refused body removes, just reached with a larger body.
pub(crate) async fn read_body(body: Incoming, limit: usize, deadline: Duration) -> Option<Bytes> {
    tokio::time::timeout(deadline, async move {
        let mut body = body;
        let mut buffer: Vec<u8> = Vec::new();
        let mut overrun = false;
        while let Some(frame) = body.frame().await {
            let Ok(frame) = frame else {
                return None;
            };
            if let Some(chunk) = frame.data_ref() {
                if overrun || buffer.len().saturating_add(chunk.len()) > limit {
                    overrun = true;
                    buffer = Vec::new();
                    continue;
                }
                buffer.extend_from_slice(chunk);
            }
        }
        (!overrun).then(|| Bytes::from(buffer))
    })
    .await
    .ok()
    .flatten()
}

/// True when the request carries no body at all.
pub(crate) async fn empty_body(body: Incoming, deadline: Duration) -> bool {
    read_body(body, 0, deadline).await.is_some()
}

/// Reads and discards the body of a request that is answered without it.
///
/// Every refusal, on a reserved path and on an ordinary path alike, consumes
/// its body here. That is the whole point of the helper: a dropped `Incoming`
/// makes hyper abort the connection mid-body, so a class of path that drops
/// and a class that drains are separable by connection framing and by latency,
/// with no credential involved. Four requests would enumerate the reserved set.
///
/// The bound is 256 KiB and five seconds, mirroring what Go's `net/http`
/// discards before it gives up and closes the connection. Past either bound the
/// remainder is left unread and hyper closes — which is also what the reference
/// does, and because both classes share this function the overrun is symmetric
/// too. The outcome is deliberately ignored: the response is the same either
/// way, and the only goal is to leave the connection framed like every other
/// path of the same site.
pub(crate) async fn refusal_drain(body: Incoming) {
    let _ = tokio::time::timeout(REFUSAL_DRAIN_DEADLINE, async move {
        let mut body = body;
        let mut seen = 0usize;
        while let Some(frame) = body.frame().await {
            let Ok(frame) = frame else {
                return;
            };
            if let Some(chunk) = frame.data_ref() {
                seen = seen.saturating_add(chunk.len());
                if seen > REFUSAL_DRAIN_LIMIT {
                    return;
                }
            }
        }
    })
    .await;
}

/// Percent-decodes a request target for classification.
///
/// An undecodable target is classified on its raw form, which cannot match a
/// reserved path and therefore falls to the site exactly as it does today.
pub(crate) fn decoded_path(raw: &str) -> String {
    crate::web::site::percent_decode(raw).unwrap_or_else(|| raw.to_string())
}

/// What a root request is entitled to once its `bridge` query is resolved.
enum RootOutcome {
    /// A valid capability that was served: the one-shot bridge document.
    Bridge(Response<WebBody>),
    /// A valid capability that could not be served. The query must not reach
    /// the operator's application, and the answer must be the ordinary index.
    Suppressed,
    /// No capability at all: an ordinary root request.
    Public,
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
