use std::collections::BTreeMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use base64::Engine as _;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use super::serve_connection;
use crate::config::{
    ProxyConfig, WebClientIpSource, WebRuntimeConfig, WebRuntimeDecoy,
    WebRuntimeProfile, WebRuntimeVhost, WebSecretMode, WebStaticAsset, WebStaticSite,
};
use crate::maestro::generation::test_runtime_generation;
use crate::web::frame::{self, FrameType};
use crate::web::manager::WebProcessRuntime;

fn runtime_config(capability: [u8; 32]) -> ProxyConfig {
    let profile = Arc::new(WebRuntimeProfile {
        host: "proxy.example.com".to_string(),
        public_addr: "203.0.113.10:443".parse().unwrap(),
        user: "alice".to_string(),
        secret_mode: WebSecretMode::Plain,
        capability,
        max_sessions: 4,
        max_streams: 16,
        max_streams_per_session: 4,
    });
    let mut assets = BTreeMap::new();
    assets.insert(
        "/index.html".to_string(),
        WebStaticAsset {
            body: Bytes::from_static(b"<!doctype html><title>decoy</title>"),
            content_type: "text/html; charset=utf-8",
            etag: "\"test\"".to_string(),
        },
    );
    let site = Arc::new(WebStaticSite {
        assets,
        index: "index.html".to_string(),
    });
    let vhost = Arc::new(WebRuntimeVhost {
        host: "proxy.example.com".to_string(),
        decoy: WebRuntimeDecoy::StaticDirectory(Arc::clone(&site)),
        decoy_header_secs: 1,
        profiles: vec![Arc::clone(&profile)],
    });
    let mut vhosts = BTreeMap::new();
    vhosts.insert("proxy.example.com".to_string(), vhost);
    vhosts.insert(
        "other.example.com".to_string(),
        Arc::new(WebRuntimeVhost {
            host: "other.example.com".to_string(),
            decoy: WebRuntimeDecoy::StaticDirectory(site),
            decoy_header_secs: 1,
            profiles: Vec::new(),
        }),
    );
    let mut config = ProxyConfig::default();
    config.web.enabled = true;
    config.web.limits.max_bootstraps_per_ip = 1;
    config.web.timeouts.shutdown_secs = 1;
    config.web.runtime = Some(Arc::new(WebRuntimeConfig {
        vhosts,
        profiles: vec![profile],
    }));
    config
}

async fn request(
    listener: &TcpListener,
    runtime: &Arc<WebProcessRuntime>,
    request: Vec<u8>,
) -> Vec<u8> {
    let addr = listener.local_addr().unwrap();
    let (accepted, client) = tokio::join!(listener.accept(), TcpStream::connect(addr));
    let (server, peer) = accepted.unwrap();
    let mut client = client.unwrap();
    let permit = runtime.try_http_connection().unwrap();
    let task = tokio::spawn(serve_connection(
        server,
        peer,
        WebClientIpSource::XForwardedFor,
        Arc::from(["127.0.0.1/32".parse().unwrap()]),
        Arc::clone(runtime),
        CancellationToken::new(),
        permit,
    ));
    client.write_all(&request).await.unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    task.await.unwrap();
    response
}

fn split_response(response: &[u8]) -> (&[u8], &[u8]) {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    (&response[..separator], &response[separator + 4..])
}

fn response_header<'a>(headers: &'a [u8], name: &str) -> &'a str {
    std::str::from_utf8(headers)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find_map(|(header, value)| header.eq_ignore_ascii_case(name).then_some(value.trim()))
        .unwrap()
}

