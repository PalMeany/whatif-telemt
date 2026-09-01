//! Audit record shape and the hash that chains one record to the last.
//!
//! Kept apart from the log itself so the tamper-evidence rules — field bounds,
//! the pre-image encoding, and the genesis value — read as one unit rather than
//! being interleaved with file rotation and retention.

use serde::{Deserialize, Serialize};

use crate::crypto::sha256;

/// Genesis value of the chain, used as the predecessor of the first record.
pub(super) const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// One audit record as written to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AuditRecord {
    /// Monotonic sequence number across the whole chain.
    pub(crate) seq: u64,
    /// Unix seconds the action completed at.
    pub(crate) ts: u64,
    /// Login name of the operator, or a machine actor such as `cluster`.
    pub(crate) actor: String,
    /// Stable operator identifier, when the actor is an operator.
    #[serde(default)]
    pub(crate) actor_id: String,
    /// Machine-readable action name, for example `user.create`.
    pub(crate) action: String,
    /// Object the action applied to.
    #[serde(default)]
    pub(crate) target: String,
    /// Node the action was routed to.
    #[serde(default)]
    pub(crate) node: String,
    /// `ok` or a machine-readable failure code.
    pub(crate) result: String,
    /// Source address the request arrived from.
    #[serde(default)]
    pub(crate) address: String,
    /// Free-form detail; never carries secrets.
    #[serde(default)]
    pub(crate) detail: String,
    /// Hash of the preceding record.
    pub(crate) prev: String,
    /// Hash of this record.
    pub(crate) hash: String,
}

/// Fields that go into a record before its hash is computed.
#[derive(Debug, Clone, Default)]
pub(crate) struct AuditEntry {
    /// Login name of the operator, or a machine actor.
    pub(crate) actor: String,
    /// Stable operator identifier, when the actor is an operator.
    pub(crate) actor_id: String,
    /// Machine-readable action name.
    pub(crate) action: String,
    /// Object the action applied to.
    pub(crate) target: String,
    /// Node the action was routed to.
    pub(crate) node: String,
    /// `ok` or a machine-readable failure code.
    pub(crate) result: String,
    /// Source address the request arrived from.
    pub(crate) address: String,
    /// Free-form detail; never carries secrets.
    pub(crate) detail: String,
}

/// Outcome of a chain verification pass.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuditVerification {
    /// Records read before the pass stopped.
    pub(crate) checked: u64,
    /// True when the whole file verified.
    pub(crate) valid: bool,
    /// Sequence number of the first record that broke the chain.
    pub(crate) broken_at: Option<u64>,
}

/// Longest value any single audit field may carry.
///
/// Some fields are attacker-influenced — a login records the submitted account
/// name whether or not it exists — and an unbounded one turns the audit log into
/// a write amplifier for anything that can reach the login route.
const MAX_FIELD_LEN: usize = 512;

/// Turns an entry into a sealed record.
pub(super) fn seal(entry: AuditEntry, seq: u64, now: u64, previous: String) -> AuditRecord {
    let mut record = AuditRecord {
        seq,
        ts: now,
        actor: clamp(entry.actor),
        actor_id: clamp(entry.actor_id),
        action: clamp(entry.action),
        target: clamp(entry.target),
        node: clamp(entry.node),
        result: clamp(entry.result),
        address: clamp(entry.address),
        detail: clamp(entry.detail),
        prev: previous.clone(),
        hash: String::new(),
    };
    record.hash = chain_hash(&record, &previous);
    record
}

/// Truncates one field to [`MAX_FIELD_LEN`] on a character boundary.
fn clamp(value: String) -> String {
    if value.len() <= MAX_FIELD_LEN {
        return value;
    }
    let mut end = MAX_FIELD_LEN;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// Computes the hash covering one record and its predecessor.
///
/// The pre-image is field-ordered by hand rather than taken from the serialized
/// JSON: a serializer that reorders or re-spaces its output would otherwise
/// invalidate every previously written chain.
///
/// Each field is length-prefixed rather than newline-separated. Some fields
/// carry text the caller chose — a submitted account name, a relayed path — and
/// a separator that can appear inside a value makes the encoding ambiguous: two
/// different records could otherwise hash identically, which is exactly the
/// property the chain exists to rule out.
pub(super) fn chain_hash(record: &AuditRecord, previous: &str) -> String {
    let mut preimage = Vec::with_capacity(256);
    for field in [
        previous,
        &record.seq.to_string(),
        &record.ts.to_string(),
        &record.actor,
        &record.actor_id,
        &record.action,
        &record.target,
        &record.node,
        &record.result,
        &record.address,
        &record.detail,
    ] {
        preimage.extend_from_slice(field.len().to_string().as_bytes());
        preimage.push(b':');
        preimage.extend_from_slice(field.as_bytes());
    }
    hex::encode(sha256(&preimage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_separators_cannot_be_forged_from_field_content() {
        // Two records that a newline-separated pre-image would collapse into
        // one: the separator is simply moved inside a value.
        let split = seal(
            AuditEntry {
                actor: "root".to_string(),
                action: "auth.login".to_string(),
                ..AuditEntry::default()
            },
            1,
            1_000,
            GENESIS.to_string(),
        );
        let merged = seal(
            AuditEntry {
                actor: "root\nauth.login".to_string(),
                action: String::new(),
                ..AuditEntry::default()
            },
            1,
            1_000,
            GENESIS.to_string(),
        );
        assert_ne!(split.hash, merged.hash);
    }

    #[test]
    fn attacker_influenced_fields_are_bounded() {
        let record = seal(
            AuditEntry {
                actor: "a".repeat(4_000),
                action: "auth.login".to_string(),
                ..AuditEntry::default()
            },
            1,
            1_000,
            GENESIS.to_string(),
        );
        assert_eq!(record.actor.len(), MAX_FIELD_LEN);
    }
}
