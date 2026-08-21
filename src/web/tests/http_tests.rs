//! End-to-end HTTP tests driving the relay over a real TCP listener.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;

use crate::config::{CarrierMode, WebBackend, WebLimits, WebTimeouts};
use crate::crypto::SecureRandom;
use crate::web::capability::{derive_capability, encode_token};
use crate::web::frame::{self, FrameType};
use crate::web::http::Relay;
use crate::web::listener;
use crate::web::site::StaticSite;

use super::harness::{
    TEST_HOST, TEST_SECRET, batch, build_manager, data_payloads, header_value, http_request,
    start_echo_backend,
};

struct Fixture {
    address: SocketAddr,
    _site: TempDir,
}

async fn start_relay(carrier: CarrierMode) -> Fixture {
    let backend = start_echo_backend().await;
    let manager = build_manager(WebBackend::Loopback(backend), carrier, WebLimits::default());
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(directory.path().join("index.html"), b"<h1>site</h1>").expect("index");
    std::fs::write(directory.path().join("404.html"), b"<h1>missing</h1>").expect("404");
    let site = StaticSite::load(directory.path()).expect("site");
    let relay = Arc::new(Relay {
        hostname: TEST_HOST.to_string(),
        manager,
        limits: WebLimits::default(),
        timeouts: WebTimeouts::default(),
        site: Some(site),
        upstream: None,
        trusted_proxies: vec!["127.0.0.0/8".parse().expect("cidr")],
        rng: Arc::new(SecureRandom::new()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind relay");
    let address = listener.local_addr().expect("relay addr");
    tokio::spawn(listener::serve_carrier(listener, relay));
    Fixture {
        address,
        _site: directory,
    }
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
    let fixture = start_relay(CarrierMode::Https).await;
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
    let fixture = start_relay(CarrierMode::WebsocketLanes).await;
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
    let fixture = start_relay(CarrierMode::Https).await;
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_host_and_bad_sequences_are_refused() {
    let fixture = start_relay(CarrierMode::Https).await;
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