#[tokio::test]
async fn https_carrier_bootstraps_and_closes_one_session() {
    let capability = [7u8; 32];
    let generation = test_runtime_generation(1, runtime_config(capability));
    let active_runtime = Arc::new(ArcSwap::from(Arc::clone(&generation)));
    let runtime = WebProcessRuntime::start(Arc::clone(&active_runtime));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(capability);
    let wrong_family = format!(
        "GET /?bridge={encoded} HTTP/1.1\r\nHost: proxy.example.com\r\nX-Forwarded-For: 2001:db8::10\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    let wrong_family_response = request(&listener, &runtime, wrong_family).await;
    let (_, wrong_family_body) = split_response(&wrong_family_response);
    assert!(!wrong_family_body
        .windows(11)
        .any(|value| value == b"bootstrap='"));
    let root = format!(
        "GET /?bridge={encoded} HTTP/1.1\r\nHost: proxy.example.com\r\nX-Forwarded-For: 192.0.2.10\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    let root_response = request(&listener, &runtime, root).await;
    let (root_headers, root_body) = split_response(&root_response);
    assert!(root_headers.starts_with(b"HTTP/1.1 200"));
    let root_body = std::str::from_utf8(root_body).unwrap();
    let bootstrap = root_body
        .split_once("bootstrap='")
        .and_then(|(_, suffix)| suffix.split_once('\''))
        .map(|(token, _)| token)
        .unwrap();
    assert_eq!(bootstrap.len(), 43);

    let hello = frame::encode(FrameType::Hello, 0, &[1]);
    let mut wrong_host = format!(
        "POST /api/v1/session HTTP/1.1\r\nHost: other.example.com\r\nX-Forwarded-For: 192.0.2.10\r\nAuthorization: Bearer {bootstrap}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        hello.len()
    )
    .into_bytes();
    wrong_host.extend_from_slice(&hello);
    let wrong_host_response = request(&listener, &runtime, wrong_host).await;
    assert!(wrong_host_response.starts_with(b"HTTP/1.1 404"));

    let mut create = format!(
        "POST /api/v1/session HTTP/1.1\r\nHost: proxy.example.com\r\nX-Forwarded-For: 192.0.2.10\r\nAuthorization: Bearer {bootstrap}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        hello.len()
    )
    .into_bytes();
    let create_retry = create.clone();
    create.extend_from_slice(&hello);
    let mut create_retry = create_retry;
    create_retry.extend_from_slice(&hello);
    let create_response = request(&listener, &runtime, create).await;
    let (create_headers, create_body) = split_response(&create_response);
    assert!(create_headers.starts_with(b"HTTP/1.1 200"));
    assert_eq!(response_header(create_headers, "x-carrier-mode"), "https");
    assert_eq!(create_body, frame::encode(FrameType::Welcome, 0, &[]));
    let session = response_header(create_headers, "x-session-token");
    assert_eq!(session.len(), 43);

    let replacement = test_runtime_generation(2, runtime_config(capability));
    active_runtime.store(Arc::clone(&replacement));
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let retry_response = request(&listener, &runtime, create_retry).await;
    let (retry_headers, retry_body) = split_response(&retry_response);
    assert!(retry_headers.starts_with(b"HTTP/1.1 200"));
    assert_eq!(response_header(retry_headers, "x-session-token"), session);
    assert_eq!(retry_body, frame::encode(FrameType::Welcome, 0, &[]));

    let next_root = format!(
        "GET /?bridge={encoded} HTTP/1.1\r\nHost: proxy.example.com\r\nX-Forwarded-For: 192.0.2.10\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    let next_root_response = request(&listener, &runtime, next_root).await;
    let (_, next_root_body) = split_response(&next_root_response);
    assert!(next_root_body.windows(11).any(|value| value == b"bootstrap='"));

    let close = format!(
        "DELETE /api/v1/session HTTP/1.1\r\nHost: proxy.example.com\r\nX-Forwarded-For: 192.0.2.10\r\nAuthorization: Bearer {session}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    let close_retry = close.clone();
    let close_response = request(&listener, &runtime, close).await;
    assert!(close_response.starts_with(b"HTTP/1.1 204"));
    let close_retry_response = request(&listener, &runtime, close_retry).await;
    assert!(close_retry_response.starts_with(b"HTTP/1.1 204"));

    runtime.shutdown().await;
    generation.stop_sessions().await;
    generation.stop_background_tasks().await;
    replacement.stop_sessions().await;
    replacement.stop_background_tasks().await;
}
