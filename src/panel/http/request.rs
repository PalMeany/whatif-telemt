//! Request parsing shared by every panel route.

use std::net::{IpAddr, SocketAddr};

use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::{HeaderMap, Request};
use ipnetwork::IpNetwork;

/// Header the browser client sets on every panel API request.
///
/// A cross-origin form cannot set it, and the panel never answers a CORS
/// preflight, so requiring it makes every panel API route unreachable from
/// another origin regardless of cookie policy.
pub(crate) const HEADER_CLIENT: &str = "x-telemt-panel";

/// Header carrying the session's double-submit CSRF token.
pub(crate) const HEADER_CSRF: &str = "x-telemt-csrf";

/// Header selecting the node a Control API relay is addressed to.
pub(crate) const HEADER_NODE: &str = "x-telemt-panel-node";

/// Cookie holding the session bearer.
pub(crate) const SESSION_COOKIE: &str = "telemt_panel_session";

/// Why a body could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyError {
    /// The body exceeded the configured ceiling.
    TooLarge,
    /// The transport failed mid-body.
    Transport,
}

/// Reads a request body, refusing anything past the ceiling.
pub(crate) async fn read_body(body: Incoming, limit: usize) -> Result<Vec<u8>, BodyError> {
    let mut body = body;
    let mut collected = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| BodyError::Transport)?;
        if let Some(chunk) = frame.data_ref() {
            if collected.len().saturating_add(chunk.len()) > limit {
                return Err(BodyError::TooLarge);
            }
            collected.extend_from_slice(chunk);
        }
    }
    Ok(collected)
}

/// Reads one header as UTF-8, treating a non-UTF-8 value as absent.
pub(crate) fn header<'a>(headers: &'a HeaderMap<HeaderValue>, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Extracts one cookie value from the `Cookie` header.
pub(crate) fn cookie<'a>(headers: &'a HeaderMap<HeaderValue>, name: &str) -> Option<&'a str> {
    let raw = header(headers, "cookie")?;
    raw.split(';')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| key.trim() == name)
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())
}

/// Resolves the client address, honouring `X-Forwarded-For` only from a proxy.
///
/// Same rule as the WEB carrier: the last entry is the observation of the
/// nearest trusted hop, which is the only element a client cannot inject.
pub(crate) fn client_ip(
    peer: SocketAddr,
    headers: &HeaderMap<HeaderValue>,
    trusted: &[IpNetwork],
) -> Option<IpAddr> {
    let peer_ip = peer.ip();
    if !trusted.iter().any(|network| network.contains(peer_ip)) {
        return Some(peer_ip);
    }
    let Some(forwarded) = header(headers, "x-forwarded-for") else {
        return Some(peer_ip);
    };
    forwarded
        .rsplit(',')
        .next()
        .map(str::trim)
        .and_then(|observed| observed.parse::<IpAddr>().ok())
}

/// True when the request's `Origin` is same-origin, or absent.
///
/// A same-origin fetch from a modern browser sends `Origin` on every
/// state-changing request. Comparing it with `Host` closes the gap left when a
/// future client library or a proxy strips the custom client header.
pub(crate) fn origin_is_same(headers: &HeaderMap<HeaderValue>) -> bool {
    let Some(origin) = header(headers, "origin") else {
        return true;
    };
    if origin == "null" {
        return false;
    }
    let Ok(parsed) = url::Url::parse(origin) else {
        return false;
    };
    let Some(host) = header(headers, "host") else {
        return false;
    };
    let origin_authority = match parsed.port() {
        Some(port) => format!("{}:{port}", parsed.host_str().unwrap_or_default()),
        None => parsed.host_str().unwrap_or_default().to_string(),
    };
    origin_authority.eq_ignore_ascii_case(host)
        || default_port_authority(&parsed).is_some_and(|value| value.eq_ignore_ascii_case(host))
}

/// Renders the authority an origin implies when it omits a default port.
fn default_port_authority(parsed: &url::Url) -> Option<String> {
    let host = parsed.host_str()?;
    let port = parsed.port_or_known_default()?;
    Some(format!("{host}:{port}"))
}

/// True when the request method changes state.
pub(crate) fn is_mutating(method: &hyper::Method) -> bool {
    !matches!(method, &hyper::Method::GET | &hyper::Method::HEAD)
}

/// Splits a request target into its path and query halves.
pub(crate) fn split_target(request: &Request<Incoming>) -> (String, Option<String>) {
    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);
    (path, query)
}

/// Reads one query parameter without decoding the whole string into a map.
pub(crate) fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == name {
            return Some(percent_decode(value));
        }
    }
    None
}

/// Decodes the percent-escapes a query value may carry.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(decoded) => {
                        out.push(decoded);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap<HeaderValue> {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                hyper::header::HeaderName::from_bytes(name.as_bytes()).expect("name"),
                HeaderValue::from_str(value).expect("value"),
            );
        }
        map
    }

    #[test]
    fn cookies_are_split_on_the_exact_name() {
        let map = headers(&[("cookie", "a=1; telemt_panel_session=abc; b=2")]);
        assert_eq!(cookie(&map, SESSION_COOKIE), Some("abc"));
        assert_eq!(cookie(&map, "missing"), None);
        let empty = headers(&[("cookie", "telemt_panel_session=")]);
        assert_eq!(cookie(&empty, SESSION_COOKIE), None);
    }

    #[test]
    fn a_cross_origin_request_is_recognised() {
        let same = headers(&[
            ("origin", "https://panel.example.com"),
            ("host", "panel.example.com"),
        ]);
        assert!(origin_is_same(&same));
        let same_port = headers(&[
            ("origin", "https://panel.example.com:8443"),
            ("host", "panel.example.com:8443"),
        ]);
        assert!(origin_is_same(&same_port));
        let cross = headers(&[
            ("origin", "https://evil.example.com"),
            ("host", "panel.example.com"),
        ]);
        assert!(!origin_is_same(&cross));
        let opaque = headers(&[("origin", "null"), ("host", "panel.example.com")]);
        assert!(!origin_is_same(&opaque));
        assert!(origin_is_same(&headers(&[("host", "panel.example.com")])));
    }

    #[test]
    fn forwarded_addresses_are_trusted_only_from_a_front_proxy() {
        let trusted: Vec<IpNetwork> = vec!["127.0.0.0/8".parse().expect("cidr")];
        let map = headers(&[("x-forwarded-for", "198.51.100.9")]);
        let loopback: SocketAddr = "127.0.0.1:1000".parse().expect("addr");
        let external: SocketAddr = "203.0.113.4:1000".parse().expect("addr");
        assert_eq!(
            client_ip(loopback, &map, &trusted),
            Some("198.51.100.9".parse().expect("ip"))
        );
        assert_eq!(
            client_ip(external, &map, &trusted),
            Some("203.0.113.4".parse().expect("ip"))
        );
    }

    #[test]
    fn query_parameters_are_percent_decoded() {
        assert_eq!(
            query_param(Some("node=edge%2D1&limit=10"), "node").as_deref(),
            Some("edge-1")
        );
        assert_eq!(query_param(Some("a=1"), "b"), None);
        assert_eq!(query_param(None, "a"), None);
    }
}
