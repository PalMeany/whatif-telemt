//! End-to-end tests for the WebSocket carriers.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

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

/// Client key from the RFC example; the accept value is checked against it.
const CLIENT_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

async fn start_relay(carrier: CarrierMode) -> (SocketAddr, tempfile::TempDir) {
    let backend = start_echo_backend().await;
    let manager = build_manager(WebBackend::Loopback(backend), carrier, WebLimits::default());
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(directory.path().join("index.html"), b"site").expect("index");
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
    let address = listener.local_addr().expect("addr");
    tokio::spawn(listener::serve_carrier(listener, relay));
    (address, directory)
}

/// Creates a session over HTTP and returns its bearer.
async fn create_session(address: SocketAddr) -> String {
    let bridge = format!(
        "GET /?bridge={} HTTP/1.1\r\nHost: {TEST_HOST}\r\nConnection: close\r\n\r\n",
        encode_token(&derive_capability(TEST_HOST, &TEST_SECRET))
    );
    let (_, _, page) = http_request(address, bridge.as_bytes()).await;
    let text = String::from_utf8_lossy(&page);
    let start = text.find(",bootstrap=\"").expect("bootstrap") + ",bootstrap=\"".len();
    let bootstrap = &text[start..][..text[start..].find('"').expect("end")];

    let hello = frame::encode(FrameType::HELLO, 0, &[1]);
    let mut request = format!(
        "POST /api/v1/session HTTP/1.1\r\nHost: {TEST_HOST}\r\nAuthorization: Bearer {bootstrap}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        hello.len()
    )
    .into_bytes();
    request.extend_from_slice(&hello);
    let (status, headers, _) = http_request(address, &request).await;
    assert_eq!(status, 200);
    header_value(&headers, "x-session-token")
        .expect("session token")
        .to_string()
}

/// Opens one WebSocket carrier and returns the connected stream.
async fn open_socket(address: SocketAddr, protocol: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address).await.expect("connect");
    let request = format!(
        "GET /api/v1/ws HTTP/1.1\r\nHost: {TEST_HOST}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {CLIENT_KEY}\r\nSec-WebSocket-Protocol: {protocol}\r\nOrigin: https://{TEST_HOST}\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write upgrade");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut byte).await.expect("read upgrade");
        assert_ne!(read, 0, "relay closed during upgrade");
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    assert!(text.starts_with("HTTP/1.1 101"), "unexpected head: {text}");
    assert!(text.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
    assert!(text.contains(protocol));
    stream
}

/// Sends one masked binary message.
async fn send_binary(stream: &mut TcpStream, payload: &[u8]) {
    let mask = [0x11u8, 0x22, 0x33, 0x44];
    let mut out = vec![0x82u8];
    if payload.len() < 126 {
        out.push(0x80 | payload.len() as u8);
    } else {
        out.push(0x80 | 126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        out.push(byte ^ mask[index & 3]);
    }
    stream.write_all(&out).await.expect("write frame");
}

/// Reads server frames until a binary message arrives, ignoring pings.
async fn read_binary(stream: &mut TcpStream) -> Option<Vec<u8>> {
    for _ in 0..40 {
        let mut header = [0u8; 2];
        let deadline = Duration::from_secs(2);
        if tokio::time::timeout(deadline, stream.read_exact(&mut header))
            .await
            .is_err()
        {
            return None;
        }
        let opcode = header[0] & 0x0F;
        let length = match header[1] & 0x7F {
            126 => {
                let mut extended = [0u8; 2];
                stream.read_exact(&mut extended).await.ok()?;
                u16::from_be_bytes(extended) as usize
            }
            127 => {
                let mut extended = [0u8; 8];
                stream.read_exact(&mut extended).await.ok()?;
                u64::from_be_bytes(extended) as usize
            }
            small => small as usize,
        };
        let mut payload = vec![0u8; length];
        if length != 0 {
            stream.read_exact(&mut payload).await.ok()?;
        }
        match opcode {
            0x2 => return Some(payload),
            0x8 => return None,
            _ => continue,
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiplexed_websocket_carrier_round_trip() {
    let (address, _site) = start_relay(CarrierMode::Websocket).await;
    let token = create_session(address).await;
    let mut socket = open_socket(address, &format!("tproxy-v1.{token}")).await;

    let payload = b"websocket-payload".to_vec();
    let uplink = batch(&[
        (FrameType::OPEN, 1, Vec::new()),
        (FrameType::DATA, 1, payload.clone()),
    ]);
    send_binary(&mut socket, &uplink).await;

    let mut echoed = Vec::new();
    while echoed.len() < payload.len() {
        let Some(message) = read_binary(&mut socket).await else {
            break;
        };
        echoed.extend_from_slice(&data_payloads(&message, 1));
    }
    assert_eq!(echoed, payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_lanes_carry_one_stream_each() {
    let (address, _site) = start_relay(CarrierMode::WebsocketLanes).await;
    let token = create_session(address).await;

    let mut first = open_socket(address, &format!("tproxy-lane-v1.{token}.1")).await;
    let mut second = open_socket(address, &format!("tproxy-lane-v1.{token}.2")).await;

    send_binary(
        &mut first,
        &batch(&[
            (FrameType::OPEN, 1, Vec::new()),
            (FrameType::DATA, 1, b"lane-one".to_vec()),
        ]),
    )
    .await;
    send_binary(
        &mut second,
        &batch(&[
            (FrameType::OPEN, 2, Vec::new()),
            (FrameType::DATA, 2, b"lane-two".to_vec()),
        ]),
    )
    .await;

    for (socket, stream_id, expected) in [
        (&mut first, 1u32, b"lane-one".as_ref()),
        (&mut second, 2u32, b"lane-two".as_ref()),
    ] {
        let mut collected = Vec::new();
        while collected.len() < expected.len() {
            let Some(message) = read_binary(socket).await else {
                break;
            };
            collected.extend_from_slice(&data_payloads(&message, stream_id));
        }
        assert_eq!(collected, expected);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_rejects_a_carrier_mode_mismatch() {
    let (address, _site) = start_relay(CarrierMode::Websocket).await;
    let token = create_session(address).await;
    let request = format!(
        "GET /api/v1/ws HTTP/1.1\r\nHost: {TEST_HOST}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {CLIENT_KEY}\r\nSec-WebSocket-Protocol: tproxy-lane-v1.{token}.1\r\nConnection: close\r\n\r\n"
    );
    let (status, _, _) = http_request(address, request.as_bytes()).await;
    assert_eq!(status, 404);
}
