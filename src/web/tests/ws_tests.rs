//! End-to-end tests for the WebSocket carriers.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::{CarrierMode, WebBackend, WebLimits, WebTimeouts};
use crate::protocol::ProtoTag;
use crate::web::capability::{derive_capability, encode_token};
use crate::web::frame::{self, FrameType};

use super::harness::{
    PublicSite, RelayFixture, TEST_HOST, TEST_SECRET, batch, build_manager,
    build_manager_with_stats, data_payloads, header_value, http_request, start_echo_backend,
    start_relay,
};
use super::internal_backend_tests::{authenticating_handshake, secure_mode_config};

/// Client key from the RFC example; the accept value is checked against it.
const CLIENT_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

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

/// Sends one masked frame with the given opcode and fin bit.
async fn send_frame(stream: &mut TcpStream, opcode: u8, fin: bool, payload: &[u8]) {
    let mask = [0x11u8, 0x22, 0x33, 0x44];
    let mut out = vec![if fin { 0x80 | opcode } else { opcode }];
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

/// Sends one masked binary message.
async fn send_binary(stream: &mut TcpStream, payload: &[u8]) {
    send_frame(stream, 0x2, true, payload).await;
}

/// Reads server frames until a pong arrives, returning its payload.
async fn read_pong(stream: &mut TcpStream) -> Option<Vec<u8>> {
    for _ in 0..40 {
        let (opcode, payload) = read_frame(stream).await?;
        match opcode {
            0xA => return Some(payload),
            0x8 => return None,
            _ => continue,
        }
    }
    None
}

/// Reads one server frame, returning its opcode and unmasked payload.
async fn read_frame(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; 2];
    if tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut header))
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
    Some((opcode, payload))
}

/// Reads server frames until a binary message arrives, ignoring pings.
async fn read_binary(stream: &mut TcpStream) -> Option<Vec<u8>> {
    for _ in 0..40 {
        let (opcode, payload) = read_frame(stream).await?;
        match opcode {
            0x2 => return Some(payload),
            0x8 => return None,
            _ => continue,
        }
    }
    None
}

