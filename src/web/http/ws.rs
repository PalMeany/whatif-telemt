//! WebSocket carriers: one multiplexed socket, or one socket per lane.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use tokio::sync::Mutex;
use tracing::debug;

use crate::config::CarrierMode;
use crate::web::error::WebError;
use crate::web::frame::MAX_STREAM_ID;
use crate::web::session::Session;
use crate::web::websocket::{WsMessage, WsReader, WsWriter, accept_key};

use super::headers::{canonical_uint, client_ip, header};
use super::{Relay, RequestHead, WebBody, full, insert};

/// Subprotocol prefix of the multiplexed carrier.
const WS_PROTOCOL_PREFIX: &str = "tproxy-v1.";

/// Subprotocol prefix of a per-stream carrier lane.
const WS_LANE_PROTOCOL_PREFIX: &str = "tproxy-lane-v1.";

/// Deadline for one carrier write and for backend-write backpressure.
const WS_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Delay between uplink retries while the queue budget is exhausted.
const WS_BACKPRESSURE_RETRY: Duration = Duration::from_millis(50);

impl Relay {
    /// Upgrades one authenticated WebSocket carrier.
    pub(crate) async fn serve_websocket(
        self: &Arc<Self>,
        mut request: Request<Incoming>,
        head: RequestHead,
        peer: SocketAddr,
    ) -> Response<WebBody> {
        // A refused upgrade still owns its request, so the body is discarded
        // through the same path every other reserved endpoint uses: a client
        // that declares one must not be able to tell this path apart from an
        // ordinary path of the operator's site by how the connection ends.
        if head.uri.query().is_some() {
            return self.reject_upgrade(request, &head, peer).await;
        }
        if head.method != Method::GET || head.headers.contains_key("authorization") {
            return self.reject_upgrade(request, &head, peer).await;
        }
        if client_ip(peer, &head.headers, &self.trusted_proxies).is_none() {
            return self.reject_upgrade(request, &head, peer).await;
        }
        if !upgrade_requested(&head) || !bodyless(&head) {
            return self.reject_upgrade(request, &head, peer).await;
        }
        let Some(key) = header(&head.headers, "sec-websocket-key").map(str::to_owned) else {
            return self.reject_upgrade(request, &head, peer).await;
        };
        let Some(protocol) = single_subprotocol(&head) else {
            return self.reject_upgrade(request, &head, peer).await;
        };
        let Some(credentials) = WsCredentials::parse(&protocol) else {
            return self.reject_upgrade(request, &head, peer).await;
        };
        let Some(session) = self.session_for(&credentials.token) else {
            return self.reject_upgrade(request, &head, peer).await;
        };
        let expected = if credentials.lanes {
            CarrierMode::WebsocketLanes
        } else {
            CarrierMode::Websocket
        };
        if session.carrier_mode() != expected {
            return self.reject_upgrade(request, &head, peer).await;
        }
        let acquired = if credentials.lanes {
            session.acquire_websocket_lane(credentials.lane_id)
        } else {
            session.acquire_websocket()
        };
        if !acquired {
            return self.reject_upgrade(request, &head, peer).await;
        }

        let upgrade = hyper::upgrade::on(&mut request);
        let read_limit = self.limits.max_body_bytes;
        let idle = Duration::from_millis(self.timeouts.long_poll_ms.saturating_mul(2));
        let lanes = credentials.lanes;
        let lane_id = credentials.lane_id;
        tokio::spawn(async move {
            match upgrade.await {
                Ok(upgraded) => {
                    let io = hyper_util::rt::TokioIo::new(upgraded);
                    run_carrier(io, session.clone(), lanes, lane_id, read_limit, idle).await;
                }
                Err(error) => debug!(error = %error, "WEB carrier upgrade failed"),
            }
            if lanes {
                session.release_websocket_lane(lane_id);
            } else {
                session.release_websocket();
            }
        });

        let mut response = Response::new(full(Bytes::new()));
        *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        let response_headers = response.headers_mut();
        insert(response_headers, "upgrade", "websocket");
        insert(response_headers, "connection", "Upgrade");
        insert(response_headers, "sec-websocket-accept", &accept_key(&key));
        insert(response_headers, "sec-websocket-protocol", &protocol);
        response
    }

