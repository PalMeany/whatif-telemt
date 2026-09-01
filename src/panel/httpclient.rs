//! Minimal HTTP/1.1 client used by the panel's control plane.
//!
//! The panel talks to exactly two kinds of endpoint: this node's own Control
//! API on loopback, and a linked node's cluster endpoint over TLS. Both are
//! low-rate request/response exchanges, so one connection per request keeps the
//! client small and stateless; there is no pool to invalidate when a node's
//! certificate or address changes.
//!
//! Certificate pinning is supported because a linked node is frequently
//! reachable only under a self-signed certificate, and "trust anything" is not
//! an acceptable alternative for a channel that carries fleet control.

use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::crypto::sha256;

/// Everything the panel needs from one upstream response.
#[derive(Debug, Clone)]
pub(crate) struct HttpResponse {
    /// HTTP status code.
    pub(crate) status: StatusCode,
    /// Response body, truncated at the caller's ceiling.
    pub(crate) body: Vec<u8>,
    /// `Content-Type`, when the upstream sent one.
    pub(crate) content_type: Option<String>,
}

/// One outbound request.
pub(crate) struct HttpRequest<'a> {
    /// Absolute target URL.
    pub(crate) url: &'a str,
    /// HTTP method.
    pub(crate) method: &'a str,
    /// Extra request headers.
    pub(crate) headers: Vec<(String, String)>,
    /// Request body; empty for verbs that carry none.
    pub(crate) body: Vec<u8>,
    /// Deadline for connect, request, and response together.
    pub(crate) timeout: Duration,
    /// Largest response body accepted.
    pub(crate) max_response_bytes: usize,
    /// Lowercase hex SHA-256 of the expected leaf certificate, when pinned.
    pub(crate) pin_sha256: Option<String>,
}

/// Failure of one outbound request.
#[derive(Debug)]
pub(crate) enum HttpClientError {
    /// The URL could not be used as a request target.
    InvalidUrl(String),
    /// The host did not resolve, or no address accepted a connection.
    Connect(String),
    /// TLS negotiation failed, including a pin mismatch.
    Tls(String),
    /// The exchange failed after the connection was established.
    Transport(String),
    /// The response body exceeded the caller's ceiling.
    ResponseTooLarge,
    /// The deadline elapsed.
    Timeout,
}

impl std::fmt::Display for HttpClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpClientError::InvalidUrl(detail) => write!(formatter, "invalid url: {detail}"),
            HttpClientError::Connect(detail) => write!(formatter, "connect failed: {detail}"),
            HttpClientError::Tls(detail) => write!(formatter, "tls failed: {detail}"),
            HttpClientError::Transport(detail) => write!(formatter, "transport failed: {detail}"),
            HttpClientError::ResponseTooLarge => write!(formatter, "response too large"),
            HttpClientError::Timeout => write!(formatter, "request timed out"),
        }
    }
}

/// Performs one request and reads the whole response.
pub(crate) async fn send(request: HttpRequest<'_>) -> Result<HttpResponse, HttpClientError> {
    let timeout = request.timeout;
    tokio::time::timeout(timeout, send_inner(request))
        .await
        .map_err(|_| HttpClientError::Timeout)?
}

/// Body of [`send`], run under the caller's deadline.
async fn send_inner(request: HttpRequest<'_>) -> Result<HttpResponse, HttpClientError> {
    let target = Target::parse(request.url)?;
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|_| HttpClientError::InvalidUrl(format!("method {}", request.method)))?;

    let mut builder = Request::builder()
        .method(method)
        .uri(&target.path)
        .header(hyper::header::HOST, target.authority.as_str());
    for (name, value) in &request.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| HttpClientError::InvalidUrl(format!("header {name}")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| HttpClientError::InvalidUrl("header value".to_string()))?;
        builder = builder.header(name, value);
    }
    let outgoing = builder
        .body(Full::new(Bytes::from(request.body)))
        .map_err(|error| HttpClientError::InvalidUrl(error.to_string()))?;

    let stream = connect(&target).await?;
    if target.tls {
        let connector = TlsConnector::from(tls_config(request.pin_sha256.as_deref())?);
        let server_name = ServerName::try_from(target.host.clone())
            .map_err(|_| HttpClientError::InvalidUrl(format!("host {}", target.host)))?;
        let stream = connector
            .connect(server_name, stream)
            .await
            .map_err(|error| HttpClientError::Tls(error.to_string()))?;
        exchange(TokioIo::new(stream), outgoing, request.max_response_bytes).await
    } else {
        exchange(TokioIo::new(stream), outgoing, request.max_response_bytes).await
    }
}

