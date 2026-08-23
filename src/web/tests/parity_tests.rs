//! Probing parity: a refused reserved path must be indistinguishable from an
//! ordinary miss.
//!
//! This is the property the whole design exists to provide, and it is the one
//! an unauthenticated prober attacks directly. The reference pins it in
//! `internal/server/parity_test.go` by comparing `{status, sorted headers minus
//! Date, body}` across a grid of methods, header decorations, and paths.
//!
//! That fingerprint alone is not enough here. Go's `net/http` consumes a
//! handler-ignored request body on the caller's behalf and applies one read
//! deadline to every path, so its two classes cannot diverge in *connection
//! framing* or in *latency* no matter what the handler does. Hyper hands the
//! body to the handler, so both are ours to get right — and both are
//! observable to a prober who never authenticates. The suite below therefore
//! adds two dimensions the reference does not need:
//!
//! - **reuse**: a large body followed by a pipelined request on one connection.
//!   A path that drops its body unread makes hyper abort mid-body, so the
//!   second response never arrives.
//! - **latency**: a declared `Content-Length` whose bytes never come. A path
//!   that drains under a different deadline than its neighbour answers in a
//!   visibly different time.

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::harness::{
    PublicSite, RelayFixture, TEST_HOST, build_manager, start_echo_backend, start_relay,
};
use crate::config::{CarrierMode, WebBackend, WebLimits, WebTimeouts};

/// The four reserved paths, plus near misses that must answer identically.
const RESERVED: [&str; 4] = [
    "/api/v1/session",
    "/api/v1/up",
    "/api/v1/down",
    "/api/v1/ws",
];

/// Ordinary paths a prober compares the reserved ones against.
const ORDINARY: [&str; 6] = [
    "/nonexistent",
    "/api/nope",
    "/api/v1/nope",
    // A near miss by one character, and the percent-encoded form of a real
    // reserved path. Both must be indistinguishable from an ordinary miss --
    // the second one because the router decodes before it classifies, so it is
    // a reserved path being refused, not a static path being served.
    "/api/v1/upx",
    "/deep/missing/path",
    "/api/v1/%75p",
];

/// Header sets a prober varies while looking for a seam.
fn decorations() -> Vec<(&'static str, Vec<(&'static str, String)>)> {
    vec![
        ("plain", Vec::new()),
        ("origin", vec![("Origin", format!("https://{TEST_HOST}"))]),
        ("cookie", vec![("Cookie", "session=1".to_string())]),
        (
            "upgrade",
            vec![
                ("Connection", "Upgrade".to_string()),
                ("Upgrade", "websocket".to_string()),
                ("Sec-WebSocket-Version", "13".to_string()),
                ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==".to_string()),
                (
                    "Sec-WebSocket-Protocol",
                    format!("tproxy-v1.{}", "A".repeat(43)),
                ),
            ],
        ),
        (
            "encoding",
            vec![("Accept-Encoding", "gzip, br".to_string())],
        ),
        (
            "bearer",
            vec![
                ("Origin", format!("https://{TEST_HOST}")),
                ("Authorization", format!("Bearer {}", "A".repeat(43))),
                ("Content-Type", "application/octet-stream".to_string()),
            ],
        ),
    ]
}

/// The observable shape of one answer: status, sorted headers, body.
#[derive(Debug, PartialEq, Eq)]
struct Shape {
    status: String,
    headers: Vec<String>,
    body: Vec<u8>,
}