    /// Refuses one upgrade request, discarding whatever body it declared.
    async fn reject_upgrade(
        self: &Arc<Self>,
        request: Request<Incoming>,
        head: &RequestHead,
        peer: SocketAddr,
    ) -> Response<WebBody> {
        self.transport_not_found_draining(head, request.into_body(), peer)
            .await
    }
}

/// Credentials carried by the WebSocket subprotocol.
struct WsCredentials {
    token: String,
    lane_id: u32,
    lanes: bool,
}

impl WsCredentials {
    fn parse(protocol: &str) -> Option<Self> {
        if let Some(token) = protocol.strip_prefix(WS_PROTOCOL_PREFIX) {
            if token.is_empty() {
                return None;
            }
            return Some(Self {
                token: token.to_owned(),
                lane_id: 0,
                lanes: false,
            });
        }
        let rest = protocol.strip_prefix(WS_LANE_PROTOCOL_PREFIX)?;
        let (token, lane) = rest.split_once('.')?;
        if token.is_empty() {
            return None;
        }
        let lane_id = canonical_uint(lane)
            .filter(|value| *value != 0 && *value <= u64::from(MAX_STREAM_ID))?;
        Some(Self {
            token: token.to_owned(),
            lane_id: lane_id as u32,
            lanes: true,
        })
    }
}

/// Runs the reader and writer of one upgraded carrier until either ends.
async fn run_carrier<S>(
    io: S,
    session: Arc<Session>,
    lanes: bool,
    lane_id: u32,
    read_limit: usize,
    idle: Duration,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(io);
    let reader = WsReader::new(read_half, read_limit);
    let writer = Arc::new(Mutex::new(WsWriter::new(write_half)));
    let read_task = read_loop(
        reader,
        writer.clone(),
        session.clone(),
        lanes,
        lane_id,
        idle,
    );
    let write_task = write_loop(writer.clone(), session, lanes, lane_id);
    tokio::select! {
        _ = read_task => {}
        _ = write_task => {}
    }
    if let Some(mut writer) = lock_writer(&writer).await {
        let _ = tokio::time::timeout(WS_WRITE_TIMEOUT, writer.write_close()).await;
    }
}

/// Takes the shared writer under the carrier write deadline.
///
/// Every write on this socket is bounded, so the guard itself must be too: a
/// peer that stops reading makes the current writer block on a full send
/// buffer, and an untimed `lock().await` would then pin this task, its file
/// descriptor, and the whole session behind it for as long as the peer cares
/// to hold the socket open.
async fn lock_writer<W>(
    writer: &Arc<Mutex<WsWriter<W>>>,
) -> Option<tokio::sync::MutexGuard<'_, WsWriter<W>>> {
    tokio::time::timeout(WS_WRITE_TIMEOUT, writer.lock())
        .await
        .ok()
}

