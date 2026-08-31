//! Switches for the runtime behaviour this fork adds to telemt.
//!
//! Each field turns off one fork-only deviation and restores what stock telemt
//! does. They all default to `true`: an operator who never writes
//! `[fork.runtime]` keeps the behaviour this fork has always had.
//!
//! Three fork deviations are deliberately absent here because they are
//! structural rather than behavioural, and an "off" for them would mean
//! reinstating code this fork deleted rather than taking a different branch:
//!
//! - the single process-wide Direct copy-buffer controller,
//! - generation-scoped `dns_overrides` (the process-global override table is
//!   gone, the snapshot is an explicit parameter everywhere),
//! - the reload ticket's `Drop` safety net.
//!
//! Product identification is likewise not switchable: TELEMT PUBLIC LICENSE 3.3
//! §3 requires a modified build to identify itself as unofficial.

use serde::{Deserialize, Serialize};

use super::defaults::default_true;

/// Fork-only runtime behaviour, one switch per deviation from telemt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkRuntimeConfig {
    /// Serves every generation from one process-wide connection semaphore.
    ///
    /// Off, each generation mints its own semaphore like telemt, so a drain
    /// reload admits up to twice `server.max_connections` while both
    /// generations are live.
    #[serde(default = "default_true")]
    pub process_admission_budget: bool,

    /// Shares one relay buffer pool across generations.
    ///
    /// Off, each generation allocates its own pool like telemt, doubling
    /// committed relay memory for the length of a drain.
    #[serde(default = "default_true")]
    pub process_buffer_pool: bool,

    /// Seeds `telemt_uptime_seconds` with the process start instant.
    ///
    /// Off, a reload reseeds it like telemt and uptime restarts from zero.
    #[serde(default = "default_true")]
    pub process_uptime_clock: bool,

    /// Enables `DELETE /v1/system/reload/{id}` and the reload cancellation token.
    ///
    /// Off, a long drain holds the single reload slot until it finishes.
    #[serde(default = "default_true")]
    pub reload_cancel: bool,

    /// Bounds runtime preparation, quiesce, and middle-end teardown.
    ///
    /// Off, those steps retry without a deadline like telemt.
    #[serde(default = "default_true")]
    pub reload_deadlines: bool,

    /// Restores the previous config file when a `failure_policy = "rollback"`
    /// reload fails.
    ///
    /// Off, only the candidate runtime is discarded and the written config
    /// stays on disk, which is what telemt does.
    #[serde(default = "default_true")]
    pub reload_config_rollback: bool,

    /// Runs `ProxyConfig::validate()` on a reload candidate before accepting it.
    ///
    /// Off, only the loader's own checks run, so a config that is fatal at
    /// startup can be installed by a reload.
    #[serde(default = "default_true")]
    pub reload_validate_candidate: bool,

    /// Serialises the stable `error_kind` slug next to `error` in reload status.
    ///
    /// Off, the status JSON carries telemt's shape: a message only.
    #[serde(default = "default_true")]
    pub reload_error_kind: bool,

    /// Seeds a new generation's config watcher with the snapshot the reload
    /// was built from.
    ///
    /// Off, the watcher re-reads the file when it starts, which loses a write
    /// that lands while the runtime is being prepared.
    #[serde(default = "default_true")]
    pub reload_config_snapshot_hash: bool,

    /// Cancels middle-end writer tasks and drops their sockets on teardown.
    ///
    /// Off, retired writers are only signalled, and their connections
    /// accumulate across reloads as telemt's do.
    #[serde(default = "default_true")]
    pub me_writer_teardown: bool,

    /// Returns a retired TLS-front cache's full-certificate reservations to the
    /// process-wide budget.
    ///
    /// Off, the budget ratchets across reloads like telemt's.
    #[serde(default = "default_true")]
    pub tls_front_cache_budget_release: bool,

    /// Re-reconciles SYN-limiter rules on a cutover and on a hot reload.
    ///
    /// Off, rules are reconciled once at startup, as telemt does.
    #[serde(default = "default_true")]
    pub synlimit_generation_reconciler: bool,

    /// Unbinds listening sockets as the first shutdown action.
    ///
    /// Off, listeners are stopped late, so the port keeps completing TCP
    /// handshakes and resetting them for the whole shutdown window.
    #[serde(default = "default_true")]
    pub shutdown_unbind_listeners_first: bool,

    /// Exports `telemt_session_admission_closed_total`.
    #[serde(default = "default_true")]
    pub session_admission_closed_metric: bool,

    /// Drops a deleted user's process-scoped quota and stats.
    ///
    /// Off, a re-created username starts pre-charged, as it does on telemt.
    #[serde(default = "default_true")]
    pub user_delete_forgets_quota: bool,

    /// Keeps an operator's `RUST_LOG` filter across a reload.
    ///
    /// Off, a reload re-derives the filter from `general.log_level` alone.
    #[serde(default = "default_true")]
    pub rust_log_survives_reload: bool,
}

impl Default for ForkRuntimeConfig {
    fn default() -> Self {
        Self {
            process_admission_budget: true,
            process_buffer_pool: true,
            process_uptime_clock: true,
            reload_cancel: true,
            reload_deadlines: true,
            reload_config_rollback: true,
            reload_validate_candidate: true,
            reload_error_kind: true,
            reload_config_snapshot_hash: true,
            me_writer_teardown: true,
            tls_front_cache_budget_release: true,
            synlimit_generation_reconciler: true,
            shutdown_unbind_listeners_first: true,
            session_admission_closed_metric: true,
            user_delete_forgets_quota: true,
            rust_log_survives_reload: true,
        }
    }
}