async fn start() -> RelayFixture {
    let backend = start_echo_backend().await;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::Https,
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

fn request(
    method: &str,
    target: &str,
    decoration: &[(&'static str, String)],
    body: &[u8],
    close: bool,
) -> Vec<u8> {
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: {TEST_HOST}\r\n");
    for (name, value) in decoration {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    if close {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

/// Splits one raw response into its status line, headers, and body.
fn parse(raw: &[u8]) -> Option<Shape> {
    let split = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.lines();
    let status = lines.next()?.to_string();
    let mut headers: Vec<String> = lines
        .filter(|line| {
            // `Date` is the one header that legitimately differs between two
            // otherwise identical answers, exactly as the reference excludes it.
            !line.to_ascii_lowercase().starts_with("date:")
        })
        .map(|line| line.to_ascii_lowercase())
        .collect();
    headers.sort();
    Some(Shape {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    })
}

/// Performs one request on its own connection and returns the answer's shape.
async fn fingerprint(
    fixture: &RelayFixture,
    method: &str,
    target: &str,
    decoration: &[(&'static str, String)],
) -> Shape {
    let body: &[u8] = if method == "POST" || method == "DELETE" {
        b"payload"
    } else {
        b""
    };
    let mut stream = TcpStream::connect(fixture.address).await.expect("connect");
    stream
        .write_all(&request(method, target, decoration, body, true))
        .await
        .expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    parse(&raw).expect("a well-formed response")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_refused_path_answers_exactly_like_a_static_miss() {
    let fixture = start().await;
    for method in ["GET", "HEAD", "POST", "OPTIONS", "DELETE"] {
        for (name, decoration) in decorations() {
            let reference = fingerprint(&fixture, method, "/nonexistent", &decoration).await;
            assert!(
                reference.status.contains("404"),
                "{method} /nonexistent ({name}) is {}, want 404",
                reference.status
            );
            for target in RESERVED.iter().chain(ORDINARY.iter()) {
                if *target == "/nonexistent" {
                    continue;
                }
                let got = fingerprint(&fixture, method, target, &decoration).await;
                assert_eq!(
                    got, reference,
                    "{method} {target} ({name}) is separable from a static miss"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_root_query_never_separates_a_capability_from_an_ordinary_visit() {
    let fixture = start().await;
    for (name, decoration) in decorations() {
        // Every root query that is not a real capability answers as the index,
        // so a prober cannot tell a malformed capability from an absent one.
        let plain = fingerprint(&fixture, "GET", "/", &decoration).await;
        assert!(plain.status.contains("200"), "GET / ({name}) must be 200");
        for query in [
            "?x=1",
            "?bridge=short",
            &format!("?bridge={}", "A".repeat(43)),
            &format!("?bridge={}&x=1", "A".repeat(43)),
        ] {
            let got = fingerprint(&fixture, "GET", &format!("/{query}"), &decoration).await;
            assert_eq!(
                got.status, plain.status,
                "GET /{query} ({name}) is separable from GET /"
            );
            assert_eq!(
                got.body, plain.body,
                "GET /{query} ({name}) returned a different body"
            );
        }
    }
}

/// Sends a large body, then a pipelined `GET /` on the same connection.
///
/// Returns the number of complete responses that arrived. A path that drops its
/// body unread makes hyper abort the connection mid-body, so the second
/// response never comes back and the count is one instead of two.
async fn pipelined_responses(fixture: &RelayFixture, target: &str) -> usize {
    let body = vec![b'x'; 200 * 1024];
    let mut stream = TcpStream::connect(fixture.address).await.expect("connect");
    stream
        .write_all(&request("POST", target, &[], &body, false))
        .await
        .expect("write body");
    stream
        .write_all(&request("GET", "/", &[], b"", true))
        .await
        .expect("write pipelined");
    let mut raw = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut raw)).await;
    raw.windows(9)
        .filter(|window| window == b"HTTP/1.1 ")
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_body_leaves_the_connection_reusable_on_every_path() {
    let fixture = start().await;
    // This is the dimension the reference gets for free from net/http, and the
    // one a fingerprint of {status, headers, body} cannot see: both classes
    // return a byte-identical 404 while only one of them keeps the connection.
    let reserved = pipelined_responses(&fixture, "/api/v1/up").await;
    let ordinary = pipelined_responses(&fixture, "/nonexistent").await;
    assert_eq!(
        reserved, 2,
        "a refused reserved path must consume its body and stay reusable"
    );
    assert_eq!(
        ordinary, 2,
        "a refused ordinary path must consume its body and stay reusable"
    );
}

/// Times an answer to a request whose declared body never arrives.
async fn stalled_body_latency(fixture: &RelayFixture, target: &str) -> Duration {
    let mut head = format!("POST {target} HTTP/1.1\r\nHost: {TEST_HOST}\r\n");
    head.push_str("Content-Length: 100000\r\n\r\n");
    let started = Instant::now();
    let mut stream = TcpStream::connect(fixture.address).await.expect("connect");
    stream.write_all(head.as_bytes()).await.expect("write head");
    stream.write_all(b"ten-bytes.").await.expect("write some");
    let mut raw = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut raw)).await;
    started.elapsed()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_body_is_answered_in_the_same_time_band_on_every_path() {
    let fixture = start().await;
    // The second invisible dimension. Before the refusal drain was shared, a
    // reserved path waited out the full body deadline while an ordinary one
    // answered in microseconds — four requests and a stopwatch enumerated the
    // reserved set with no credential at all.
    let reserved = stalled_body_latency(&fixture, "/api/v1/up").await;
    let ordinary = stalled_body_latency(&fixture, "/nonexistent").await;
    let (slower, faster) = if reserved > ordinary {
        (reserved, ordinary)
    } else {
        (ordinary, reserved)
    };
    // A generous band: the point is that neither path waits on a deadline the
    // other does not, not that scheduling noise is absent.
    assert!(
        slower.saturating_sub(faster) < Duration::from_secs(1),
        "reserved answered in {reserved:?} and ordinary in {ordinary:?}: the gap is a \
         credential-free oracle for the reserved path set"
    );
}