/// Applies client messages as uplink batches with bounded backpressure waits.
async fn read_loop<R, W>(
    mut reader: WsReader<R>,
    writer: Arc<Mutex<WsWriter<W>>>,
    session: Arc<Session>,
    lanes: bool,
    lane_id: u32,
    idle: Duration,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut sequence = 1u64;
    loop {
        let message = match tokio::time::timeout(idle, reader.read_message()).await {
            Ok(Ok(message)) => message,
            _ => return,
        };
        let payload = match message {
            WsMessage::Binary(payload) if !payload.is_empty() => payload,
            // An empty binary message carries no frame at all. The reference
            // client never sends one, but tearing the carrier down over it
            // would turn a harmless keepalive into a session loss.
            WsMessage::Binary(_) => continue,
            WsMessage::Ping(payload) => {
                let Some(mut writer) = lock_writer(&writer).await else {
                    return;
                };
                match tokio::time::timeout(WS_WRITE_TIMEOUT, writer.write_pong(&payload)).await {
                    Ok(Ok(())) => continue,
                    _ => return,
                }
            }
            WsMessage::Pong => continue,
            WsMessage::Close => return,
        };
        let deadline = tokio::time::Instant::now() + WS_WRITE_TIMEOUT;
        loop {
            let result = if lanes {
                session.process_up_lane(lane_id, sequence, &payload)
            } else {
                session.process_up(sequence, &payload)
            };
            match result {
                Ok(ack) if ack == sequence => {
                    sequence += 1;
                    break;
                }
                Err(WebError::Backpressure) | Err(WebError::Concurrent) => {
                    if tokio::time::Instant::now() >= deadline {
                        return;
                    }
                    tokio::time::sleep(WS_BACKPRESSURE_RETRY).await;
                }
                _ => return,
            }
        }
    }
}

/// Streams downlink batches, pinging the peer during idle poll periods.
async fn write_loop<W>(
    writer: Arc<Mutex<WsWriter<W>>>,
    session: Arc<Session>,
    lanes: bool,
    lane_id: u32,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut cursor = 0u64;
    loop {
        let polled = if lanes {
            session.poll_lane(lane_id, cursor).await
        } else {
            session
                .poll(cursor)
                .await
                .map(|(body, next)| (body, next, false))
        };
        let Ok((payload, next, lane_closed)) = polled else {
            return;
        };
        if lane_closed {
            return;
        }
        let Some(mut writer) = lock_writer(&writer).await else {
            return;
        };
        if payload.is_empty() {
            match tokio::time::timeout(WS_WRITE_TIMEOUT, writer.write_ping()).await {
                Ok(Ok(())) => continue,
                _ => return,
            }
        }
        match tokio::time::timeout(WS_WRITE_TIMEOUT, writer.write_binary(&payload)).await {
            Ok(Ok(())) => cursor = next,
            _ => return,
        }
    }
}

/// True when the request is a well-formed WebSocket upgrade.
fn upgrade_requested(head: &RequestHead) -> bool {
    let upgrade = header(&head.headers, "upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let connection = header(&head.headers, "connection").is_some_and(|value| {
        value
            .split(',')
            .any(|item| item.trim().eq_ignore_ascii_case("upgrade"))
    });
    let version = header(&head.headers, "sec-websocket-version").is_some_and(|value| value == "13");
    upgrade && connection && version
}

/// True when the upgrade request declares no body.
fn bodyless(head: &RequestHead) -> bool {
    match header(&head.headers, "content-length") {
        Some(value) => value == "0",
        None => true,
    }
}

/// Returns the single requested subprotocol, if there is exactly one.
fn single_subprotocol(head: &RequestHead) -> Option<String> {
    let mut found: Option<String> = None;
    for value in head.headers.get_all("sec-websocket-protocol") {
        let text = value.to_str().ok()?;
        for item in text.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(item.to_owned());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiplexed_and_lane_credentials() {
        let plain = WsCredentials::parse("tproxy-v1.abc").expect("plain");
        assert_eq!(plain.token, "abc");
        assert!(!plain.lanes);

        let lane = WsCredentials::parse("tproxy-lane-v1.abc.7").expect("lane");
        assert_eq!(lane.token, "abc");
        assert_eq!(lane.lane_id, 7);
        assert!(lane.lanes);

        assert!(WsCredentials::parse("tproxy-v1.").is_none());
        assert!(WsCredentials::parse("tproxy-lane-v1.abc.0").is_none());
        assert!(WsCredentials::parse("tproxy-lane-v1.abc.07").is_none());
        assert!(WsCredentials::parse("tproxy-lane-v1..7").is_none());
        assert!(WsCredentials::parse("other").is_none());
    }
}
