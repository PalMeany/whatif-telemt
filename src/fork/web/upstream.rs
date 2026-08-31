//! Reverse proxy to the operator's private loopback web application.
//!
//! Application mode keeps the relay the only public gateway while the operator
//! owns the framework, headers, cookies, and persistence of the public site.
//! Streaming responses and protocol upgrades are forwarded unchanged, so the
//! application may use SSE or its own WebSockets.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use http_body_util::{BodyExt, Empty};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode, Uri};
use tokio::net::TcpStream;
use tracing::debug;

use super::http::{WebBody, full};

/// Headers that describe one hop and must not be forwarded verbatim.
const HOP_BY_HOP: [&str; 7] = [
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
];

/// Relay transport headers stripped from a sanitized fallback request.
const TRANSPORT_HEADERS: [&str; 10] = [
    "authorization",
    "content-length",
    "content-type",
    "sec-websocket-key",
    "sec-websocket-protocol",
    "sec-websocket-version",
    "upgrade",
    "x-down-cursor",
    "x-lane-id",
    "x-up-seq",
];

/// Deadline for connecting and handshaking with the loopback application.
///
/// The application is on loopback, so a connect that has not completed in this
/// long is wedged, not slow.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Forwarder to one private loopback application.
pub(crate) struct UpstreamProxy {
    address: SocketAddr,
    /// Deadline for the application's response head.
    ///
    /// Nothing else bounds this hop: hyper disarms its header deadline once the
    /// relay's own head is parsed, and the reference front-proxy templates set
    /// no read timeout because long polls must not be cut short. An application
    /// that accepts a connection and never answers would otherwise hold a
    /// carrier connection and its accept-loop permit forever.
    response_timeout: Duration,
}

impl UpstreamProxy {
    pub(crate) fn new(address: SocketAddr, response_timeout: Duration) -> Self {
        Self {
            address,
            response_timeout,
        }
    }

    /// Forwards a request and streams the application's response back.
    pub(crate) async fn forward(
        &self,
        request: Request<Incoming>,
        client_ip: Option<IpAddr>,
    ) -> Response<WebBody> {
        let (mut parts, body) = request.into_parts();
        let upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>();
        sanitize_hop_headers(&mut parts.headers);
        if let Some(ip) = client_ip
            && let Ok(value) = HeaderValue::from_str(&ip.to_string())
        {
            parts.headers.insert("x-forwarded-for", value);
        }
        let outgoing = Request::from_parts(parts, body.boxed());
        self.send(outgoing, upgrade).await
    }

    /// Forwards a request that only pretended to be a transport request.
    ///
    /// The relay-specific headers and the body are dropped so the application
    /// sees a plain, bodyless request for that path.
    pub(crate) async fn forward_sanitized(
        &self,
        method: Method,
        uri: Uri,
        mut headers: hyper::HeaderMap<HeaderValue>,
        client_ip: Option<IpAddr>,
    ) -> Response<WebBody> {
        sanitize_hop_headers(&mut headers);
        for name in TRANSPORT_HEADERS {
            headers.remove(name);
        }
        headers.insert("connection", HeaderValue::from_static("close"));
        if let Some(ip) = client_ip
            && let Ok(value) = HeaderValue::from_str(&ip.to_string())
        {
            headers.insert("x-forwarded-for", value);
        }
        let empty: WebBody = Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed();
        let mut request = Request::new(empty);
        *request.method_mut() = method;
        *request.uri_mut() = uri;
        *request.headers_mut() = headers;
        self.send(request, None).await
    }

