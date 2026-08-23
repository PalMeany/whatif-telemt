//! Session creation, serialized uplink, and long-poll downlink endpoints.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use bytes::Bytes;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};

use crate::config::CarrierMode;
use crate::web::error::WebError;
use crate::web::frame::MAX_STREAM_ID;
use crate::web::session::Session;

use super::headers::{
    bearer_token, binary_content_type, canonical_uint, client_ip, header, header_present,
};
use super::{Relay, RequestHead, WebBody, empty_body, full, insert, read_body, request_head};

/// A create body is one HELLO frame, so a tiny cap keeps an unauthenticated
/// POST from streaming megabytes before the bootstrap check rejects it.
const MAX_CREATE_BODY_BYTES: usize = 64;

impl Relay {
    /// Dispatches one of the four reserved transport paths.
    /// `path` is the percent-decoded request target the router classified on,
    /// so `/api/v1/%75p` dispatches as the uplink it names rather than falling
    /// through to the site with its carrier headers intact.
    pub(crate) async fn serve_api(
        self: &Arc<Self>,
        request: Request<Incoming>,
        peer: SocketAddr,
        path: String,
    ) -> Response<WebBody> {
        let head = request_head(&request);
        let websocket = path == "/api/v1/ws";
        // The upgrade endpoint needs the request intact for hyper's upgrade
        // handle, so it owns its own refusal path and drains there.
        if websocket {
            return self.serve_websocket(request, head, peer).await;
        }
        // From here the body belongs to this function: every refusal has to
        // consume it, or the reserved paths become distinguishable from the
        // operator's own paths by connection framing alone.
        let body = request.into_body();
        if head.uri.query().is_some() || head.headers.contains_key("cookie") {
            return self.transport_not_found_draining(&head, body, peer).await;
        }
        let Some(ip) = client_ip(peer, &head.headers, &self.trusted_proxies) else {
            return self.transport_not_found_draining(&head, body, peer).await;
        };
        let Some(token) = bearer_token(header(&head.headers, "authorization")).map(str::to_owned)
        else {
            return self.transport_not_found_draining(&head, body, peer).await;
        };
        match path.as_str() {
            "/api/v1/session" => self.serve_session(head, body, peer, &token, ip).await,
            "/api/v1/up" => self.serve_up(head, body, peer, &token).await,
            "/api/v1/down" => self.serve_down(head, body, peer, &token).await,
            _ => self.transport_not_found_draining(&head, body, peer).await,
        }
    }

    /// Creates or deletes one relay session.
    async fn serve_session(
        self: &Arc<Self>,
        head: RequestHead,
        body: Incoming,
        peer: SocketAddr,
        token: &str,
        ip: IpAddr,
    ) -> Response<WebBody> {
        if head.method == Method::DELETE {
            if self.manager.close_token(token).is_err() {
                return self.transport_not_found_draining(&head, body, peer).await;
            }
            let bodyless = empty_body(body, self.body_deadline()).await;
            if !bodyless || head.headers.contains_key("content-type") {
                return self.transport_not_found(&head, peer).await;
            }
            let mut response = Response::new(full(Bytes::new()));
            *response.status_mut() = StatusCode::NO_CONTENT;
            insert(response.headers_mut(), "cache-control", "no-store");
            return response;
        }
        if head.method != Method::POST
            || !binary_content_type(header(&head.headers, "content-type"))
            || !self.manager.has_bootstrap(token)
        {
            return self.transport_not_found_draining(&head, body, peer).await;
        }
        let Some(payload) = read_body(body, MAX_CREATE_BODY_BYTES, self.body_deadline()).await
        else {
            return self.transport_not_found(&head, peer).await;
        };
        if payload.is_empty() {
            return self.transport_not_found(&head, peer).await;
        }
        match self.manager.create(token, ip, &payload) {
            Ok(outcome) => {
                let welcome = Bytes::from(outcome.welcome);
                let mut response = Response::new(full(welcome.clone()));
                let response_headers = response.headers_mut();
                insert(response_headers, "content-type", "application/octet-stream");
                insert(response_headers, "cache-control", "no-store");
                insert(response_headers, "x-session-token", &outcome.token);
                insert(
                    response_headers,
                    "x-carrier-mode",
                    outcome.session.carrier_mode().as_str(),
                );
                insert(response_headers, "x-down-cursor", "0");
                insert(
                    response_headers,
                    "content-length",
                    &welcome.len().to_string(),
                );
                response
            }
            Err(WebError::Limit) => self.retry_later(),
            Err(_) => self.transport_not_found(&head, peer).await,
        }
    }

