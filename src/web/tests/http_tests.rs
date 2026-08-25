//! End-to-end HTTP tests driving the relay over a real TCP listener.

use std::time::Duration;

use crate::config::{CarrierMode, WebBackend, WebLimits, WebTimeouts};
use crate::web::capability::{derive_capability, encode_token};
use crate::web::frame::{self, FrameType};

use super::harness::{
    PublicSite, RelayFixture, TEST_HOST, TEST_SECRET, batch, build_manager,
    build_manager_with_stats, data_payloads, header_value, http_request, start_echo_backend,
    start_relay,
};
use super::internal_backend_tests::{authenticating_handshake, secure_mode_config};
use crate::config::WebBackend as Backend;
use crate::protocol::ProtoTag;

async fn start_fixture(carrier: CarrierMode) -> RelayFixture {
    start_fixture_with_limits(carrier, WebLimits::default()).await
}

/// Starts a relay whose manager and listener share caller-chosen ceilings.
///
/// The retryable answers only appear once a ceiling is actually reachable, and
/// every production default is far out of reach of a test.
async fn start_fixture_with_limits(carrier: CarrierMode, limits: WebLimits) -> RelayFixture {
    let backend = start_echo_backend().await;
    let manager = build_manager(WebBackend::Loopback(backend), carrier, limits.clone());
    start_relay(
        manager,
        PublicSite::Directory,
        limits,
        WebTimeouts::default(),
    )
    .await
}

fn get(path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: {TEST_HOST}\r\nConnection: close\r\n\r\n").into_bytes()
}