    async fn send(
        &self,
        request: Request<WebBody>,
        client_upgrade: Option<hyper::upgrade::OnUpgrade>,
    ) -> Response<WebBody> {
        let connected =
            tokio::time::timeout(UPSTREAM_CONNECT_TIMEOUT, TcpStream::connect(self.address)).await;
        let stream = match connected {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                debug!(upstream = %self.address, error = %error, "Public upstream unreachable");
                return bad_gateway();
            }
            Err(_) => {
                debug!(upstream = %self.address, "Public upstream connect timed out");
                return bad_gateway();
            }
        };
        let _ = stream.set_nodelay(true);
        let handshake = tokio::time::timeout(
            UPSTREAM_CONNECT_TIMEOUT,
            hyper::client::conn::http1::handshake::<_, WebBody>(hyper_util::rt::TokioIo::new(
                stream,
            )),
        )
        .await;
        let (mut sender, connection) = match handshake {
            Ok(Ok(parts)) => parts,
            Ok(Err(error)) => {
                debug!(upstream = %self.address, error = %error, "Public upstream handshake failed");
                return bad_gateway();
            }
            Err(_) => {
                debug!(upstream = %self.address, "Public upstream handshake timed out");
                return bad_gateway();
            }
        };
        tokio::spawn(async move {
            if let Err(error) = connection.with_upgrades().await {
                debug!(error = %error, "Public upstream connection ended");
            }
        });
        // Only the response head is bounded. The body streams unbounded on
        // purpose, because that is what carries SSE and large downloads for the
        // operator's own site.
        let sent = tokio::time::timeout(self.response_timeout, sender.send_request(request)).await;
        let mut response = match sent {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                debug!(upstream = %self.address, error = %error, "Public upstream request failed");
                return bad_gateway();
            }
            Err(_) => {
                debug!(upstream = %self.address, "Public upstream response timed out");
                return bad_gateway();
            }
        };
        if response.status() == StatusCode::SWITCHING_PROTOCOLS
            && let Some(client_upgrade) = client_upgrade
        {
            let upstream_upgrade = hyper::upgrade::on(&mut response);
            tokio::spawn(async move {
                splice_upgrade(client_upgrade, upstream_upgrade).await;
            });
        }
        let (mut parts, body) = response.into_parts();
        sanitize_hop_headers(&mut parts.headers);
        Response::from_parts(parts, body.boxed())
    }
}

/// Copies both directions of an upgraded connection until either side ends.
async fn splice_upgrade(client: hyper::upgrade::OnUpgrade, upstream: hyper::upgrade::OnUpgrade) {
    let (Ok(client), Ok(upstream)) = tokio::join!(client, upstream) else {
        return;
    };
    let mut client = hyper_util::rt::TokioIo::new(client);
    let mut upstream = hyper_util::rt::TokioIo::new(upstream);
    if let Err(error) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        debug!(error = %error, "Upgraded upstream connection ended");
    }
}

/// Removes hop-by-hop headers, including those named by `Connection`.
///
/// An upgrade keeps its `Connection` and `Upgrade` pair, because those two
/// headers are what makes the next hop perform the upgrade at all.
fn sanitize_hop_headers(headers: &mut hyper::HeaderMap<HeaderValue>) {
    let upgrading = headers.contains_key("upgrade");
    let named: Vec<HeaderName> = headers
        .get("connection")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .filter_map(|item| HeaderName::try_from(item.trim().to_ascii_lowercase()).ok())
                .collect()
        })
        .unwrap_or_default();
    for name in named {
        if name != "upgrade" {
            headers.remove(&name);
        }
    }
    if !upgrading {
        headers.remove("connection");
        headers.remove("upgrade");
    }
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
}

/// The answer when the operator's application cannot be reached.
///
/// The body is empty and carries no media type. Any fixed sentence here is a
/// banner a prober can force by exhausting the application's own accept queue,
/// and a banner unique to telemt identifies the origin outright.
fn bad_gateway() -> Response<WebBody> {
    let mut response = Response::new(full(Bytes::new()));
    *response.status_mut() = StatusCode::BAD_GATEWAY;
    let headers = response.headers_mut();
    headers.insert("content-length", HeaderValue::from_static("0"));
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    response
}
