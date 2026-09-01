//! Operator password hashing and verification.
//!
//! PBKDF2 at a production work factor takes hundreds of milliseconds, which is
//! the point of it. Every call therefore goes through `spawn_blocking`: running
//! it on a runtime worker would stall every other task on that thread for the
//! duration of one login.

use crate::crypto::SecureRandom;
use crate::error::{ProxyError, Result};

use super::crypto::{SALT_LEN, decode, encode, pbkdf2_sha256, secure_eq};
use super::store::{PASSWORD_ALGORITHM, PasswordRecord};

/// Outcome of checking a submitted password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verification {
    /// The password matched and the stored record uses the current work factor.
    Match,
    /// The password matched but the record should be rehashed on the way out.
    MatchNeedsRehash,
    /// The password did not match.
    Mismatch,
}

/// Derives a fresh credential record for a plaintext password.
pub(crate) async fn hash(
    password: &str,
    iterations: u32,
    random: &SecureRandom,
    now: u64,
) -> Result<PasswordRecord> {
    let mut salt = [0u8; SALT_LEN];
    random.fill(&mut salt);
    let owned = password.to_string();
    let derived =
        tokio::task::spawn_blocking(move || pbkdf2_sha256(owned.as_bytes(), &salt, iterations))
            .await
            .map_err(|error| ProxyError::Internal(format!("password hashing failed: {error}")))?;
    Ok(PasswordRecord {
        algorithm: PASSWORD_ALGORITHM.to_string(),
        iterations,
        salt: encode(&salt),
        hash: encode(&derived),
        updated_at: now,
    })
}

/// Checks a submitted password against a stored record.
pub(crate) async fn verify(
    record: &PasswordRecord,
    password: &str,
    current_iterations: u32,
) -> Result<Verification> {
    if record.algorithm != PASSWORD_ALGORITHM {
        return Ok(Verification::Mismatch);
    }
    let (Some(salt), Some(expected)) = (decode(&record.salt), decode(&record.hash)) else {
        return Ok(Verification::Mismatch);
    };
    let iterations = record.iterations;
    let owned = password.to_string();
    let derived =
        tokio::task::spawn_blocking(move || pbkdf2_sha256(owned.as_bytes(), &salt, iterations))
            .await
            .map_err(|error| {
                ProxyError::Internal(format!("password verification failed: {error}"))
            })?;
    if !secure_eq(&derived, &expected) {
        return Ok(Verification::Mismatch);
    }
    if record.iterations < current_iterations {
        return Ok(Verification::MatchNeedsRehash);
    }
    Ok(Verification::Match)
}

/// Rejects a password that does not meet the configured policy.
///
/// The policy is deliberately length-first: composition rules push operators
/// toward predictable substitutions without adding meaningful entropy, whereas
/// a length floor cannot be gamed.
pub(crate) fn check_policy(password: &str, min_length: usize) -> std::result::Result<(), String> {
    let length = password.chars().count();
    if length < min_length {
        return Err(format!(
            "password must contain at least {min_length} characters"
        ));
    }
    if length > 1_024 {
        return Err("password must contain at most 1024 characters".to_string());
    }
    if password.chars().all(char::is_whitespace) {
        return Err("password must not be blank".to_string());
    }
    if password.chars().any(|character| character.is_control()) {
        return Err("password must not contain control characters".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_hashed_password_verifies_and_a_wrong_one_does_not() {
        let random = SecureRandom::new();
        let record = hash("correct horse battery", 1_000, &random, 42)
            .await
            .expect("hash");
        assert_eq!(record.algorithm, PASSWORD_ALGORITHM);
        assert_eq!(record.updated_at, 42);
        assert_eq!(
            verify(&record, "correct horse battery", 1_000)
                .await
                .expect("verify"),
            Verification::Match
        );
        assert_eq!(
            verify(&record, "wrong", 1_000).await.expect("verify"),
            Verification::Mismatch
        );
    }

    #[tokio::test]
    async fn an_outdated_work_factor_asks_for_a_rehash() {
        let random = SecureRandom::new();
        let record = hash("correct horse battery", 1_000, &random, 0)
            .await
            .expect("hash");
        assert_eq!(
            verify(&record, "correct horse battery", 2_000)
                .await
                .expect("verify"),
            Verification::MatchNeedsRehash
        );
    }

    #[tokio::test]
    async fn a_corrupt_record_never_matches() {
        let random = SecureRandom::new();
        let mut record = hash("password", 1_000, &random, 0).await.expect("hash");
        record.hash = "not base64url!".to_string();
        assert_eq!(
            verify(&record, "password", 1_000).await.expect("verify"),
            Verification::Mismatch
        );
        record.algorithm = "bcrypt".to_string();
        assert_eq!(
            verify(&record, "password", 1_000).await.expect("verify"),
            Verification::Mismatch
        );
    }

    #[test]
    fn the_policy_is_length_first() {
        assert!(check_policy("short", 12).is_err());
        assert!(check_policy("            ", 12).is_err());
        assert!(check_policy("correct horse battery", 12).is_ok());
        assert!(check_policy("with\u{0}null bytes here", 12).is_err());
    }
}