/// Drives one request/response exchange over an established transport.
async fn exchange<T>(
    io: T,
    request: Request<Full<Bytes>>,
    max_response_bytes: usize,
) -> Result<HttpResponse, HttpClientError>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (mut sender, connection) = http1::handshake(io)
        .await
        .map_err(|error| HttpClientError::Transport(error.to_string()))?;
    // The connection task ends when the response body completes or the sender
    // is dropped; it owns no state the caller has to join on.
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let response = sender
        .send_request(request)
        .await
        .map_err(|error| HttpClientError::Transport(error.to_string()))?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut body = response.into_body();
    let mut collected = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|error| HttpClientError::Transport(error.to_string()))?;
        if let Some(chunk) = frame.data_ref() {
            if collected.len().saturating_add(chunk.len()) > max_response_bytes {
                return Err(HttpClientError::ResponseTooLarge);
            }
            collected.extend_from_slice(chunk);
        }
    }
    Ok(HttpResponse {
        status,
        body: collected,
        content_type,
    })
}

/// Opens a TCP connection to the first address that accepts one.
async fn connect(target: &Target) -> Result<TcpStream, HttpClientError> {
    let addresses = tokio::net::lookup_host((target.host.as_str(), target.port))
        .await
        .map_err(|error| HttpClientError::Connect(error.to_string()))?;
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect(address).await {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    Err(HttpClientError::Connect(last_error.unwrap_or_else(|| {
        format!("no address for {}", target.host)
    })))
}

/// Builds the client TLS configuration, pinned or web-PKI.
fn tls_config(pin_sha256: Option<&str>) -> Result<Arc<rustls::ClientConfig>, HttpClientError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|error| HttpClientError::Tls(error.to_string()))?;
    let config = match pin_sha256 {
        Some(pin) => {
            let expected =
                hex::decode(pin).map_err(|_| HttpClientError::Tls("pin is not hex".to_string()))?;
            if expected.len() != 32 {
                return Err(HttpClientError::Tls(
                    "pin must be a SHA-256 digest".to_string(),
                ));
            }
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(PinnedVerifier { expected, provider }))
                .with_no_client_auth()
        }
        None => {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };
    Ok(Arc::new(config))
}

/// Certificate verifier that accepts exactly one leaf certificate.
///
/// Name validation is deliberately not performed: the pin *is* the identity,
/// and a linked node is routinely reached by an address that no certificate
/// names. Signature verification stays intact so the handshake still proves
/// possession of the pinned certificate's private key.
#[derive(Debug)]
struct PinnedVerifier {
    expected: Vec<u8>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let digest = sha256(end_entity.as_ref());
        if digest.as_slice() == self.expected.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "server certificate does not match the pinned fingerprint".to_string(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Parsed request target.
struct Target {
    tls: bool,
    host: String,
    port: u16,
    authority: String,
    path: String,
}

impl Target {
    /// Splits a URL into the pieces the request needs.
    fn parse(raw: &str) -> Result<Self, HttpClientError> {
        let parsed =
            url::Url::parse(raw).map_err(|error| HttpClientError::InvalidUrl(error.to_string()))?;
        let tls = match parsed.scheme() {
            "https" => true,
            "http" => false,
            other => return Err(HttpClientError::InvalidUrl(format!("scheme {other}"))),
        };
        let host = parsed
            .host_str()
            .ok_or_else(|| HttpClientError::InvalidUrl("missing host".to_string()))?
            .to_string();
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| HttpClientError::InvalidUrl("missing port".to_string()))?;
        let default_port = if tls { 443 } else { 80 };
        let authority = if port == default_port {
            host.clone()
        } else {
            format!("{host}:{port}")
        };
        let mut path = parsed.path().to_string();
        if path.is_empty() {
            path.push('/');
        }
        if let Some(query) = parsed.query() {
            path.push('?');
            path.push_str(query);
        }
        Ok(Self {
            tls,
            host,
            port,
            authority,
            path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_normalises_authority_and_path() {
        let target = Target::parse("https://node.example.com/cluster/v1/hello?x=1").expect("parse");
        assert!(target.tls);
        assert_eq!(target.authority, "node.example.com");
        assert_eq!(target.path, "/cluster/v1/hello?x=1");
        assert_eq!(target.port, 443);

        let target = Target::parse("http://127.0.0.1:9091/v1/health").expect("parse");
        assert!(!target.tls);
        assert_eq!(target.authority, "127.0.0.1:9091");
        assert_eq!(target.path, "/v1/health");

        let target = Target::parse("https://node.example.com:8443").expect("parse");
        assert_eq!(target.authority, "node.example.com:8443");
        assert_eq!(target.path, "/");
    }

    #[test]
    fn unsupported_schemes_are_refused() {
        assert!(matches!(
            Target::parse("ftp://example.com"),
            Err(HttpClientError::InvalidUrl(_))
        ));
    }

    #[test]
    fn a_malformed_pin_is_refused_before_any_connection() {
        assert!(matches!(
            tls_config(Some("zz")),
            Err(HttpClientError::Tls(_))
        ));
        assert!(matches!(
            tls_config(Some(&"ab".repeat(16))),
            Err(HttpClientError::Tls(_))
        ));
        assert!(tls_config(Some(&"ab".repeat(32))).is_ok());
        assert!(tls_config(None).is_ok());
    }
}
