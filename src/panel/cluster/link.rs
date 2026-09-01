//! The link string an operator copies from an agent into a master.
//!
//! Everything a master needs to reach an agent travels in one token: the
//! agent's identity, its reachable base URL, the shared HMAC key, and the
//! certificate fingerprint to pin. Packing it into a single opaque string keeps
//! the key off the operator's clipboard as a separate item they might paste
//! into the wrong field.

use serde::{Deserialize, Serialize};

use crate::panel::crypto::{decode, encode};

/// Scheme-like prefix that makes the token recognisable in a chat log.
const PREFIX: &str = "telemt-node:";

/// Current token version.
const VERSION: u32 = 1;

/// Decoded link token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LinkToken {
    /// Token version.
    pub(crate) v: u32,
    /// Agent node identifier.
    pub(crate) id: String,
    /// Agent display name.
    #[serde(default)]
    pub(crate) name: String,
    /// Base URL of the agent's panel, without a trailing slash.
    pub(crate) url: String,
    /// Base64url HMAC link key.
    pub(crate) key: String,
    /// Lowercase hex SHA-256 of the agent's leaf certificate, when pinned.
    #[serde(default)]
    pub(crate) fp: Option<String>,
}

impl LinkToken {
    /// Builds a token for this node.
    pub(crate) fn new(
        id: String,
        name: String,
        url: String,
        key: String,
        fingerprint: Option<String>,
    ) -> Self {
        Self {
            v: VERSION,
            id,
            name,
            url: url.trim_end_matches('/').to_string(),
            key,
            fp: fingerprint,
        }
    }

    /// Renders the token an operator copies.
    pub(crate) fn render(&self) -> String {
        let encoded = serde_json::to_vec(self).unwrap_or_default();
        format!("{PREFIX}{}", encode(&encoded))
    }

    /// Parses a token, rejecting anything malformed or of a foreign version.
    pub(crate) fn parse(raw: &str) -> Result<Self, &'static str> {
        let trimmed = raw.trim();
        let body = trimmed
            .strip_prefix(PREFIX)
            .ok_or("link token must start with `telemt-node:`")?;
        let decoded = decode(body).ok_or("link token is not canonical base64url")?;
        let token: LinkToken =
            serde_json::from_slice(&decoded).map_err(|_| "link token payload is not valid JSON")?;
        if token.v != VERSION {
            return Err("link token version is not supported");
        }
        if token.id.is_empty() || token.id.len() > 128 {
            return Err("link token node id is invalid");
        }
        if decode(&token.key).map(|key| key.len()) != Some(32) {
            return Err("link token key must be 32 bytes of base64url");
        }
        validate_url(&token.url)?;
        if let Some(fingerprint) = token.fp.as_deref()
            && (fingerprint.len() != 64
                || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err("link token fingerprint must be a SHA-256 hex digest");
        }
        Ok(token)
    }
}

/// Rejects a base URL a master must not dial.
pub(crate) fn validate_url(raw: &str) -> Result<(), &'static str> {
    let parsed = url::Url::parse(raw).map_err(|_| "node url is not a URL")?;
    match parsed.scheme() {
        "https" => {}
        "http" => {
            let loopback = parsed
                .host_str()
                .map(|host| {
                    host.eq_ignore_ascii_case("localhost")
                        || host
                            .trim_start_matches('[')
                            .trim_end_matches(']')
                            .parse::<std::net::IpAddr>()
                            .map(|address| address.is_loopback())
                            .unwrap_or(false)
                })
                .unwrap_or(false);
            if !loopback {
                return Err("node url may only use http for a loopback host");
            }
        }
        _ => return Err("node url scheme must be https"),
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("node url must contain a host");
    }
    if !parsed.path().is_empty() && parsed.path() != "/" {
        return Err("node url must not contain a path");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> LinkToken {
        LinkToken::new(
            "node-abc".to_string(),
            "edge-1".to_string(),
            "https://node.example.com:8443".to_string(),
            encode(&[4u8; 32]),
            Some("ab".repeat(32)),
        )
    }

    #[test]
    fn a_token_round_trips() {
        let rendered = token().render();
        assert!(rendered.starts_with(PREFIX));
        let parsed = LinkToken::parse(&rendered).expect("parse");
        assert_eq!(parsed.id, "node-abc");
        assert_eq!(parsed.url, "https://node.example.com:8443");
        assert_eq!(parsed.fp.as_deref(), Some("ab".repeat(32).as_str()));
    }

    #[test]
    fn malformed_tokens_are_refused() {
        assert!(LinkToken::parse("nonsense").is_err());
        assert!(LinkToken::parse("telemt-node:!!!").is_err());

        let mut short_key = token();
        short_key.key = encode(&[1u8; 8]);
        assert!(LinkToken::parse(&short_key.render()).is_err());

        let mut bad_version = token();
        bad_version.v = 99;
        assert!(LinkToken::parse(&bad_version.render()).is_err());

        let mut bad_fingerprint = token();
        bad_fingerprint.fp = Some("zz".to_string());
        assert!(LinkToken::parse(&bad_fingerprint.render()).is_err());
    }

    #[test]
    fn plaintext_urls_are_only_allowed_on_loopback() {
        assert!(validate_url("https://node.example.com").is_ok());
        assert!(validate_url("http://127.0.0.1:8443").is_ok());
        assert!(validate_url("http://node.example.com").is_err());
        assert!(validate_url("https://node.example.com/panel").is_err());
        assert!(validate_url("ftp://node.example.com").is_err());
    }
}
