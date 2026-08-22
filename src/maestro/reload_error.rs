//! Failure classification and rollback payload for in-process runtime reloads.

use std::fmt;
use std::path::PathBuf;

/// Why a reload did not reach `Succeeded`.
///
/// Callers need to tell "the config is invalid" apart from "the network probe
/// timed out" apart from "the operator cancelled it"; a bare `String` collapsed
/// all of those into one opaque field and left `failure_policy` with nothing to
/// discriminate on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReloadError {
    /// The candidate configuration failed validation.
    ConfigInvalid(String),
    /// `network.dns_overrides` could not be parsed for the candidate.
    DnsOverrides(String),
    /// The network/NAT probe failed.
    Probe(String),
    /// TLS front bootstrap did not reach a usable state.
    TlsBootstrap(String),
    /// Middle-End is required by config but the pool never became ready.
    MiddleEndUnavailable(String),
    /// The on-disk revision moved while the candidate was being prepared.
    RevisionChanged(String),
    /// Preparation exceeded its hard deadline.
    Timeout(String),
    /// Process shutdown or an operator cancel aborted the reload.
    Cancelled,
    /// Anything the supervisor could not classify.
    Internal(String),
}

impl ReloadError {
    /// Stable machine-readable slug exported alongside the human message.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::ConfigInvalid(_) => "config_invalid",
            Self::DnsOverrides(_) => "dns_overrides_invalid",
            Self::Probe(_) => "probe_failed",
            Self::TlsBootstrap(_) => "tls_bootstrap_failed",
            Self::MiddleEndUnavailable(_) => "middle_end_unavailable",
            Self::RevisionChanged(_) => "revision_changed",
            Self::Timeout(_) => "timeout",
            Self::Cancelled => "cancelled",
            Self::Internal(_) => "internal",
        }
    }
}

impl fmt::Display for ReloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfigInvalid(message)
            | Self::DnsOverrides(message)
            | Self::Probe(message)
            | Self::TlsBootstrap(message)
            | Self::MiddleEndUnavailable(message)
            | Self::RevisionChanged(message)
            | Self::Timeout(message)
            | Self::Internal(message) => f.write_str(message),
            Self::Cancelled => f.write_str("reload cancelled"),
        }
    }
}

/// Pre-patch config snapshot a `failure_policy=rollback` reload must restore.
///
/// `PATCH /v1/config?reload=…` commits the merged config to disk *before* the
/// reload runs, and the live generation's own file watcher hot-applies it. A
/// rolled-back reload that only discarded the candidate runtime would leave the
/// proxy enforcing the new config and would survive the next restart, so the
/// supervisor puts the previous bytes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigRollback {
    /// Config file that was rewritten.
    pub(crate) path: PathBuf,
    /// Exact bytes the file held before the patch.
    pub(crate) previous_content: String,
    /// Revision this reload wrote. The restore is skipped when the file no
    /// longer matches it, so a concurrent editor is never clobbered.
    pub(crate) written_revision: String,
}