/// Reads server frames until the close frame arrives, returning its code.
async fn read_close_code(stream: &mut TcpStream) -> Option<u16> {
    for _ in 0..40 {
        let (opcode, payload) = read_frame(stream).await?;
        if opcode == 0x8 {
            if payload.len() < 2 {
                return None;
            }
            return Some(u16::from_be_bytes([payload[0], payload[1]]));
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiplexed_websocket_carrier_round_trip() {
    let fixture = start_fixture(CarrierMode::Websocket).await;
    let address = fixture.address;
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
    let fixture = start_fixture(CarrierMode::WebsocketLanes).await;
    let address = fixture.address;
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
    let fixture = start_fixture(CarrierMode::Websocket).await;
    let address = fixture.address;
    let token = create_session(address).await;
    let request = format!(
        "GET /api/v1/ws HTTP/1.1\r\nHost: {TEST_HOST}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {CLIENT_KEY}\r\nSec-WebSocket-Protocol: tproxy-lane-v1.{token}.1\r\nConnection: close\r\n\r\n"
    );
    let (status, _, _) = http_request(address, request.as_bytes()).await;
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_ping_is_echoed_and_the_carrier_survives() {
    let fixture = start_fixture(CarrierMode::Websocket).await;
    let address = fixture.address;
    let token = create_session(address).await;
    let mut socket = open_socket(address, &format!("tproxy-v1.{token}")).await;

    send_frame(&mut socket, 0x9, true, b"liveness").await;
    assert_eq!(
        read_pong(&mut socket).await.as_deref(),
        Some(b"liveness".as_ref()),
        "a ping must be answered with the same payload"
    );

    // The carrier has to keep working after the echo, which is the part the
    // untimed pong write used to be able to wedge.
    let payload = b"after-ping".to_vec();
    send_binary(
        &mut socket,
        &batch(&[
            (FrameType::OPEN, 1, Vec::new()),
            (FrameType::DATA, 1, payload.clone()),
        ]),
    )
    .await;
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
async fn a_ping_between_fragments_does_not_lose_the_message() {
    let fixture = start_fixture(CarrierMode::Websocket).await;
    let address = fixture.address;
    let token = create_session(address).await;
    let mut socket = open_socket(address, &format!("tproxy-v1.{token}")).await;

    // RFC 6455 §5.4 permits a control frame between the fragments of a message,
    // and the relay pings on every idle poll, so its own client can produce
    // exactly this interleaving.
    let payload = b"fragmented-payload".to_vec();
    let uplink = batch(&[
        (FrameType::OPEN, 1, Vec::new()),
        (FrameType::DATA, 1, payload.clone()),
    ]);
    let split = uplink.len() / 2;
    send_frame(&mut socket, 0x2, false, &uplink[..split]).await;
    send_frame(&mut socket, 0x9, true, b"mid").await;
    send_frame(&mut socket, 0x0, true, &uplink[split..]).await;

    assert_eq!(
        read_pong(&mut socket).await.as_deref(),
        Some(b"mid".as_ref())
    );
    let mut echoed = Vec::new();
    while echoed.len() < payload.len() {
        let Some(message) = read_binary(&mut socket).await else {
            break;
        };
        echoed.extend_from_slice(&data_payloads(&message, 1));
    }
    assert_eq!(
        echoed, payload,
        "the fragments around the ping must still reassemble"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_binary_message_tears_down_the_carrier() {
    // Every binary message on this carrier is a bounded batch of complete
    // frames, so a batch of none is a grammar violation, exactly as the
    // reference treats it. Keepalive is a protocol Ping, which is why nothing
    // legitimate lands here.
    let fixture = start_fixture(CarrierMode::Websocket).await;
    let address = fixture.address;
    let token = create_session(address).await;
    let mut socket = open_socket(address, &format!("tproxy-v1.{token}")).await;

    send_binary(&mut socket, b"").await;

    // The carrier ends, and it ends with a protocol close rather than a
    // normal one: a client that respects close codes must be told it may
    // reconnect instead of that the session finished cleanly.
    let closed = read_close_code(&mut socket).await;
    assert_eq!(closed, Some(1002));
}

/// Starts a relay whose streams terminate in this process, like a deployment.
async fn start_internal_fixture(carrier: CarrierMode) -> RelayFixture {
    let (manager, _) = build_manager_with_stats(
        secure_mode_config(),
        WebBackend::Internal,
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

/// Reads whatever the relay sends next, reporting the opcode it arrived on.
///
/// A carrier that is working answers a handshake with either data or a CLOSE
/// for the stream; one that is broken sends a WebSocket close, or nothing.
async fn next_carrier_event(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    for _ in 0..40 {
        let (opcode, payload) = read_frame(stream).await?;
        match opcode {
            0x2 | 0x8 => return Some((opcode, payload)),
            _ => continue,
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_handshake_survives_a_lane_websocket() {
    // The mode a lane deployment actually runs, driven by a handshake the
    // proxy accepts, over a real socket. The echo-backend lane test proves
    // bytes move; this proves a client could ever get through.
    let fixture = start_internal_fixture(CarrierMode::WebsocketLanes).await;
    let address = fixture.address;
    let token = create_session(address).await;
    let mut socket = open_socket(address, &format!("tproxy-lane-v1.{token}.1")).await;

    let handshake = authenticating_handshake(&TEST_SECRET, ProtoTag::Secure);
    send_binary(
        &mut socket,
        &batch(&[
            (FrameType::OPEN, 1, Vec::new()),
            (FrameType::DATA, 1, handshake.to_vec()),
        ]),
    )
    .await;

    let event = next_carrier_event(&mut socket).await;
    assert!(
        event.is_some(),
        "the lane carrier answered nothing at all: a client would sit on \"connecting\" here"
    );
    let (opcode, payload) = event.expect("carrier event");
    assert_eq!(
        opcode, 0x2,
        "the relay closed the lane socket instead of carrying the stream"
    );
    assert!(
        !payload.is_empty(),
        "the lane delivered an empty carrier message"
    );
}