    /// Applies one uplink batch to the shared carrier or to one lane.
    async fn serve_up(
        self: &Arc<Self>,
        head: RequestHead,
        body: Incoming,
        peer: SocketAddr,
        token: &str,
    ) -> Response<WebBody> {
        if head.method != Method::POST
            || !binary_content_type(header(&head.headers, "content-type"))
        {
            return self.transport_not_found_draining(&head, body, peer).await;
        }
        let sequence = header(&head.headers, "x-up-seq").and_then(canonical_uint);
        let Some(sequence) = sequence.filter(|value| *value != 0) else {
            return self.transport_not_found_draining(&head, body, peer).await;
        };
        let Some(session) = self.manager.get(token) else {
            return self.transport_not_found_draining(&head, body, peer).await;
        };
        let Some(payload) = read_body(body, self.limits.max_body_bytes, self.body_deadline()).await
        else {
            return self.transport_not_found(&head, peer).await;
        };
        if payload.is_empty() {
            return self.transport_not_found(&head, peer).await;
        }
        let lane = header(&head.headers, "x-lane-id");
        let result = match session.carrier_mode() {
            CarrierMode::Https => {
                // Presence is the violation, not decodability: a non-UTF-8
                // `X-Lane-ID` read as absent would route a lanes request onto
                // the shared carrier, where the reference answers its 404.
                if header_present(&head.headers, "x-lane-id") {
                    return self.transport_not_found(&head, peer).await;
                }
                session.process_up(sequence, &payload)
            }
            CarrierMode::HttpsLanes => {
                let Some(lane_id) = lane
                    .and_then(canonical_uint)
                    .filter(|value| *value <= u64::from(MAX_STREAM_ID))
                else {
                    return self.transport_not_found(&head, peer).await;
                };
                session.process_up_lane(lane_id as u32, sequence, &payload)
            }
            _ => return self.transport_not_found(&head, peer).await,
        };
        match result {
            Ok(ack) => {
                let mut response = Response::new(full(Bytes::new()));
                *response.status_mut() = StatusCode::NO_CONTENT;
                let response_headers = response.headers_mut();
                insert(response_headers, "cache-control", "no-store");
                insert(response_headers, "x-up-ack", &ack.to_string());
                response
            }
            // A refused lane is a capacity condition, not an authentication
            // one, so it gets the same retryable answer as a refused session.
            Err(WebError::Backpressure) | Err(WebError::Concurrent) | Err(WebError::Limit) => {
                self.retry_later()
            }
            Err(_) => self.transport_not_found(&head, peer).await,
        }
    }

    /// Parks one downlink poll for the shared carrier or for one lane.
    async fn serve_down(
        self: &Arc<Self>,
        head: RequestHead,
        body: Incoming,
        peer: SocketAddr,
        token: &str,
    ) -> Response<WebBody> {
        if head.method != Method::POST || head.headers.contains_key("content-type") {
            return self.transport_not_found_draining(&head, body, peer).await;
        }
        let Some(cursor) = header(&head.headers, "x-down-cursor").and_then(canonical_uint) else {
            return self.transport_not_found_draining(&head, body, peer).await;
        };
        let Some(session) = self.manager.get(token) else {
            return self.transport_not_found_draining(&head, body, peer).await;
        };
        if !empty_body(body, self.body_deadline()).await {
            return self.transport_not_found(&head, peer).await;
        }
        let lane = header(&head.headers, "x-lane-id");
        let result = match session.carrier_mode() {
            CarrierMode::Https => {
                // Presence is the violation, not decodability: a non-UTF-8
                // `X-Lane-ID` read as absent would route a lanes request onto
                // the shared carrier, where the reference answers its 404.
                if header_present(&head.headers, "x-lane-id") {
                    return self.transport_not_found(&head, peer).await;
                }
                session
                    .poll(cursor)
                    .await
                    .map(|(body, next)| (body, next, false))
            }
            CarrierMode::HttpsLanes => {
                let Some(lane_id) = lane
                    .and_then(canonical_uint)
                    .filter(|value| *value <= u64::from(MAX_STREAM_ID))
                else {
                    return self.transport_not_found(&head, peer).await;
                };
                session.poll_lane(lane_id as u32, cursor).await
            }
            _ => return self.transport_not_found(&head, peer).await,
        };
        match result {
            Ok((payload, next, lane_closed)) => {
                let mut response = Response::new(full(Bytes::new()));
                {
                    let response_headers = response.headers_mut();
                    insert(response_headers, "cache-control", "no-store");
                    insert(response_headers, "x-down-cursor", &next.to_string());
                    if lane_closed {
                        insert(response_headers, "x-lane-closed", "1");
                    }
                }
                if payload.is_empty() {
                    *response.status_mut() = StatusCode::NO_CONTENT;
                    return response;
                }
                {
                    let response_headers = response.headers_mut();
                    insert(response_headers, "content-type", "application/octet-stream");
                    insert(
                        response_headers,
                        "content-length",
                        &payload.len().to_string(),
                    );
                }
                *response.body_mut() = full(payload);
                response
            }
            Err(WebError::Concurrent) => self.retry_later(),
            Err(_) => self.transport_not_found(&head, peer).await,
        }
    }

    /// Resolves a session bearer for the WebSocket endpoints.
    pub(crate) fn session_for(&self, token: &str) -> Option<Arc<Session>> {
        self.manager.get(token)
    }

    /// The single retryable answer: capacity will return shortly.
    ///
    /// It is counted, because a 503 here is the only externally visible symptom
    /// of an exhausted queue budget or a saturated capacity ceiling, and the
    /// deliberately indistinguishable 404 hides everything else.
    fn retry_later(&self) -> Response<WebBody> {
        self.manager.count_retry_later();
        let mut response = Response::new(full(Bytes::new()));
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        let response_headers = response.headers_mut();
        insert(response_headers, "cache-control", "no-store");
        insert(response_headers, "retry-after", "1");
        response
    }
}
