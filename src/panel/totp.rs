//! RFC 6238 time-based one-time passwords for operator second factors.
//!
//! Authenticator applications interoperate on exactly one profile in practice:
//! HMAC-SHA1, a 30-second step, and six digits. That profile is implemented
//! here rather than made configurable, because every knob is a way for an
//! operator to enrol a secret their application cannot read back.

use hmac::{Hmac, Mac};
use sha1::Sha1;
use subtle::ConstantTimeEq;

use crate::crypto::SecureRandom;

type HmacSha1 = Hmac<Sha1>;

/// Seconds covered by one code.
pub(crate) const STEP_SECS: u64 = 30;

/// Digits in one code.
pub(crate) const DIGITS: u32 = 6;

/// Steps accepted on either side of the current one.
///
/// One step of tolerance covers ordinary client clock drift; more would widen
/// the window a stolen code stays usable in.
const SKEW_STEPS: i64 = 1;

/// Bytes of shared secret, matching the 160-bit HMAC-SHA1 block.
const SECRET_BYTES: usize = 20;

/// RFC 4648 base32 alphabet used by `otpauth://` secrets.
const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Mints a fresh base32 TOTP secret.
pub(crate) fn generate_secret(random: &SecureRandom) -> String {
    let mut bytes = [0u8; SECRET_BYTES];
    random.fill(&mut bytes);
    base32_encode(&bytes)
}

/// Renders the `otpauth://` URI an authenticator application scans.
pub(crate) fn provisioning_uri(secret: &str, account: &str, issuer: &str) -> String {
    format!(
        "otpauth://totp/{issuer}:{account}?secret={secret}&issuer={issuer}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
        issuer = percent_encode(issuer),
        account = percent_encode(account),
        secret = secret,
    )
}

/// Verifies one submitted code against the secret at the given unix time.
pub(crate) fn verify(secret: &str, code: &str, unix_secs: u64) -> bool {
    let Some(key) = base32_decode(secret) else {
        return false;
    };
    let code = code.trim();
    if code.len() != DIGITS as usize || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let step = (unix_secs / STEP_SECS) as i64;
    let mut accepted = false;
    for offset in -SKEW_STEPS..=SKEW_STEPS {
        let counter = match step.checked_add(offset) {
            Some(counter) if counter >= 0 => counter as u64,
            _ => continue,
        };
        let expected = format_code(&key, counter);
        // Every candidate step is evaluated: an early return would leak which
        // step matched through the response time.
        accepted |= bool::from(expected.as_bytes().ct_eq(code.as_bytes()));
    }
    accepted
}

/// Computes the code for one counter value.
fn format_code(key: &[u8], counter: u64) -> String {
    let mut mac = HmacSha1::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let truncated = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);
    let modulus = 10u32.pow(DIGITS);
    format!("{:0width$}", truncated % modulus, width = DIGITS as usize)
}

/// Encodes bytes as unpadded RFC 4648 base32.
fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    for chunk in bytes.chunks(5) {
        let mut buffer = [0u8; 5];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let bits = u64::from_be_bytes([
            0, 0, 0, buffer[0], buffer[1], buffer[2], buffer[3], buffer[4],
        ]);
        let symbols = match chunk.len() {
            1 => 2,
            2 => 4,
            3 => 5,
            4 => 7,
            _ => 8,
        };
        for index in 0..symbols {
            let shift = 35 - index * 5;
            out.push(BASE32_ALPHABET[((bits >> shift) & 0x1f) as usize] as char);
        }
    }
    out
}

/// Decodes unpadded RFC 4648 base32, tolerating lowercase and spacing.
fn base32_decode(value: &str) -> Option<Vec<u8>> {
    let mut accumulator: u64 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(value.len() * 5 / 8);
    for symbol in value.chars() {
        if symbol == ' ' || symbol == '-' || symbol == '=' {
            continue;
        }
        let upper = symbol.to_ascii_uppercase() as u8;
        let index = BASE32_ALPHABET.iter().position(|entry| *entry == upper)?;
        accumulator = (accumulator << 5) | index as u64;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Percent-encodes the label components of an `otpauth://` URI.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 test vectors pin the encoder against every partial-chunk case.
    #[test]
    fn base32_round_trips_rfc4648_vectors() {
        for (plain, encoded) in [
            ("f", "MY"),
            ("fo", "MZXQ"),
            ("foo", "MZXW6"),
            ("foob", "MZXW6YQ"),
            ("fooba", "MZXW6YTB"),
            ("foobar", "MZXW6YTBOI"),
        ] {
            assert_eq!(base32_encode(plain.as_bytes()), encoded);
            assert_eq!(
                base32_decode(encoded).as_deref(),
                Some(plain.as_bytes()),
                "{encoded}"
            );
        }
    }

    /// RFC 6238 appendix B, SHA-1 rows, truncated to the six digits used here.
    #[test]
    fn matches_rfc6238_reference_codes() {
        let secret = base32_encode(b"12345678901234567890");
        for (time, expected) in [
            (59u64, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
        ] {
            let key = base32_decode(&secret).expect("secret decodes");
            assert_eq!(format_code(&key, time / STEP_SECS), expected, "t={time}");
        }
    }

    #[test]
    fn verification_tolerates_one_step_of_drift_only() {
        let secret = base32_encode(b"12345678901234567890");
        let key = base32_decode(&secret).expect("secret decodes");
        let now = 1_111_111_111u64;
        let step = now / STEP_SECS;
        assert!(verify(&secret, &format_code(&key, step), now));
        assert!(verify(&secret, &format_code(&key, step - 1), now));
        assert!(verify(&secret, &format_code(&key, step + 1), now));
        assert!(!verify(&secret, &format_code(&key, step + 2), now));
        assert!(!verify(&secret, "0000000", now));
        assert!(!verify(&secret, "abcdef", now));
    }

    #[test]
    fn provisioning_uri_escapes_label_components() {
        let uri = provisioning_uri("ABCD", "op erator", "telemt panel");
        assert!(uri.contains("telemt%20panel:op%20erator"));
        assert!(uri.contains("secret=ABCD"));
    }
}
