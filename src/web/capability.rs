//! Bridge capability derivation, hostname rules, and carrier token primitives.
//!
//! The capability is `HMAC-SHA256(secret, "tdesktop-web-proxy-bridge-v1\n" + host)`
//! rendered as unpadded base64url. Clients derive it locally from the hostname
//! and their MTProxy secret, so the raw secret never travels to the bridge page.

use crate::crypto::hash::{sha256, sha256_hmac};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Frozen v1 domain-separation label. The name is retained for compatibility
/// and does not restrict the protocol to one Telegram client.
const CAPABILITY_CONTEXT: &str = "tdesktop-web-proxy-bridge-v1\n";

/// Canonical length of a base64url-encoded 32-byte capability or token.
pub(crate) const CAPABILITY_TEXT_LEN: usize = 43;

/// Raw byte length of a capability, bootstrap token, or session token.
pub(crate) const TOKEN_BYTES: usize = 32;

/// Length of an MTProxy secret key, before any prefix or fronted domain.
pub(crate) const SECRET_BYTES: usize = 16;

/// Derives the bridge capability for one hostname and secret.
pub(crate) fn derive_capability(host: &str, secret: &[u8]) -> [u8; 32] {
    let mut message = Vec::with_capacity(CAPABILITY_CONTEXT.len() + host.len());
    message.extend_from_slice(CAPABILITY_CONTEXT.as_bytes());
    message.extend_from_slice(host.as_bytes());
    sha256_hmac(secret, &message)
}

/// Renders a capability or token as canonical unpadded base64url.
pub(crate) fn encode_token(value: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(value)
}

/// Decodes canonical unpadded base64url into exactly 32 bytes.
///
/// Non-canonical encodings (padding, alternative trailing bits, wrong length)
/// are rejected so a token has exactly one textual representation.
pub(crate) fn decode_token(value: &str) -> Option<[u8; TOKEN_BYTES]> {
    if value.len() != CAPABILITY_TEXT_LEN {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(value.as_bytes()).ok()?;
    if decoded.len() != TOKEN_BYTES || URL_SAFE_NO_PAD.encode(&decoded) != value {
        return None;
    }
    let mut result = [0u8; TOKEN_BYTES];
    result.copy_from_slice(&decoded);
    Some(result)
}

/// Hashes a bearer token into its lookup key.
///
/// Tokens are stored hashed so a memory disclosure never yields a usable
/// bearer, mirroring the reference relay.
pub(crate) fn token_hash(token: &[u8; TOKEN_BYTES]) -> [u8; 32] {
    sha256(token)
}

/// Decodes an MTProxy secret in hex or base64url form.
///
/// Accepts 16 raw bytes, or 17 bytes when the leading `dd` random-padding byte
/// is retained. `ee` fake-TLS secrets are accepted with the same 17-byte shape
/// because a client may derive its capability from the secret it was given.
pub(crate) fn decode_secret(value: &str) -> Result<Vec<u8>, &'static str> {
    const INVALID: &str = "secret must decode to 16 bytes, optionally prefixed with dd";
    let value = value.trim();
    let decoded = if value.len() == 32 || value.len() == 34 {
        hex::decode(value).map_err(|_| INVALID)?
    } else {
        URL_SAFE_NO_PAD
            .decode(value.as_bytes())
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(value.as_bytes()))
            .map_err(|_| INVALID)?
    };
    if decoded.len() != SECRET_BYTES && decoded.len() != SECRET_BYTES + 1 {
        return Err(INVALID);
    }
    // A WEB client accepts only a plain or `dd` random-padding secret. `ee`
    // fake-TLS secrets are rejected client-side, because a stock MTProxy behind
    // the relay would expect an inner TLS-emulation record that this raw
    // carrier deliberately does not add.
    if decoded.len() == SECRET_BYTES + 1 && decoded[0] != 0xdd {
        return Err("17-byte secret must use the dd prefix");
    }
    Ok(decoded)
}

/// Validates a public hostname in lowercase ASCII/IDNA A-label form.
pub(crate) fn validate_hostname(host: &str) -> Result<(), &'static str> {
    if host.is_empty()
        || host.len() > 253
        || host.ends_with('.')
        || host.contains([':', '/', '@', '?', '#', '[', ']'])
    {
        return Err("must be a DNS hostname without scheme, port, path, query, or trailing dot");
    }
    if !host.contains('.') || host.parse::<std::net::IpAddr>().is_ok() {
        return Err("IP addresses and single-label names are not allowed");
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 || label.starts_with('-') || label.ends_with('-') {
            return Err("invalid DNS label");
        }
        for byte in label.bytes() {
            let ok = byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-';
            if !ok {
                return Err("hostname must be lowercase ASCII/IDNA");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_matches_protocol_vectors() {
        let host = "proxy.example.com";
        let secret = hex::decode("000102030405060708090a0b0c0d0e0f").expect("hex");
        assert_eq!(
            encode_token(&derive_capability(host, &secret)),
            "MHLEY5PmW1GWqJkSrlmJpvJUiLhBH_QKy6yKg8a0JPk"
        );
        let dd_secret = hex::decode("dd000102030405060708090a0b0c0d0e0f").expect("hex");
        assert_eq!(
            encode_token(&derive_capability(host, &dd_secret)),
            "IpJrt3e7sKtzPyoXy6w-Zj6GGEvsvclN66JzQEfPYLA"
        );
    }

    #[test]
    fn token_encoding_is_canonical() {
        let raw = [7u8; TOKEN_BYTES];
        let text = encode_token(&raw);
        assert_eq!(text.len(), CAPABILITY_TEXT_LEN);
        assert_eq!(decode_token(&text), Some(raw));
        assert_eq!(decode_token(&format!("{text}=")), None);
        assert_eq!(decode_token("short"), None);
    }

    #[test]
    fn secret_decoding_accepts_documented_forms() {
        assert_eq!(
            decode_secret("000102030405060708090a0b0c0d0e0f")
                .unwrap()
                .len(),
            16
        );
        assert_eq!(
            decode_secret("dd000102030405060708090a0b0c0d0e0f")
                .unwrap()
                .len(),
            17
        );
        // A WEB client rejects `ee` fake-TLS secrets, so a capability derived
        // from one could never be presented.
        assert!(decode_secret("ee000102030405060708090a0b0c0d0e0f").is_err());
        assert!(decode_secret("ab000102030405060708090a0b0c0d0e0f").is_err());
        assert!(decode_secret("00").is_err());
    }

    #[test]
    fn hostname_rules_reject_non_canonical_input() {
        assert!(validate_hostname("proxy.example.com").is_ok());
        assert!(validate_hostname("Proxy.example.com").is_err());
        assert!(validate_hostname("example").is_err());
        assert!(validate_hostname("127.0.0.1").is_err());
        assert!(validate_hostname("proxy.example.com.").is_err());
        assert!(validate_hostname("proxy.example.com:443").is_err());
        assert!(validate_hostname("-bad.example.com").is_err());
    }
}
