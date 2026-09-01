//! Shared fixtures for the panel integration tests.
//!
//! Every test drives a real listener over loopback rather than calling the
//! router directly: the cookie, the client header, and the CSRF gate are all
//! properties of the HTTP surface, and a harness that bypassed it would prove
//! nothing about them.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::config::{ClusterRole, PanelConfig, ProxyConfig};
use crate::panel::listener;
use crate::panel::state::PanelState;

/// Work factor used by the fixtures.
///
/// Production runs six hundred thousand iterations; a test that paid that on
/// every login would spend its whole budget deriving keys.
const TEST_HASH_ITERATIONS: u32 = 1_000;

/// One running panel plus everything a test needs to talk to it.
pub(super) struct PanelFixture {
    pub(super) address: SocketAddr,
    pub(super) state: Arc<PanelState>,
    pub(super) bootstrap_password: String,
    shutdown: CancellationToken,
    _directory: tempfile::TempDir,
}

impl Drop for PanelFixture {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// A stand-in Control API that records what the panel forwarded to it.
#[derive(Clone)]
pub(super) struct FakeControlApi {
    pub(super) address: SocketAddr,
    pub(super) requests: Arc<parking_lot::Mutex<Vec<RecordedRequest>>>,
}

/// One request the fake Control API received.
#[derive(Clone, Debug)]
pub(super) struct RecordedRequest {
    pub(super) method: String,
    pub(super) target: String,
    pub(super) authorization: Option<String>,
    pub(super) body: Vec<u8>,
}

/// Starts a loopback Control API that answers a fixed success envelope.
pub(super) async fn start_fake_control_api() -> FakeControlApi {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind control api");
    let address = listener.local_addr().expect("control api addr");
    let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let recorder = requests.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let recorder = recorder.clone();
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut buffer = vec![0u8; 8192];
                loop {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    raw.extend_from_slice(&buffer[..read]);
                    let Some(head_end) = find_header_end(&raw) else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
                    let length = header_of(&head, "content-length")
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(0);
                    if raw.len() - head_end < length {
                        continue;
                    }
                    let mut lines = head.split("\r\n");
                    let request_line = lines.next().unwrap_or_default();
                    let mut parts = request_line.split(' ');
                    let method = parts.next().unwrap_or_default().to_string();
                    let target = parts.next().unwrap_or_default().to_string();
                    recorder.lock().push(RecordedRequest {
                        method,
                        target,
                        authorization: header_of(&head, "authorization"),
                        body: raw[head_end..head_end + length].to_vec(),
                    });
                    let body = br#"{"ok":true,"data":{"status":"ok"},"revision":"rev"}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                    let _ = stream.flush().await;
                    return;
                }
            });
        }
    });
    FakeControlApi { address, requests }
}

/// Starts a panel bound to loopback with the given cluster role.
pub(super) async fn start_panel(control: &FakeControlApi, role: ClusterRole) -> PanelFixture {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut config = ProxyConfig::default();
    config.panel = PanelConfig {
        enabled: true,
        listen: "127.0.0.1:0".to_string(),
        data_dir: directory.path().to_string_lossy().into_owned(),
        control_api_url: format!("http://{}", control.address),
        password_hash_iterations: TEST_HASH_ITERATIONS,
        login_max_attempts: 3,
        login_lockout_secs: 60,
        ..PanelConfig::default()
    };
    config.panel.cluster.enabled = role != ClusterRole::Standalone;
    config.panel.cluster.role = role;
    config.panel.cluster.advertise_url = "https://node.example.com".to_string();
    config.server.api.auth_header = "Bearer control-token".to_string();

    let state = PanelState::bootstrap(&config, Some(directory.path()))
        .await
        .expect("panel bootstrap");
    let bootstrap_password = read_bootstrap_password(directory.path());

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind panel");
    let address = listener.local_addr().expect("panel addr");
    let shutdown = CancellationToken::new();
    tokio::spawn(listener::serve(
        listener,
        state.clone(),
        None,
        shutdown.clone(),
    ));
    PanelFixture {
        address,
        state,
        bootstrap_password,
        shutdown,
        _directory: directory,
    }
}

