//! Bot API transport.
//!
//! A small HTTPS client modelled on `transport::middle_proxy::http_fetch`: the
//! same rustls configuration, and the same routing through the configured
//! `[[upstreams]]`. Routing matters more here than anywhere else in this
//! crate — a host that needs a SOCKS or Shadowsocks egress to reach Telegram
//! at all would otherwise have the bot dial `api.telegram.org` directly, which
//! either fails or announces the proxy's real address.
//!
//! Kept separate from `http_fetch` because that helper is GET-only under a
//! fixed 15-second budget, and a `getUpdates` long poll is a POST that has to
//! outlive it.

use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::header::{CONNECTION, CONTENT_TYPE, HOST, USER_AGENT};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tracing::debug;

use crate::error::{ProxyError, Result};
use crate::transport::{UpstreamManager, UpstreamStream};

/// Budget for establishing the transport under the Bot API origin.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest Bot API response this client will read.
///
/// `getUpdates` is bounded by its own `limit`, so anything past this is a
/// misbehaving or impersonated endpoint rather than a large batch.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// One Bot API call's outcome.
pub(super) struct ApiResponse {
    /// HTTP status the endpoint answered with.
    pub(super) status: u16,
    /// Response body, truncated at [`MAX_RESPONSE_BYTES`].
    pub(super) body: Vec<u8>,
}

/// Posts one JSON document to a Bot API method and reads the reply.
pub(super) async fn post_json(
    origin: &str,
    token: &str,
    method: &str,
    payload: &str,
    egress: Option<(Arc<UpstreamManager>, String)>,
    request_timeout: Duration,
) -> Result<ApiResponse> {
    let (scheme, host, port) = split_origin(origin)?;
    let path = format!("/bot{token}/{method}");
    let stream = connect(&host, port, egress).await?;

    let response = match scheme {
        Scheme::Https => {
            let server_name = ServerName::try_from(host.clone())
                .map_err(|_| ProxyError::Proxy(format!("invalid TLS server name: {host}")))?;
            let connector = TlsConnector::from(tls_client_config());
            let tls = timeout(CONNECT_TIMEOUT, connector.connect(server_name, stream))
                .await
                .map_err(|_| ProxyError::Proxy(format!("TLS handshake timeout for {host}")))?
                .map_err(|error| {
                    ProxyError::Proxy(format!("TLS handshake failed for {host}: {error}"))
                })?;
            send(
                TokioIo::new(tls),
                &host,
                port,
                &path,
                payload,
                request_timeout,
            )
            .await?
        }
        // Only reachable with a self-hosted Bot API server on the same box;
        // the configuration refuses anything else.
        Scheme::Http => {
            send(
                TokioIo::new(stream),
                &host,
                port,
                &path,
                payload,
                request_timeout,
            )
            .await?
        }
    };
    Ok(response)
}

/// Origin scheme, as written in `fork.telegram.api_base`.
enum Scheme {
    Http,
    Https,
}

/// Splits `scheme://host[:port]` into its parts.
fn split_origin(origin: &str) -> Result<(Scheme, String, u16)> {
    let (scheme, rest, default_port) = if let Some(rest) = origin.strip_prefix("https://") {
        (Scheme::Https, rest, 443u16)
    } else if let Some(rest) = origin.strip_prefix("http://") {
        (Scheme::Http, rest, 80u16)
    } else {
        return Err(ProxyError::Config(format!(
            "fork.telegram.api_base '{origin}' must be an http:// or https:// origin"
        )));
    };
    if rest.contains('/') {
        return Err(ProxyError::Config(format!(
            "fork.telegram.api_base '{origin}' must not contain a path"
        )));
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !host.contains(']') => {
            let port = port.parse::<u16>().map_err(|_| {
                ProxyError::Config(format!(
                    "fork.telegram.api_base '{origin}' has an invalid port"
                ))
            })?;
            (host.to_string(), port)
        }
        _ => (rest.to_string(), default_port),
    };
    if host.is_empty() {
        return Err(ProxyError::Config(format!(
            "fork.telegram.api_base '{origin}' has no host"
        )));
    }
    Ok((scheme, host, port))
}

