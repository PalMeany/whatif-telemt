//! Header and body parsing rules shared by the carrier endpoints.

use std::net::{IpAddr, SocketAddr};

use hyper::HeaderMap;
use hyper::header::HeaderValue;
use ipnetwork::IpNetwork;

use crate::fork::web::capability::decode_token;

/// Reads one header as UTF-8, treating a non-UTF-8 value as absent.
///
/// Callers that route on a header must use [`header_present`] instead: a
/// non-UTF-8 `X-Lane-ID` read as "absent" would silently move a lanes request
/// onto the shared-carrier path, where the reference answers its ordinary 404.
pub(crate) fn header<'a>(headers: &'a HeaderMap<HeaderValue>, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// True when the header is present at all, decodable or not.
pub(crate) fn header_present(headers: &HeaderMap<HeaderValue>, name: &str) -> bool {
    headers.contains_key(name)
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
    if forwarded.contains(',') {
        // A chain collapses every user behind the far proxy into one per-address
        // bucket. That is a working configuration, not an error, but it silently
        // changes what the per-address ceilings mean, so it is said once.
        warn_forwarded_chain();
    }
    let observed = forwarded.rsplit(',').next()?.trim();
    observed.parse::<IpAddr>().ok()
}

/// Warns once per process that `X-Forwarded-For` arrives as a chain.
fn warn_forwarded_chain() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            "WEB proxy sees an X-Forwarded-For chain: the nearest proxy's observation is used, so \
             every client behind the far proxy shares one per-address bucket. Set \
             web.trusted_proxies to the nearest hop only if that is not intended."
        );
    });
}

/// True when the `Host` header names the configured public hostname.
///
/// Host names are case-insensitive and may carry any port when a front proxy or
/// CDN reaches the origin on a non-standard one. Both are normalised away,
/// because rejecting them produces a blanket 404 that looks exactly like a
/// misconfigured site; a trailing dot is not — see `request_host`.
pub(crate) fn host_matches(headers: &HeaderMap<HeaderValue>, hostname: &str) -> bool {
    // More than one `Host` is a request-smuggling primitive and RFC 9112
    // forbids it outright, so it is refused before the value is even read.
    if headers.get_all("host").iter().count() != 1 {
        return false;
    }
    request_host(headers).is_some_and(|host| host.eq_ignore_ascii_case(hostname))
}

/// Returns the `Host` header without its port.
///
/// Case and port are normalised away deliberately, unlike the reference, which
/// compares byte-exactly against `H` or `H:443`. Every ordinary origin server
/// matches this way, and refusing a CDN-fronted or non-443 origin with a
/// blanket 404 is a bigger difference from an ordinary site than serving it.
/// A trailing dot is *not* normalised: `H.` is a distinct name that no browser
/// sends here, and accepting it would widen the set of request targets that
/// reach the bridge for no client that needs it.
pub(crate) fn request_host(headers: &HeaderMap<HeaderValue>) -> Option<&str> {
    let host = header(headers, "host")?;
    // An IPv6 literal keeps its colons inside brackets; the configured
    // hostname can never be an address, so such a value simply will not match.
    let without_port = match host.rfind(':') {
        Some(index) if !host.starts_with('[') => {
            // A port has to be a port. `example.com:` and `example.com:http`
            // are malformed authorities, not the configured host.
            let port = &host[index + 1..];
            if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            &host[..index]
        }
        _ => host,
    };
    (!without_port.is_empty()).then_some(without_port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fork::web::capability::encode_token;

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
    fn host_matching_normalises_case_and_port_only() {
        let mut headers = HeaderMap::new();
        // Case and port are normalised away: every ordinary origin matches this
        // way, and a CDN-fronted or non-443 deployment must not 404 everything.
        for value in [
            "proxy.example.com",
            "PROXY.Example.CoM",
            "proxy.example.com:443",
            "proxy.example.com:8443",
        ] {
            headers.insert("host", HeaderValue::from_str(value).expect("header"));
            assert!(
                host_matches(&headers, "proxy.example.com"),
                "expected {value} to match"
            );
        }
        for value in [
            "other.example.com",
            "proxy.example.com.evil.net",
            ":443",
            "",
            // A trailing dot is a distinct name no browser sends, and a port
            // that is not a port is a malformed authority.
            "proxy.example.com.",
            "proxy.example.com.:443",
            "proxy.example.com:",
            "proxy.example.com:https",
        ] {
            headers.insert("host", HeaderValue::from_str(value).expect("header"));
            assert!(
                !host_matches(&headers, "proxy.example.com"),
                "expected {value} to be refused"
            );
        }
        assert!(!host_matches(&HeaderMap::new(), "proxy.example.com"));

        // A duplicated Host is a smuggling primitive, refused before the value
        // is read at all -- even when both copies name the right host.
        let mut duplicated = HeaderMap::new();
        duplicated.append("host", HeaderValue::from_static("proxy.example.com"));
        duplicated.append("host", HeaderValue::from_static("proxy.example.com"));
        assert!(!host_matches(&duplicated, "proxy.example.com"));
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
