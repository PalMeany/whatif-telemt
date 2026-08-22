//! Shared fixtures for the WEB relay integration tests.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::{
    CarrierMode, ProxyConfig, WebBackend, WebLimits, WebProfileLimits, WebTimeouts,
};
use crate::maestro::generation::test_runtime_generation;
use crate::web::capability::derive_capability;
use crate::web::frame::{self, FrameType};
use crate::web::manager::{Manager, WebProfile};
use crate::web::runtime::WebRuntime;

/// Hostname used by every fixture.
pub(super) const TEST_HOST: &str = "proxy.example.com";

/// Secret whose capability the protocol document pins.
pub(super) const TEST_SECRET: [u8; 16] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
];

/// Starts a loopback echo server used as a stand-in backend.
///
/// The backend is opaque to the relay, so echoing is enough to prove that
/// bytes survive the full uplink, backend, and downlink path.
pub(super) async fn start_echo_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
    let address = listener.local_addr().expect("echo addr");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 16 * 1024];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => {
                            if stream.write_all(&buffer[..read]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    address
}

/// Builds a manager whose single profile points at `backend`.
pub(super) fn build_manager(
    backend: WebBackend,
    carrier: CarrierMode,
    limits: WebLimits,
) -> Arc<Manager> {
    let mut config = ProxyConfig::default();
    config
        .access
        .users
        .insert("tester".to_string(), hex::encode(TEST_SECRET));
    build_manager_with_config(config, backend, carrier, limits)
}

/// Builds a manager over a caller-supplied proxy configuration.
pub(super) fn build_manager_with_config(
    config: ProxyConfig,
    backend: WebBackend,
    carrier: CarrierMode,
    limits: WebLimits,
) -> Arc<Manager> {
    build_manager_with_stats(config, backend, carrier, limits).0
}

/// Builds a manager and hands back the runtime statistics it feeds, so a test
/// can tell an accepted handshake from a rejected one.
pub(super) fn build_manager_with_stats(
    mut config: ProxyConfig,
    backend: WebBackend,
    carrier: CarrierMode,
    limits: WebLimits,
) -> (Arc<Manager>, Arc<crate::stats::Stats>) {
    config
        .rebuild_runtime_user_auth()
        .expect("user auth snapshot");
    let generation = test_runtime_generation(1, config);
    let stats = generation.stats.clone();
    let runtime = WebRuntime::new(Arc::new(arc_swap::ArcSwap::from(generation)));
    let profile = Arc::new(WebProfile {
        name: "tester".to_string(),
        backend,
        carrier,
        capabilities: vec![derive_capability(TEST_HOST, &TEST_SECRET)],
        limits: WebProfileLimits::default().with_defaults(&limits),
    });
    let manager =
        Manager::new(limits, WebTimeouts::default(), vec![profile], runtime).expect("manager");
    (manager, stats)
}

/// Encodes one frame batch.
pub(super) fn batch(frames: &[(FrameType, u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (kind, id, payload) in frames {
        frame::encode_into(&mut out, *kind, *id, payload);
    }
    out
}

/// Collects every DATA payload of one stream from a downlink batch.
pub(super) fn data_payloads(body: &[u8], stream_id: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for value in frame::parse_all(body, frame::MAX_PAYLOAD).expect("parse downlink") {
        if value.kind == FrameType::DATA && value.stream_id == stream_id {
            out.extend_from_slice(value.payload);
        }
    }
    out
}

/// True when the batch carries a CLOSE for the stream.
pub(super) fn has_close(body: &[u8], stream_id: u32) -> bool {
    frame::parse_all(body, frame::MAX_PAYLOAD)
        .map(|frames| {
            frames
                .iter()
                .any(|value| value.kind == FrameType::CLOSE && value.stream_id == stream_id)
        })
        .unwrap_or(false)
}

/// Sends one raw HTTP request and returns the status line, headers, and body.
pub(super) async fn http_request(
    address: SocketAddr,
    request: &[u8],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut stream = TcpStream::connect(address).await.expect("connect relay");
    stream.write_all(request).await.expect("write request");
    let mut raw = Vec::new();
    let mut buffer = vec![0u8; 8192];
    loop {
        let read = stream.read(&mut buffer).await.expect("read response");
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        if let Some(split) = find_header_end(&raw) {
            let (status, headers) = parse_head(&raw[..split]);
            let length = headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .and_then(|(_, value)| value.parse::<usize>().ok())
                .unwrap_or(0);
            if raw.len() - split >= length {
                return (status, headers, raw[split..split + length].to_vec());
            }
        }
    }
    let split = find_header_end(&raw).unwrap_or(raw.len());
    let (status, headers) = parse_head(&raw[..split]);
    (status, headers, raw[split..].to_vec())
}

/// Reads one header value from a parsed response.
pub(super) fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn parse_head(raw: &[u8]) -> (u16, Vec<(String, String)>) {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    (status, headers)
}
