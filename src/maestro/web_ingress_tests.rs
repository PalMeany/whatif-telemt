//! Telemt's own WEB transport, driven through this fork's accept loop.
//!
//! Upstream binds that transport with a listener subsystem this fork replaced,
//! so the dispatch in `listeners::spawn_tcp_accept_loops` and the ownership in
//! `WebIngress` are the fork's own code. Everything downstream of them is
//! upstream's and has its own tests; what is untested without this file is that
//! a connection accepted on a `transport = "web"` listener reaches upstream's
//! HTTP service at all, with the vhost routing and the client-address policy
//! the listener was configured with.

use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use super::listeners::{BoundListener, spawn_tcp_accept_loops};
use super::web_ingress::WebIngress;
use crate::config::{ListenerTransport, ProxyConfig, WebClientIpSource};
use crate::maestro::generation::test_runtime_generation;

/// Body served by the decoy site, so a response can be attributed to it.
const DECOY_BODY: &str = "<!doctype html><title>ordinary site</title>";

/// Hostname the vhost answers for.
const VHOST: &str = "proxy.example.com";

/// Builds a configuration with telemt's WEB transport enabled on one vhost.
///
/// The decoy is a static directory rather than an HTTP upstream so the test
/// needs no second server to prove which side answered.
fn web_config(site: &std::path::Path) -> ProxyConfig {
    let toml = format!(
        r#"
[access.users]
alice = "000102030405060708090a0b0c0d0e0f"

[web]
enabled = true
carrier = "https"

[[web.vhosts]]
host = "{VHOST}"
public_addr = "203.0.113.10:443"

[web.vhosts.decoy]
mode = "static_directory"
directory = "{}"

[[web.vhosts.profiles]]
user = "alice"
secret_mode = "dd"
"#,
        site.display()
    );
    let mut config: ProxyConfig = toml::from_str(&toml).expect("the fixture must parse");
    config
        .rebuild_runtime_user_auth()
        .expect("the fixture has one valid user");
    config
        .rebuild_runtime_web()
        .expect("the fixture has one valid vhost");
    config
}

/// Writes the decoy site the vhost serves.
fn write_site() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temp directory must be available");
    let mut index = std::fs::File::create(dir.path().join("index.html"))
        .expect("the decoy index must be writable");
    index
        .write_all(DECOY_BODY.as_bytes())
        .expect("the decoy index must be writable");
    dir
}

/// Binds one loopback WEB listener described exactly as `bind_listeners` would.
async fn bound_web_listener() -> (BoundListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port must be available");
    let addr = listener
        .local_addr()
        .expect("a bound listener has an address");
    (
        BoundListener {
            listener,
            proxy_protocol: false,
            tls_response_fragment_size: None,
            transport: ListenerTransport::Web,
            web_client_ip_source: WebClientIpSource::XForwardedFor,
            web_trusted_proxy_cidrs: Arc::from(vec![
                "127.0.0.0/8".parse().expect("a valid loopback network"),
            ]),
        },
        addr,
    )
}

/// Sends one request and reads whatever the listener answers.
async fn request(addr: SocketAddr, head: &str) -> String {
    let mut stream = TcpStream::connect(addr)
        .await
        .expect("the WEB listener must accept a connection");
    stream
        .write_all(head.as_bytes())
        .await
        .expect("the request must be writable");
    let mut response = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("the WEB listener must answer within the test budget");
    read.expect("the response must be readable");
    String::from_utf8_lossy(&response).into_owned()
}

#[tokio::test]
async fn a_web_listener_answers_the_decoy_site_through_the_forks_accept_loop() {
    // The whole point of the wiring: a connection accepted on a
    // `transport = "web"` listener must reach upstream's HTTP service, not the
    // MTProto handshake the same loop runs for every other transport.
    let site = write_site();
    let config = web_config(site.path());
    let generation = test_runtime_generation(1, config.clone());
    let active_runtime = Arc::new(ArcSwap::from(generation));

    let (bound, addr) = bound_web_listener().await;
    let mut ingress = WebIngress::new(&config);
    let web_runtime = ingress.start(std::slice::from_ref(&bound), active_runtime.clone());
    assert!(
        web_runtime.is_some(),
        "a bound WEB listener must start the process runtime"
    );

    let shutdown = CancellationToken::new();
    spawn_tcp_accept_loops(
        vec![bound],
        active_runtime.clone(),
        shutdown.clone(),
        web_runtime,
    );

    let response = request(
        addr,
        &format!("GET / HTTP/1.1\r\nHost: {VHOST}\r\nConnection: close\r\n\r\n"),
    )
    .await;

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "the decoy site must answer an ordinary request: {response}"
    );
    assert!(
        response.contains(DECOY_BODY),
        "the response must come from the configured decoy: {response}"
    );

    shutdown.cancel();
    ingress.shutdown().await;
}

#[tokio::test]
async fn a_request_for_an_unknown_host_is_refused_rather_than_served() {
    // Vhost routing is the reason the listener carries a hostname at all; a
    // listener that answered every `Host` would serve the decoy to a scanner
    // that never learned the name.
    let site = write_site();
    let config = web_config(site.path());
    let generation = test_runtime_generation(1, config.clone());
    let active_runtime = Arc::new(ArcSwap::from(generation));

    let (bound, addr) = bound_web_listener().await;
    let mut ingress = WebIngress::new(&config);
    let web_runtime = ingress.start(std::slice::from_ref(&bound), active_runtime.clone());
    let shutdown = CancellationToken::new();
    spawn_tcp_accept_loops(
        vec![bound],
        active_runtime.clone(),
        shutdown.clone(),
        web_runtime,
    );

    let response = request(
        addr,
        "GET / HTTP/1.1\r\nHost: someone-else.example\r\nConnection: close\r\n\r\n",
    )
    .await;

    assert!(
        !response.contains(DECOY_BODY),
        "an unknown host must not be served the operator's site: {response}"
    );

    shutdown.cancel();
    ingress.shutdown().await;
}

#[tokio::test]
async fn no_web_listener_leaves_the_process_runtime_unstarted() {
    // `WebIngress` is built before any listener is bound, because the API task
    // needs its handles; it must not start a session manager for a process that
    // never asked for the transport.
    let site = write_site();
    let config = web_config(site.path());
    let generation = test_runtime_generation(1, config.clone());
    let active_runtime = Arc::new(ArcSwap::from(generation));

    let mut ingress = WebIngress::new(&config);
    assert!(ingress.start(&[], active_runtime).is_none());
    ingress.shutdown().await;
}

#[tokio::test]
async fn the_fork_implementation_selector_keeps_the_telemt_transport_down() {
    // `fork.web_implementation = "fork"` is refused at load time when a WEB
    // listener is configured, but the runtime must not depend on that check
    // having run: it is the last place the decision is enforced.
    let site = write_site();
    let mut config = web_config(site.path());
    config.fork.web_implementation = crate::config::WebImplementation::Fork;
    let generation = test_runtime_generation(1, config.clone());
    let active_runtime = Arc::new(ArcSwap::from(generation));

    let (bound, _addr) = bound_web_listener().await;
    let mut ingress = WebIngress::new(&config);

    assert!(
        ingress
            .start(std::slice::from_ref(&bound), active_runtime)
            .is_none(),
        "the selector must keep telemt's transport down"
    );
    ingress.shutdown().await;
}
