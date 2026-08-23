//! End-to-end tests for application mode (`web.public_upstream`).
//!
//! Application mode is the deployment where the operator keeps their own web
//! framework behind the relay, and it is where the cover story is easiest to
//! break: the relay must answer every non-carrier request exactly as the
//! application would, including for a mismatched `Host` and for the four
//! reserved paths when they are not authenticated.

use std::net::SocketAddr;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::{CarrierMode, WebBackend, WebLimits, WebTimeouts};

use super::harness::{
    PublicSite, RelayFixture, TEST_HOST, build_manager, header_value, http_request,
    start_echo_backend, start_relay,
};

/// Body the stand-in application returns for every path.
const APP_BODY: &[u8] = b"<html><body>operator application</body></html>";

/// One request as the stand-in application saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenRequest {
    method: String,
    path: String,
    body_len: usize,
    /// Lowercased header names, sorted. Enough to prove that a carrier bearer
    /// and the transport headers never reach the operator's application.
    header_names: Vec<String>,
}

/// Requests observed by the stand-in application, newest last.
type Seen = Arc<Mutex<Vec<SeenRequest>>>;

/// Starts a loopback HTTP application that answers 404 for every path.
///
/// It reads the declared body in full before responding, exactly like a real
/// framework, so a truncated body shows up as a connection error rather than
/// being silently absorbed.
async fn start_application() -> (SocketAddr, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind app");
    let address = listener.local_addr().expect("app addr");
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let sink = sink.clone();
            tokio::spawn(async move {
                serve_application_connection(stream, sink).await;
            });
        }
    });
    (address, seen)
}

async fn serve_application_connection(mut stream: TcpStream, seen: Seen) {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 1024];
    let head_end = loop {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            return;
        }
        raw.extend_from_slice(&buffer[..read]);
        if let Some(position) = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        {
            break position;
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let declared = head
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = raw[head_end..].to_vec();
    while body.len() < declared {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..read]);
    }
    let mut header_names: Vec<String> = head
        .split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, _)| name.trim().to_ascii_lowercase())
        .collect();
    header_names.sort();
    seen.lock().push(SeenRequest {
        method,
        path,
        body_len: body.len(),
        header_names,
    });
    let response = format!(
        "HTTP/1.1 404 Not Found\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        APP_BODY.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.write_all(APP_BODY).await;
    let _ = stream.flush().await;
}

/// Starts a relay in application mode over the given loopback application.
async fn start_application_relay(application: SocketAddr) -> RelayFixture {
    let backend = start_echo_backend().await;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::Https,
        WebLimits::default(),
    );
    start_relay(
        manager,
        PublicSite::Upstream(application),
        WebLimits::default(),
        WebTimeouts::default(),
    )
    .await
}

