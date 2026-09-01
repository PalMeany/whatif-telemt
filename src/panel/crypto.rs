//! Credential and token primitives used by the panel.
//!
//! Everything here is built from crates the proxy already depends on: PBKDF2 is
//! assembled from `hmac`/`sha2` rather than pulled in as a new dependency, and
//! the encodings are the unpadded base64url the rest of the control plane uses.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

use crate::crypto::SecureRandom;

type HmacSha256 = Hmac<Sha256>;

/// Length of every random secret the panel mints.
pub(crate) const SECRET_LEN: usize = 32;

/// Derived key length of the password hash.
pub(crate) const DERIVED_KEY_LEN: usize = 32;

/// Salt length of the password hash.
pub(crate) const SALT_LEN: usize = 16;

/// Encodes bytes as unpadded base64url.
pub(crate) fn encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes canonical unpadded base64url, rejecting any non-canonical form.
///
/// Canonicality matters for the cluster nonce: two spellings of the same bytes
/// would otherwise occupy two replay-window slots and let one signature be
/// replayed once per spelling.
pub(crate) fn decode(value: &str) -> Option<Vec<u8>> {
    let decoded = URL_SAFE_NO_PAD.decode(value).ok()?;
    (URL_SAFE_NO_PAD.encode(&decoded) == value).then_some(decoded)
}

/// Mints one random secret.
pub(crate) fn random_secret(random: &SecureRandom) -> [u8; SECRET_LEN] {
    let mut bytes = [0u8; SECRET_LEN];
    random.fill(&mut bytes);
    bytes
}

/// Mints one random secret already encoded for transport.
pub(crate) fn random_token(random: &SecureRandom) -> String {
    let mut bytes = random_secret(random);
    let token = encode(&bytes);
    bytes.zeroize();
    token
}

/// Mints a human-typable password from an unambiguous alphabet.
///
/// The alphabet omits the character pairs an operator reads back wrongly from a
/// terminal (`0`/`O`, `1`/`l`/`I`), because the bootstrap credential is copied
/// by hand exactly once and a misread looks like a broken install.
pub(crate) fn random_password(random: &SecureRandom, length: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut out = String::with_capacity(length);
    let mut buffer = vec![0u8; length * 2];
    random.fill(&mut buffer);
    let mut index = 0usize;
    while out.len() < length {
        if index >= buffer.len() {
            random.fill(&mut buffer);
            index = 0;
        }
        // Rejection sampling keeps the distribution flat: taking a modulus of a
        // byte over a 56-character alphabet would bias the first 32 characters.
        let byte = buffer[index];
        index += 1;
        let limit = (256 / ALPHABET.len()) * ALPHABET.len();
        if (byte as usize) < limit {
            out.push(ALPHABET[byte as usize % ALPHABET.len()] as char);
        }
    }
    buffer.zeroize();
    out
}

/// Constant-time equality over two byte strings of any length.
pub(crate) fn secure_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.ct_eq(right).into()
}

/// PBKDF2-HMAC-SHA256 with a single derived block.
///
/// `dkLen` equals the hash length, so the standard block loop collapses to one
/// iteration of `F`. This is CPU-bound by design and must not run on a runtime
/// worker; callers hand it to `spawn_blocking`.
pub(crate) fn pbkdf2_sha256(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
) -> [u8; DERIVED_KEY_LEN] {
    let mut mac = HmacSha256::new_from_slice(password).expect("HMAC accepts any key length");
    mac.update(salt);
    mac.update(&1u32.to_be_bytes());
    let mut block = mac.finalize().into_bytes();
    let mut derived = block;
    for _ in 1..iterations.max(1) {
        let mut mac = HmacSha256::new_from_slice(password).expect("HMAC accepts any key length");
        mac.update(&block);
        block = mac.finalize().into_bytes();
        for (accumulated, next) in derived.iter_mut().zip(block.iter()) {
            *accumulated ^= next;
        }
    }
    derived.into()
}

/// HMAC-SHA256 over one message.
pub(crate) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    crate::crypto::sha256_hmac(key, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_rejects_non_canonical_input() {
        let encoded = encode(&[1u8, 2, 3]);
        assert_eq!(decode(&encoded).as_deref(), Some(&[1u8, 2, 3][..]));
        assert_eq!(decode("AQID="), None);
        assert_eq!(decode("!!!"), None);
    }

    #[test]
    fn pbkdf2_matches_rfc6070_style_vector() {
        // RFC 6070 uses HMAC-SHA1; the SHA-256 analogue of its first vector is
        // the widely published value below, and it pins both the block layout
        // and the XOR accumulation.
        let derived = pbkdf2_sha256(b"password", b"salt", 1);
        assert_eq!(
            hex::encode(derived),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        let derived = pbkdf2_sha256(b"password", b"salt", 2);
        assert_eq!(
            hex::encode(derived),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
    }

    #[test]
    fn secure_eq_is_length_aware() {
        assert!(secure_eq(b"abc", b"abc"));
        assert!(!secure_eq(b"abc", b"abd"));
        assert!(!secure_eq(b"abc", b"abcd"));
    }

    #[test]
    fn generated_password_uses_the_unambiguous_alphabet() {
        let random = SecureRandom::new();
        let password = random_password(&random, 24);
        assert_eq!(password.chars().count(), 24);
        assert!(!password.contains('0'));
        assert!(!password.contains('O'));
        assert!(!password.contains('l'));
        assert!(!password.contains('1'));
    }
}