/// Reads the generated first-start password out of the bootstrap file.
fn read_bootstrap_password(directory: &std::path::Path) -> String {
    let path: PathBuf = directory.join("panel-bootstrap.txt");
    let content = std::fs::read_to_string(path).expect("bootstrap credential file");
    content
        .lines()
        .find_map(|line| line.strip_prefix("password: "))
        .expect("password line")
        .trim()
        .to_string()
}

/// One raw HTTP response.
pub(super) struct Response {
    pub(super) status: u16,
    pub(super) headers: Vec<(String, String)>,
    pub(super) body: Vec<u8>,
}

impl Response {
    /// Reads one response header.
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Reads every value of a repeated response header.
    pub(super) fn headers_all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// Decodes the body as JSON.
    pub(super) fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }

    /// Reads `data.<field>` from a panel envelope.
    pub(super) fn data(&self) -> serde_json::Value {
        self.json()
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }
}

/// One raw HTTP request built by a test.
pub(super) struct Request {
    pub(super) method: String,
    pub(super) target: String,
    pub(super) headers: Vec<(String, String)>,
    pub(super) body: Vec<u8>,
}

impl Request {
    /// Builds a request with the panel client header already set.
    pub(super) fn new(method: &str, target: &str) -> Self {
        Self {
            method: method.to_string(),
            target: target.to_string(),
            headers: vec![("x-telemt-panel".to_string(), "1".to_string())],
            body: Vec::new(),
        }
    }

    /// Builds a request without the panel client header.
    pub(super) fn bare(method: &str, target: &str) -> Self {
        Self {
            method: method.to_string(),
            target: target.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Adds one header.
    pub(super) fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// Attaches a JSON body.
    pub(super) fn json(mut self, value: serde_json::Value) -> Self {
        self.body = serde_json::to_vec(&value).expect("encode body");
        self.headers
            .push(("content-type".to_string(), "application/json".to_string()));
        self
    }

    /// Attaches a raw body.
    pub(super) fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    /// Renders the request as bytes on the wire.
    fn encode(&self, host: &str) -> Vec<u8> {
        let mut out = format!(
            "{} {} HTTP/1.1\r\nhost: {host}\r\n",
            self.method, self.target
        );
        for (name, value) in &self.headers {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        out.push_str(&format!("content-length: {}\r\n", self.body.len()));
        out.push_str("connection: close\r\n\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

/// Sends one request to the panel and reads the whole response.
pub(super) async fn send(address: SocketAddr, request: Request) -> Response {
    let host = address.to_string();
    let mut stream = TcpStream::connect(address).await.expect("connect panel");
    stream
        .write_all(&request.encode(&host))
        .await
        .expect("write request");
    let mut raw = Vec::new();
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let read = stream.read(&mut buffer).await.expect("read response");
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
    }
    let split = find_header_end(&raw).unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    Response {
        status,
        headers,
        body: raw[split..].to_vec(),
    }
}

/// An authenticated browser session against the fixture.
pub(super) struct Browser {
    pub(super) cookie: String,
    pub(super) csrf: String,
}

/// Signs in as the bootstrap administrator.
pub(super) async fn sign_in(fixture: &PanelFixture) -> Browser {
    let response = send(
        fixture.address,
        Request::new("POST", "/panel/api/session").json(serde_json::json!({
            "username": "admin",
            "password": fixture.bootstrap_password,
        })),
    )
    .await;
    assert_eq!(response.status, 200, "login failed: {:?}", response.json());
    let cookie = response
        .headers_all("set-cookie")
        .into_iter()
        .find_map(|value| value.split(';').next())
        .expect("session cookie")
        .to_string();
    let csrf = response.data()["csrf_token"]
        .as_str()
        .expect("csrf token")
        .to_string();
    Browser { cookie, csrf }
}

impl Browser {
    /// Adds the session cookie to a request.
    pub(super) fn authorize(&self, request: Request) -> Request {
        request.header("cookie", &self.cookie)
    }

    /// Adds the session cookie and the CSRF token to a request.
    pub(super) fn authorize_mutation(&self, request: Request) -> Request {
        request
            .header("cookie", &self.cookie)
            .header("x-telemt-csrf", &self.csrf)
    }
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn header_of(head: &str, name: &str) -> Option<String> {
    head.split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_string())
}
