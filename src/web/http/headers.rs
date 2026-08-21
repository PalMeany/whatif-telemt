//! Header and body parsing rules shared by the carrier endpoints.

use std::net::{IpAddr, SocketAddr};

use hyper::HeaderMap;
use hyper::header::HeaderValue;
use ipnetwork::IpNetwork;

use crate::web::capability::decode_token;

/// Reads one header as UTF-8, treating a non-UTF-8 value as absent.
pub(crate) fn header<'a>(headers: &'a HeaderMap<HeaderValue>, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Extracts a canonical bearer token from an `Authorization` header.
///
/// The value must be exactly `Bearer <token>` with a single space, and the
/// token must be canonical unpadded base64url of 32 bytes.
pub(crate) fn bearer_token(value: Option<&str>) -> Option<&str> {
    let value = value?;
    let token = value.strip_prefix("Bearer ")?;
    if token.contains(' ') || decode_token(token).is_none() {
        return None;
    }
    Some(token)
}

/// Parses a canonical decimal integer, rejecting padded or signed forms.
pub(crate) fn canonical_uint(value: &str) -> Option<u64> {
    if value.is_empty() || value.starts_with('+') || (value.len() > 1 && value.starts_with('0')) {
        return None;
    }
    let parsed = value.parse::<u64>().ok()?;
    (parsed.to_string() == value).then_some(parsed)
}

/// True when the media type is exactly `application/octet-stream`.
pub(crate) fn binary_content_type(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let trimmed = value.trim();
    if trimmed.contains(';') {
        return false;
    }
    trimmed.eq_ignore_ascii_case("application/octet-stream")
}

/// Resolves the effective client address for accounting and limits.
///
/// `X-Forwarded-For` is honoured only from a configured front proxy. A request
/// that arrives from an untrusted source is accounted against its own peer
/// address, and its forwarding header is ignored entirely.
///
/// When the header carries a list, the **last** entry is used: that is the
/// address the nearest trusted proxy observed, and it is the only element a
/// client cannot inject, because every proxy appends its own observation. A
/// list only appears when a proxy chain forwards inbound values — with one
/// front proxy the header is a single address either way.
pub(crate) fn client_ip(
    peer: SocketAddr,
    headers: &HeaderMap<HeaderValue>,
    trusted: &[IpNetwork],
) -> Option<IpAddr> {
    let peer_ip = peer.ip();
    let is_trusted = trusted.iter().any(|network| network.contains(peer_ip));
    if !is_trusted {
        return Some(peer_ip);
    }
    let Some(forwarded) = header(headers, "x-forwarded-for") else {
        return Some(peer_ip);
    };
    let observed = forwarded.rsplit(',').next()?.trim();
    observed.parse::<IpAddr>().ok()
}

/// True when the `Host` header names the configured public hostname.
pub(crate) fn host_matches(headers: &HeaderMap<HeaderValue>, hostname: &str) -> bool {
    let Some(host) = header(headers, "host") else {
        return false;
    };
    host == hostname || host.len() == hostname.len() + 4 && host == format!("{hostname}:443")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::capability::encode_token;

    #[test]
    fn bearer_requires_canonical_token() {
        let token = encode_token(&[3u8; 32]);
        let header = format!("Bearer {token}");
        assert_eq!(bearer_token(Some(&header)), Some(token.as_str()));
        assert_eq!(bearer_token(Some("Bearer  x")), None);
        assert_eq!(bearer_token(Some(&format!("Bearer {token} extra"))), None);
        assert_eq!(bearer_token(Some("Basic abc")), None);
        assert_eq!(bearer_token(None), None);
    }

    #[test]
    fn canonical_uint_rejects_padding() {
        assert_eq!(canonical_uint("0"), Some(0));
        assert_eq!(canonical_uint("12"), Some(12));
        assert_eq!(canonical_uint("012"), None);
        assert_eq!(canonical_uint("+1"), None);
        assert_eq!(canonical_uint(""), None);
        assert_eq!(canonical_uint("1 "), None);
    }

    #[test]
    fn binary_content_type_rejects_parameters() {
        assert!(binary_content_type(Some("application/octet-stream")));
        assert!(!binary_content_type(Some(
            "application/octet-stream; charset=utf-8"
        )));
        assert!(!binary_content_type(Some("text/plain")));
        assert!(!binary_content_type(None));
    }

    #[test]
    fn forwarded_address_is_only_trusted_from_a_front_proxy() {
        let trusted: Vec<IpNetwork> = vec!["127.0.0.0/8".parse().expect("cidr")];
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("8.8.8.8"));
        let loopback: SocketAddr = "127.0.0.1:1234".parse().expect("addr");
        let external: SocketAddr = "203.0.113.7:1234".parse().expect("addr");
        assert_eq!(
            client_ip(loopback, &headers, &trusted),
            Some("8.8.8.8".parse().expect("ip"))
        );
        assert_eq!(
            client_ip(external, &headers, &trusted),
            Some("203.0.113.7".parse().expect("ip"))
        );
        // A proxy chain appends its own observation, so the last entry is the
        // address the nearest trusted hop actually saw.
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.1.1.1, 2.2.2.2"),
        );
        assert_eq!(
            client_ip(loopback, &headers, &trusted),
            Some("2.2.2.2".parse().expect("ip"))
        );
        // A client-injected value never displaces the peer of an untrusted hop.
        assert_eq!(
            client_ip(external, &headers, &trusted),
            Some("203.0.113.7".parse().expect("ip"))
        );
        // A malformed final entry fails closed rather than guessing.
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("1.1.1.1, bogus"),
        );
        assert_eq!(client_ip(loopback, &headers, &trusted), None);
        headers.insert("x-forwarded-for", HeaderValue::from_static(""));
        assert_eq!(client_ip(loopback, &headers, &trusted), None);
    }
}
