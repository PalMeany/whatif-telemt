//! Secret-form policy shared between the WEB transport and the MTProto handshake.
//!
//! Telemt's `[web]` configuration surface lives in `crate::config::web`; this
//! module keeps only the enum the handshake needs, so that one logical WEB
//! stream can be pinned to the secret form its profile was issued for.

use serde::{Deserialize, Serialize};

/// Client-facing secret representation used to derive a WEB capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSecretMode {
    /// Use the existing 16-byte access secret without a prefix.
    Plain,
    /// Prefix the existing access secret with `0xdd` for capability derivation.
    Dd,
}