fn post_with_body(path: &str, host: &str, body: &[u8]) -> Vec<u8> {
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    request
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_paths_are_answered_by_the_application() {
    let (application, seen) = start_application().await;
    let fixture = start_application_relay(application).await;

    let request = format!("GET /about HTTP/1.1\r\nHost: {TEST_HOST}\r\nConnection: close\r\n\r\n")
        .into_bytes();
    let (status, headers, body) = http_request(fixture.address, &request).await;
    assert_eq!(status, 404);
    assert_eq!(body, APP_BODY);
    assert_eq!(
        header_value(&headers, "content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        seen.lock().last().map(|value| value.path.clone()),
        Some("/about".to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mismatched_host_is_answered_by_the_application() {
    let (application, seen) = start_application().await;
    let fixture = start_application_relay(application).await;

    // The relay must not answer this itself. A fixed body here would identify
    // the origin from one unauthenticated request with any other Host.
    let request =
        b"GET / HTTP/1.1\r\nHost: unrelated.example.net\r\nConnection: close\r\n\r\n".to_vec();
    let (status, _, body) = http_request(fixture.address, &request).await;
    assert_eq!(status, 404);
    assert_eq!(
        body, APP_BODY,
        "the wrong-Host answer must come from the application"
    );
    assert_eq!(seen.lock().len(), 1, "the application must see the request");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reserved_path_answers_exactly_like_a_neighbouring_path() {
    let (application, seen) = start_application().await;
    let fixture = start_application_relay(application).await;

    // One reserved path and one ordinary path, same method, same declared body.
    // Any difference between the two answers is an oracle for the four paths
    // the transport reserves.
    let payload = vec![0x37u8; 64 * 1024];
    let reserved = http_request(
        fixture.address,
        &post_with_body("/api/v1/up", TEST_HOST, &payload),
    )
    .await;
    let ordinary = http_request(
        fixture.address,
        &post_with_body("/api/v1/upx", TEST_HOST, &payload),
    )
    .await;

    assert_eq!(reserved.0, ordinary.0);
    assert_eq!(reserved.2, ordinary.2);
    assert_eq!(reserved.2, APP_BODY);
    assert_eq!(
        header_value(&reserved.1, "content-length"),
        header_value(&ordinary.1, "content-length")
    );
    // The relay strips the body before delegating a reserved path, but it has
    // to read it off the wire first; only the ordinary path forwards it.
    let observed = seen.lock().clone();
    assert_eq!(observed.len(), 2);
    assert_eq!(observed[0].method, "POST");
    assert_eq!(
        observed[0].body_len, 0,
        "a reserved path is delegated bodyless"
    );
    assert_eq!(observed[1].body_len, payload.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_application_answers_without_a_banner() {
    // Binding and dropping yields an address nothing listens on.
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        listener.local_addr().expect("addr")
    };
    let fixture = start_application_relay(closed).await;

    let request =
        format!("GET / HTTP/1.1\r\nHost: {TEST_HOST}\r\nConnection: close\r\n\r\n").into_bytes();
    let (status, headers, body) = http_request(fixture.address, &request).await;
    assert_eq!(status, 502);
    assert!(body.is_empty(), "a 502 must not carry a telemt banner");
    assert_eq!(header_value(&headers, "content-length"), Some("0"));
    assert_eq!(header_value(&headers, "content-type"), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bridge_capability_never_reaches_the_application() {
    let (application, seen) = start_application().await;
    let fixture = start_application_relay(application).await;

    let capability = crate::web::capability::encode_token(
        &crate::web::capability::derive_capability(TEST_HOST, &super::harness::TEST_SECRET),
    );
    let request = format!(
        "GET /?bridge={capability} HTTP/1.1\r\nHost: {TEST_HOST}\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    let (status, _, body) = http_request(fixture.address, &request).await;
    assert_eq!(status, 200, "a valid capability is served the bridge page");
    assert_ne!(body, APP_BODY);
    assert!(
        seen.lock().is_empty(),
        "a capability request must never be delegated to the application"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mismatched_host_never_hands_the_application_a_carrier_bearer() {
    let (application, seen) = start_application().await;
    let fixture = start_application_relay(application).await;

    // The `Host` check runs before the request target is classified, so a
    // reserved path with the wrong `Host` used to reach the plain forwarder,
    // which strips only hop-by-hop headers. That handed the operator's own
    // application a live session bearer, the carrier headers, and the opaque
    // MTProto body -- from an unauthenticated request that chose the `Host`.
    let payload = vec![0x5au8; 4096];
    let mut request = format!(
        "POST /api/v1/up HTTP/1.1\r\nHost: unrelated.example.net\r\n\
         Authorization: Bearer {}\r\nContent-Type: application/octet-stream\r\n\
         X-Up-Seq: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        "A".repeat(43),
        payload.len()
    )
    .into_bytes();
    request.extend_from_slice(&payload);

    let (status, _, body) = http_request(fixture.address, &request).await;
    assert_eq!(status, 404);
    assert_eq!(body, APP_BODY);

    let observed = seen.lock().clone();
    assert_eq!(observed.len(), 1);
    assert_eq!(
        observed[0].body_len, 0,
        "the carrier body must not reach the application"
    );
    for forbidden in [
        "authorization",
        "content-type",
        "x-up-seq",
        "x-lane-id",
        "x-down-cursor",
    ] {
        assert!(
            !observed[0]
                .header_names
                .iter()
                .any(|name| name == forbidden),
            "{forbidden} reached the application: {:?}",
            observed[0].header_names
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_percent_encoded_reserved_path_is_still_a_reserved_path() {
    let (application, seen) = start_application().await;
    let fixture = start_application_relay(application).await;

    // The reference classifies on the decoded target, so `/api/v1/%75p` is the
    // uplink it names. Matching the raw target instead let it miss the reserved
    // set and fall through to the *unsanitised* forwarder -- the same leak as a
    // mismatched `Host`, reached by a second route.
    let payload = vec![0x21u8; 2048];
    let mut request = format!(
        "POST /api/v1/%75p HTTP/1.1\r\nHost: {TEST_HOST}\r\n\
         Authorization: Bearer {}\r\nContent-Type: application/octet-stream\r\n\
         X-Up-Seq: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        "A".repeat(43),
        payload.len()
    )
    .into_bytes();
    request.extend_from_slice(&payload);

    let (status, _, body) = http_request(fixture.address, &request).await;
    assert_eq!(status, 404);
    assert_eq!(body, APP_BODY);

    let observed = seen.lock().clone();
    assert_eq!(observed.len(), 1);
    assert_eq!(
        observed[0].body_len, 0,
        "a percent-encoded reserved path must be delegated bodyless"
    );
    assert!(
        !observed[0]
            .header_names
            .iter()
            .any(|name| name == "authorization"),
        "the bearer reached the application: {:?}",
        observed[0].header_names
    );
    // The application still sees the target the client actually asked for.
    assert_eq!(observed[0].path, "/api/v1/%75p");
}
