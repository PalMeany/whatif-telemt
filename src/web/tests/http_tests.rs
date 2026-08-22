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
    let backend = start_echo_backend().await;
    let manager = build_manager(WebBackend::Loopback(backend), carrier, WebLimits::default());
    start_relay(
        manager,
        PublicSite::Directory,
        WebLimits::default(),
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