/// Builds the same TLS configuration the middle-end control fetches use.
fn tls_client_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = rustls::crypto::ring::default_provider();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .expect("bot API rustls protocol versions must be valid")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

/// Opens the transport, through a scoped upstream when one is configured.
///
/// The scope is not optional when an upstream is used: selecting an unscoped
/// upstream would charge a Bot API outage to the ones client traffic depends
/// on, and five failed polls are enough to mark them unhealthy.
async fn connect(
    host: &str,
    port: u16,
    egress: Option<(Arc<UpstreamManager>, String)>,
) -> Result<UpstreamStream> {
    if let Some((manager, scope)) = egress {
        let target = manager.resolve_hostname(host, port).await?;
        return timeout(
            CONNECT_TIMEOUT,
            manager.connect(target, None, Some(scope.as_str())),
        )
        .await
        .map_err(|_| ProxyError::Proxy(format!("bot API connect timeout for {host}:{port}")))?
        .map_err(|error| {
            ProxyError::Proxy(format!("bot API connect failed for {host}:{port}: {error}"))
        });
    }
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| ProxyError::Proxy(format!("bot API connect timeout for {host}:{port}")))?
        .map_err(|error| {
            ProxyError::Proxy(format!("bot API connect failed for {host}:{port}: {error}"))
        })?;
    Ok(UpstreamStream::Tcp(stream))
}

/// Sends one request over an established transport and reads the reply.
async fn send<T>(
    io: TokioIo<T>,
    host: &str,
    port: u16,
    path: &str,
    payload: &str,
    request_timeout: Duration,
) -> Result<ApiResponse>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|error| ProxyError::Proxy(format!("bot API handshake failed: {error}")))?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            debug!(error = %error, "Bot API connection task failed");
        }
    });

    let authority = if port == 443 || port == 80 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    let request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(HOST, authority)
        .header(CONTENT_TYPE, "application/json")
        // A distinctive agent string would make the proxy identifiable from a
        // Bot API log, so this claims nothing but the product it is.
        .header(USER_AGENT, "telemt-bot/1")
        .header(CONNECTION, "close")
        .body(Full::new(bytes::Bytes::from(payload.to_string())))
        .map_err(|error| ProxyError::Proxy(format!("bot API request build failed: {error}")))?;

    let response = timeout(request_timeout, sender.send_request(request))
        .await
        .map_err(|_| ProxyError::Proxy("bot API request timed out".to_string()))?
        .map_err(|error| ProxyError::Proxy(format!("bot API request failed: {error}")))?;
    let status = response.status().as_u16();

    let collected = timeout(request_timeout, response.into_body().collect())
        .await
        .map_err(|_| ProxyError::Proxy("bot API body read timed out".to_string()))?
        .map_err(|error| ProxyError::Proxy(format!("bot API body read failed: {error}")))?
        .to_bytes();
    let body = collected
        .iter()
        .take(MAX_RESPONSE_BYTES)
        .copied()
        .collect::<Vec<u8>>();

    Ok(ApiResponse { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_without_a_port_takes_the_scheme_default() {
        let (_, host, port) = split_origin("https://api.telegram.org").unwrap();
        assert_eq!(host, "api.telegram.org");
        assert_eq!(port, 443);

        let (_, host, port) = split_origin("http://127.0.0.1").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 80);
    }

    #[test]
    fn an_explicit_port_is_honoured() {
        let (_, host, port) = split_origin("http://127.0.0.1:8081").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8081);
    }

    #[test]
    fn an_origin_carrying_a_path_is_refused() {
        // A path here would silently move the bot token into someone else's
        // URL space, so it is refused rather than trimmed.
        assert!(split_origin("https://api.telegram.org/bot").is_err());
    }

    #[test]
    fn an_origin_without_a_scheme_is_refused() {
        assert!(split_origin("api.telegram.org").is_err());
    }
}
