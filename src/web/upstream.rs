//! Reverse proxy to the operator's private loopback web application.
//!
//! Application mode keeps the relay the only public gateway while the operator
//! owns the framework, headers, cookies, and persistence of the public site.
//! Streaming responses and protocol upgrades are forwarded unchanged, so the
//! application may use SSE or its own WebSockets.

use std::net::{IpAddr, SocketAddr};

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

/// Forwarder to one private loopback application.
pub(crate) struct UpstreamProxy {
    address: SocketAddr,
}

impl UpstreamProxy {
    pub(crate) fn new(address: SocketAddr) -> Self {
        Self { address }
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
        let stream = match TcpStream::connect(self.address).await {
            Ok(stream) => stream,
            Err(error) => {
                debug!(upstream = %self.address, error = %error, "Public upstream unreachable");
                return bad_gateway();
            }
        };
        let _ = stream.set_nodelay(true);
        let handshake = hyper::client::conn::http1::handshake::<_, WebBody>(
            hyper_util::rt::TokioIo::new(stream),
        )
        .await;
        let (mut sender, connection) = match handshake {
            Ok(parts) => parts,
            Err(error) => {
                debug!(upstream = %self.address, error = %error, "Public upstream handshake failed");
                return bad_gateway();
            }
        };
        tokio::spawn(async move {
            if let Err(error) = connection.with_upgrades().await {
                debug!(error = %error, "Public upstream connection ended");
            }
        });
        let mut response = match sender.send_request(request).await {
            Ok(response) => response,
            Err(error) => {
                debug!(upstream = %self.address, error = %error, "Public upstream request failed");
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

fn bad_gateway() -> Response<WebBody> {
    let mut response = Response::new(full(Bytes::from_static(b"site unavailable\n")));
    *response.status_mut() = StatusCode::BAD_GATEWAY;
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}