fn post(path: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut request = format!("POST {path} HTTP/1.1\r\nHost: {TEST_HOST}\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    request.push_str("Connection: close\r\n\r\n");
    let mut out = request.into_bytes();
    out.extend_from_slice(body);
    out
}

fn bridge_url() -> String {
    format!(
        "/?bridge={}",
        encode_token(&derive_capability(TEST_HOST, &TEST_SECRET))
    )
}

fn bootstrap_from_page(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let start = text.find(",bootstrap=\"").expect("bootstrap literal") + ",bootstrap=\"".len();
    let rest = &text[start..];
    let end = rest.find('"').expect("bootstrap terminator");
    rest[..end].to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_paths_and_plain_root_serve_the_public_site() {
    let fixture = start_fixture(CarrierMode::Https).await;
    let (status, headers, body) = http_request(fixture.address, &get("/")).await;
    assert_eq!(status, 200);
    assert_eq!(body, b"<h1>site</h1>");
    assert_eq!(
        header_value(&headers, "cache-control"),
        Some("public, max-age=300")
    );

    let (status, _, body) = http_request(fixture.address, &get("/nope")).await;
    assert_eq!(status, 404);
    assert_eq!(body, b"<h1>missing</h1>");

    // A wrong capability is answered by the ordinary index, not a 404.
    let (status, _, body) = http_request(
        fixture.address,
        &get(&format!("/?bridge={}", encode_token(&[7u8; 32]))),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body, b"<h1>site</h1>");

    // Unauthenticated transport paths look exactly like an unknown path.
    let (status, _, body) = http_request(
        fixture.address,
        &post(
            "/api/v1/up",
            &[("Content-Type", "application/octet-stream")],
            b"x",
        ),
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(body, b"<h1>missing</h1>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_capability_serves_a_one_shot_bridge_page() {
    let fixture = start_fixture(CarrierMode::WebsocketLanes).await;
    let (status, headers, body) = http_request(fixture.address, &get(&bridge_url())).await;
    assert_eq!(status, 200);
    assert_eq!(
        header_value(&headers, "content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(header_value(&headers, "cache-control"), Some("no-store"));
    let csp = header_value(&headers, "content-security-policy").expect("csp");
    assert!(csp.contains("script-src 'nonce-"));
    assert!(csp.contains(&format!("connect-src 'self' wss://{TEST_HOST}")));
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("\"websocket-lanes\""));
    assert!(!bootstrap_from_page(&body).is_empty());

    // Each render issues a fresh bootstrap.
    let (_, _, second) = http_request(fixture.address, &get(&bridge_url())).await;
    assert_ne!(bootstrap_from_page(&body), bootstrap_from_page(&second));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_https_carrier_round_trip() {
    let fixture = start_fixture(CarrierMode::Https).await;
    let (_, _, page) = http_request(fixture.address, &get(&bridge_url())).await;
    let bootstrap = bootstrap_from_page(&page);

    let hello = frame::encode(FrameType::HELLO, 0, &[1]);
    let (status, headers, body) = http_request(
        fixture.address,
        &post(
            "/api/v1/session",
            &[
                ("Authorization", &format!("Bearer {bootstrap}")),
                ("Content-Type", "application/octet-stream"),
            ],
            &hello,
        ),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(header_value(&headers, "x-carrier-mode"), Some("https"));
    assert_eq!(header_value(&headers, "x-down-cursor"), Some("0"));
    assert_eq!(body, frame::encode(FrameType::WELCOME, 0, &[]));
    let token = header_value(&headers, "x-session-token")
        .expect("session token")
        .to_string();

    let payload = b"carrier-payload".to_vec();
    let uplink = batch(&[
        (FrameType::OPEN, 1, Vec::new()),
        (FrameType::DATA, 1, payload.clone()),
    ]);
    let (status, headers, _) = http_request(
        fixture.address,
        &post(
            "/api/v1/up",
            &[
                ("Authorization", &format!("Bearer {token}")),
                ("Content-Type", "application/octet-stream"),
                ("X-Up-Seq", "1"),
            ],
            &uplink,
        ),
    )
    .await;
    assert_eq!(status, 204);
    assert_eq!(header_value(&headers, "x-up-ack"), Some("1"));

    let mut cursor = "0".to_string();
    let mut echoed = Vec::new();
    for _ in 0..20 {
        let (status, headers, body) = http_request(
            fixture.address,
            &post(
                "/api/v1/down",
                &[
                    ("Authorization", &format!("Bearer {token}")),
                    ("X-Down-Cursor", &cursor),
                ],
                b"",
            ),
        )
        .await;
        cursor = header_value(&headers, "x-down-cursor")
            .unwrap_or("0")
            .to_string();
        if status == 200 {
            echoed.extend_from_slice(&data_payloads(&body, 1));
            if echoed.len() >= payload.len() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(echoed, payload);

    // Deleting the session is idempotent for a currently valid bearer.
    let delete = format!(
        "DELETE /api/v1/session HTTP/1.1\r\nHost: {TEST_HOST}\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let (status, _, _) = http_request(fixture.address, delete.as_bytes()).await;
    assert_eq!(status, 204);
    assert_eq!(fixture.manager.capacity().sessions, 0);
    assert_eq!(fixture.manager.capacity().pending_bytes, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_host_and_bad_sequences_are_refused() {
    let fixture = start_fixture(CarrierMode::Https).await;
    let wrong_host =
        b"GET / HTTP/1.1\r\nHost: other.example.com\r\nConnection: close\r\n\r\n".to_vec();
    let (status, _, body) = http_request(fixture.address, &wrong_host).await;
    assert_eq!(status, 404);
    assert_eq!(body, b"<h1>missing</h1>");

    let (_, _, page) = http_request(fixture.address, &get(&bridge_url())).await;
    let bootstrap = bootstrap_from_page(&page);
    let hello = frame::encode(FrameType::HELLO, 0, &[1]);
    let (_, headers, _) = http_request(
        fixture.address,
        &post(
            "/api/v1/session",
            &[
                ("Authorization", &format!("Bearer {bootstrap}")),
                ("Content-Type", "application/octet-stream"),
            ],
            &hello,
        ),
    )
    .await;
    let token = header_value(&headers, "x-session-token")
        .expect("token")
        .to_string();

    // A non-canonical sequence header is rejected before the body is applied.
    let uplink = batch(&[(FrameType::OPEN, 1, Vec::new())]);
    let (status, _, _) = http_request(
        fixture.address,
        &post(
            "/api/v1/up",
            &[
                ("Authorization", &format!("Bearer {token}")),
                ("Content-Type", "application/octet-stream"),
                ("X-Up-Seq", "01"),
            ],
            &uplink,
        ),
    )
    .await;
    assert_eq!(status, 404);

    // A cookie-bearing carrier request is refused.
    let (status, _, _) = http_request(
        fixture.address,
        &post(
            "/api/v1/up",
            &[
                ("Authorization", &format!("Bearer {token}")),
                ("Content-Type", "application/octet-stream"),
                ("X-Up-Seq", "1"),
                ("Cookie", "a=b"),
            ],
            &uplink,
        ),
    )
    .await;
    assert_eq!(status, 404);
}

/// Starts a relay whose streams terminate in this process, like a deployment.
async fn start_internal_fixture(carrier: CarrierMode) -> RelayFixture {
    let (manager, _) = build_manager_with_stats(
        secure_mode_config(),
        Backend::Internal,
        carrier,
        WebLimits::default(),
    );
    start_relay(
        manager,
        PublicSite::Directory,
        WebLimits::default(),
        WebTimeouts::default(),
    )
    .await
}

/// Creates a session over the HTTPS surface and returns its bearer.
async fn open_session(fixture: &RelayFixture) -> String {
    let (_, _, page) = http_request(fixture.address, &get(&bridge_url())).await;
    let bootstrap = bootstrap_from_page(&page);
    let hello = frame::encode(FrameType::HELLO, 0, &[1]);
    let (status, headers, _) = http_request(
        fixture.address,
        &post(
            "/api/v1/session",
            &[
                ("Authorization", &format!("Bearer {bootstrap}")),
                ("Content-Type", "application/octet-stream"),
            ],
            &hello,
        ),
    )
    .await;
    assert_eq!(status, 200, "session creation failed");
    header_value(&headers, "x-session-token")
        .expect("session token")
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_handshake_survives_an_https_lane() {
    // `https-lanes` drives one request lane per stream. This is the same
    // handshake the shared `https` carrier accepts, sent the way a lane client
    // sends it, against streams that terminate in this process.
    let fixture = start_internal_fixture(CarrierMode::HttpsLanes).await;
    let token = open_session(&fixture).await;

    let handshake = authenticating_handshake(&TEST_SECRET, ProtoTag::Secure);
    let uplink = batch(&[
        (FrameType::OPEN, 1, Vec::new()),
        (FrameType::DATA, 1, handshake.to_vec()),
    ]);
    let (status, headers, _) = http_request(
        fixture.address,
        &post(
            "/api/v1/up",
            &[
                ("Authorization", &format!("Bearer {token}")),
                ("Content-Type", "application/octet-stream"),
                ("X-Up-Seq", "1"),
                ("X-Lane-ID", "1"),
            ],
            &uplink,
        ),
    )
    .await;
    assert_eq!(
        status,
        204,
        "the lane refused the opening batch: {:?}",
        header_value(&headers, "x-up-ack")
    );

    // The relay must answer the lane with something: data, or the CLOSE that
    // tells the client the stream is gone. Silence is the reported symptom.
    let mut cursor = "0".to_string();
    let mut answered = false;
    for _ in 0..12 {
        let (status, headers, body) = http_request(
            fixture.address,
            &post(
                "/api/v1/down",
                &[
                    ("Authorization", &format!("Bearer {token}")),
                    ("X-Down-Cursor", &cursor),
                    ("X-Lane-ID", "1"),
                ],
                b"",
            ),
        )
        .await;
        assert!(
            status == 200 || status == 204,
            "the lane downlink answered {status}"
        );
        cursor = header_value(&headers, "x-down-cursor")
            .unwrap_or("0")
            .to_string();
        if status == 200 && !body.is_empty() {
            answered = true;
            break;
        }
        if header_value(&headers, "x-lane-closed") == Some("1") {
            answered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        answered,
        "the lane carried the handshake but never answered: a client sits on \"connecting\" here"
    );
}

/// Renders the bridge page once and returns the bootstrap it carries.
async fn fresh_bootstrap(fixture: &RelayFixture) -> String {
    let (_, _, page) = http_request(fixture.address, &get(&bridge_url())).await;
    bootstrap_from_page(&page)
}

/// Posts one session-creation body under a bootstrap bearer.
async fn create_session(
    fixture: &RelayFixture,
    bootstrap: &str,
    body: &[u8],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    http_request(
        fixture.address,
        &post(
            "/api/v1/session",
            &[
                ("Authorization", &format!("Bearer {bootstrap}")),
                ("Content-Type", "application/octet-stream"),
            ],
            body,
        ),
    )
    .await
}

/// Posts one uplink batch, optionally on a named lane.
async fn post_uplink(
    fixture: &RelayFixture,
    token: &str,
    sequence: u64,
    lane: Option<u32>,
    body: &[u8],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let bearer = format!("Bearer {token}");
    let sequence = sequence.to_string();
    let mut headers: Vec<(&str, &str)> = vec![
        ("Authorization", &bearer),
        ("Content-Type", "application/octet-stream"),
        ("X-Up-Seq", &sequence),
    ];
    let lane = lane.map(|value| value.to_string());
    if let Some(lane) = lane.as_deref() {
        headers.push(("X-Lane-ID", lane));
    }
    http_request(fixture.address, &post("/api/v1/up", &headers, body)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_refused_by_a_capacity_ceiling_is_retryable_with_the_same_bootstrap() {
    // The bridge page's whole retry loop is `if (response.status !== 503) give
    // up`, so mapping a capacity refusal onto the deliberately indistinguishable
    // 404 turns a transient condition into a dead carrier for every client that
    // meets it. The half that actually matters is the second one: refusing must
    // not burn the one-shot bootstrap, because the retry the page is about to
    // make carries that same bootstrap and has no other credential to fall back
    // on.
    let mut limits = WebLimits::default();
    limits.max_sessions_global = 1;
    let fixture = start_fixture_with_limits(CarrierMode::Https, limits).await;

    let first = fresh_bootstrap(&fixture).await;
    let second = fresh_bootstrap(&fixture).await;
    let hello = frame::encode(FrameType::HELLO, 0, &[1]);

    let (status, headers, _) = create_session(&fixture, &first, &hello).await;
    assert_eq!(status, 200);
    let token = header_value(&headers, "x-session-token")
        .expect("session token")
        .to_string();

    let (status, headers, _) = create_session(&fixture, &second, &hello).await;
    assert_eq!(status, 503, "a full session table must answer retryably");
    assert_eq!(header_value(&headers, "retry-after"), Some("1"));
    assert_eq!(header_value(&headers, "cache-control"), Some("no-store"));
    assert_eq!(fixture.manager.capacity().sessions, 1);

    // Free the slot the way a client would, then repeat the refused request
    // byte for byte.
    let delete = format!(
        "DELETE /api/v1/session HTTP/1.1\r\nHost: {TEST_HOST}\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let (status, _, _) = http_request(fixture.address, delete.as_bytes()).await;
    assert_eq!(status, 204);
    assert_eq!(fixture.manager.capacity().sessions, 0);

    let (status, headers, body) = create_session(&fixture, &second, &hello).await;
    assert_eq!(
        status, 200,
        "the refusal consumed the bootstrap the client was told to retry with"
    );
    assert_eq!(body, frame::encode(FrameType::WELCOME, 0, &[]));
    assert!(header_value(&headers, "x-session-token").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uplink_that_will_not_fit_the_queue_budget_is_retryable_at_the_same_sequence() {
    // Backpressure is a capacity condition, not an authentication one, so it has
    // to reach the client as the one status every carrier retries. And because
    // the refusal is all-or-nothing, the sequence must stay uncommitted: a
    // client that shrinks its batch and resends at the same number would
    // otherwise be read as replaying a committed sequence with a changed body,
    // which is a session-fatal protocol violation.
    let mut limits = WebLimits::default();
    limits.max_streams_per_session = 1;
    // Small enough that the uplink partition cannot hold one 512-byte DATA
    // frame once the control reserve has been carved out of it.
    limits.max_pending_per_session = 5500;
    limits.max_pending_items_per_session = 64;
    let fixture = start_fixture_with_limits(CarrierMode::Https, limits).await;
    let token = open_session(&fixture).await;

    let oversized = batch(&[
        (FrameType::OPEN, 23, Vec::new()),
        (FrameType::DATA, 23, vec![1u8; 512]),
    ]);
    let (status, headers, _) = post_uplink(&fixture, &token, 1, None, &oversized).await;
    assert_eq!(status, 503, "a refused uplink batch must be retryable");
    assert_eq!(header_value(&headers, "retry-after"), Some("1"));
    assert_eq!(header_value(&headers, "cache-control"), Some("no-store"));
    assert_eq!(header_value(&headers, "x-up-ack"), None);
    // Not one frame of the batch was applied: the OPEN that preceded the DATA
    // never reached the stream table.
    assert_eq!(fixture.manager.capacity().streams, 0);

    let (status, headers, _) = post_uplink(
        &fixture,
        &token,
        1,
        None,
        &batch(&[(FrameType::OPEN, 23, Vec::new())]),
    )
    .await;
    assert_eq!(
        status, 204,
        "the refused sequence must still be open for a smaller retry"
    );
    assert_eq!(header_value(&headers, "x-up-ack"), Some("1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_evicted_lane_answers_its_poll_as_closed_rather_than_as_a_stranger() {
    // A lane whose queue was reclaimed still has a client polling it. Answering
    // that poll with the site's 404 reads as parent-carrier failure, and the
    // bridge tears down every other lane on the session for it; the header says
    // "this one is done, stop polling it" and costs the client nothing else.
    let mut limits = WebLimits::default();
    // Caps the session at three carrier lanes (two per live stream, plus lane
    // zero), so the third lane opened has to reclaim the first drained one.
    limits.max_streams_per_session = 1;
    let fixture = start_fixture_with_limits(CarrierMode::HttpsLanes, limits).await;
    let token = open_session(&fixture).await;

    for lane in 1..=2u32 {
        let (status, _, _) = post_uplink(
            &fixture,
            &token,
            1,
            Some(lane),
            &batch(&[(FrameType::OPEN, lane, Vec::new())]),
        )
        .await;
        assert_eq!(status, 204, "lane {lane} was refused");
        let (status, _, _) = post_uplink(
            &fixture,
            &token,
            2,
            Some(lane),
            &batch(&[(FrameType::CLOSE, lane, Vec::new())]),
        )
        .await;
        assert_eq!(status, 204, "lane {lane} could not be closed");
    }
    let (status, _, _) = post_uplink(
        &fixture,
        &token,
        1,
        Some(3),
        &batch(&[(FrameType::OPEN, 3, Vec::new())]),
    )
    .await;
    assert_eq!(status, 204, "the lane that forces the reclaim was refused");

    let (status, headers, body) = http_request(
        fixture.address,
        &post(
            "/api/v1/down",
            &[
                ("Authorization", &format!("Bearer {token}")),
                ("X-Down-Cursor", "0"),
                ("X-Lane-ID", "1"),
            ],
            b"",
        ),
    )
    .await;
    assert_eq!(
        status, 204,
        "a reclaimed lane must not answer like an unknown path"
    );
    assert_eq!(header_value(&headers, "x-lane-closed"), Some("1"));
    assert_eq!(body, b"");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_hello_is_refused_without_consuming_the_bootstrap() {
    // The ordering is the whole contract. A one-shot bootstrap keys its replay
    // check on the digest of the body that redeemed it, so a create refused
    // *after* the bootstrap was marked used would make the client's corrected
    // retry fail authentication instead of succeeding — and the client has no
    // second bootstrap, so it would have to re-render the bridge page to
    // recover from its own typo.
    let fixture = start_fixture(CarrierMode::Https).await;
    let bootstrap = fresh_bootstrap(&fixture).await;

    let mut trailing = frame::encode(FrameType::HELLO, 0, &[1]);
    trailing.extend_from_slice(&frame::encode(FrameType::PONG, 0, &[]));
    for (name, body) in [
        (
            "a v2 protocol byte",
            frame::encode(FrameType::HELLO, 0, &[2]),
        ),
        ("an empty payload", frame::encode(FrameType::HELLO, 0, &[])),
        (
            "a two-byte payload",
            frame::encode(FrameType::HELLO, 0, &[1, 1]),
        ),
        (
            "a nonzero stream id",
            frame::encode(FrameType::HELLO, 7, &[1]),
        ),
        ("a frame after the HELLO", trailing),
    ] {
        let (status, _, page) = create_session(&fixture, &bootstrap, &body).await;
        assert_eq!(status, 404, "{name} was not refused");
        assert_eq!(
            page, b"<h1>missing</h1>",
            "{name} was answered off something other than the site's 404 path"
        );
    }

    let (status, headers, body) = create_session(
        &fixture,
        &bootstrap,
        &frame::encode(FrameType::HELLO, 0, &[1]),
    )
    .await;
    assert_eq!(
        status, 200,
        "the refusals consumed the one-shot bootstrap the client must retry with"
    );
    assert_eq!(body, frame::encode(FrameType::WELCOME, 0, &[]));
    assert!(header_value(&headers, "x-session-token").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_a_rendered_bridge_page_is_counted() {
    use crate::web::metrics::WebMetricsSource;

    // The counter exists to be read against sessions_created_total: a gap
    // between them is a client that loaded the bridge and never reached the
    // carrier. That only holds if an ordinary visit never moves it.
    let fixture = start_fixture(CarrierMode::Https).await;
    assert_eq!(fixture.manager.snapshot().bridge_pages_served, 0);

    let (status, _, _) = http_request(fixture.address, &get("/")).await;
    assert_eq!(status, 200);
    let (status, _, _) = http_request(
        fixture.address,
        &get(&format!("/?bridge={}", encode_token(&[7u8; 32]))),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(fixture.manager.snapshot().bridge_pages_served, 0);

    let (status, _, body) = http_request(fixture.address, &get(&bridge_url())).await;
    assert_eq!(status, 200);
    assert!(!body.is_empty());
    assert_eq!(fixture.manager.snapshot().bridge_pages_served, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payload_counters_move_only_for_stream_bytes() {
    use crate::web::metrics::WebMetricsSource;

    // bytes_* counts carrier bodies, so it moves for framing and for polls
    // that carried nothing. stream_bytes_* must move only for the MTProto
    // payload that actually crossed the backend boundary.
    let fixture = start_fixture(CarrierMode::Https).await;
    let (_, headers, page) = http_request(fixture.address, &get(&bridge_url())).await;
    assert_eq!(header_value(&headers, "x-carrier-mode"), None);
    let bootstrap = bootstrap_from_page(&page);

    let (status, headers, _) = http_request(
        fixture.address,
        &post(
            "/api/v1/session",
            &[
                ("Authorization", &format!("Bearer {bootstrap}")),
                ("Content-Type", "application/octet-stream"),
            ],
            &frame::encode(FrameType::HELLO, 0, &[1]),
        ),
    )
    .await;
    assert_eq!(status, 200);
    let token = header_value(&headers, "x-session-token")
        .expect("session token")
        .to_string();

    // A session exists and its creation moved no payload at all.
    let before = fixture.manager.snapshot();
    assert_eq!(before.stream_bytes_up, 0);
    assert_eq!(before.stream_bytes_down, 0);

    let payload = b"web payload".to_vec();
    let uplink = batch(&[
        (FrameType::OPEN, 1, Vec::new()),
        (FrameType::DATA, 1, payload.clone()),
    ]);
    let (status, _, _) = http_request(
        fixture.address,
        &post(
            "/api/v1/up",
            &[
                ("Authorization", &format!("Bearer {token}")),
                ("Content-Type", "application/octet-stream"),
                ("X-Up-Seq", "1"),
            ],
            &uplink,
        ),
    )
    .await;
    assert_eq!(status, 204);

    // The echo backend returns the same bytes, so both directions must land on
    // exactly the payload length once the poll has drained them.
    let mut echoed = Vec::new();
    let mut cursor = 0u64;
    for _ in 0..40 {
        let (status, headers, body) = http_request(
            fixture.address,
            &post(
                "/api/v1/down",
                &[
                    ("Authorization", &format!("Bearer {token}")),
                    ("X-Down-Cursor", &cursor.to_string()),
                ],
                &[],
            ),
        )
        .await;
        cursor = header_value(&headers, "x-down-cursor")
            .and_then(|value| value.parse().ok())
            .unwrap_or(cursor);
        if status == 200 {
            echoed.extend_from_slice(&data_payloads(&body, 1));
        }
        if echoed.len() >= payload.len() {
            break;
        }
    }
    assert_eq!(echoed, payload);

    let after = fixture.manager.snapshot();
    assert_eq!(after.stream_bytes_up, payload.len() as u64);
    assert_eq!(after.stream_bytes_down, payload.len() as u64);
    // The carrier moved strictly more than the payload: frame headers, the
    // OPEN, and the WINDOW grant all count there and nowhere else.
    assert!(
        after.bytes_up > after.stream_bytes_up,
        "carrier uplink {} must exceed payload {}",
        after.bytes_up,
        after.stream_bytes_up
    );
    assert!(
        after.bytes_down > after.stream_bytes_down,
        "carrier downlink {} must exceed payload {}",
        after.bytes_down,
        after.stream_bytes_down
    );
}
