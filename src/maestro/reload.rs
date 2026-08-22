//! Reload request types and the process-scoped reload coordinator.
//!
//! Submissions are two-phase: [`ReloadControl::reserve`] takes the single reload
//! slot before the caller performs any side effect, and [`ReloadTicket::dispatch`]
//! hands the prepared command to the supervisor. Callers that write to disk
//! (`PATCH /v1/config?reload=…`) depend on that ordering — reserving afterwards
//! meant a shutting-down coordinator answered 503 with the write already
//! committed.
//!
//! Sibling modules: `reload_error` (failure taxonomy, rollback payload) and
//! `reload_status` (bounded status history, single-slot invariant).

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::config::ProxyConfig;

#[cfg(test)]
use super::reload_status::RELOAD_HISTORY_CAPACITY;
use super::reload_status::ReloadStatusStore;

// Re-exported so the failure taxonomy and the rollback payload keep a single
// import path for every caller.
pub(crate) use super::reload_error::{ConfigRollback, ReloadError};
pub(crate) use super::reload_status::{ReloadAccepted, ReloadPhase, ReloadStatus};

const RELOAD_COMMAND_CAPACITY: usize = 1;
const MAX_DRAIN_TIMEOUT_SECS: u64 = 3_600;

/// Session handling policy for an in-process runtime reload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReloadMode {
    #[default]
    Instant,
    Drain,
}

/// Failure policy applied during the activation barrier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReloadFailurePolicy {
    #[default]
    KeepNew,
    Rollback,
}

/// Request body accepted by the maestro reload endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReloadRequest {
    #[serde(default)]
    pub(crate) mode: ReloadMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timeout_secs: Option<u64>,
    #[serde(default)]
    pub(crate) failure_policy: ReloadFailurePolicy,
}

impl ReloadRequest {
    /// Validates mode-specific request parameters.
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        match (self.mode, self.timeout_secs) {
            (ReloadMode::Instant, None) => Ok(()),
            (ReloadMode::Instant, Some(_)) => Err("timeout_secs is only valid when mode is drain"),
            (ReloadMode::Drain, Some(1..=MAX_DRAIN_TIMEOUT_SECS)) => Ok(()),
            (ReloadMode::Drain, Some(_)) => Err("timeout_secs must be within 1..=3600"),
            (ReloadMode::Drain, None) => Err("timeout_secs is required when mode is drain"),
        }
    }

    /// Parses optional PATCH query parameters into a reload request.
    pub(crate) fn from_query(query: Option<&str>) -> Result<Option<Self>, String> {
        let Some(query) = query.filter(|query| !query.is_empty()) else {
            return Ok(None);
        };
        let mut mode = None;
        let mut timeout_secs = None;
        let mut failure_policy = None;
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "reload" if mode.is_none() => {
                    mode = Some(match value.as_ref() {
                        "instant" => ReloadMode::Instant,
                        "drain" => ReloadMode::Drain,
                        _ => return Err("reload must be instant or drain".to_string()),
                    });
                }
                "timeout_secs" if timeout_secs.is_none() => {
                    timeout_secs = Some(
                        value
                            .parse::<u64>()
                            .map_err(|_| "timeout_secs must be an integer".to_string())?,
                    );
                }
                "failure_policy" if failure_policy.is_none() => {
                    failure_policy = Some(match value.as_ref() {
                        "keep_new" => ReloadFailurePolicy::KeepNew,
                        "rollback" => ReloadFailurePolicy::Rollback,
                        _ => {
                            return Err("failure_policy must be keep_new or rollback".to_string());
                        }
                    });
                }
                "reload" | "timeout_secs" | "failure_policy" => {
                    return Err(format!("duplicate query parameter: {}", key));
                }
                _ => return Err(format!("unknown query parameter: {}", key)),
            }
        }
        let mode = mode.ok_or_else(|| "reload query parameter is required".to_string())?;
        let request = Self {
            mode,
            timeout_secs,
            failure_policy: failure_policy.unwrap_or_default(),
        };
        request.validate().map_err(str::to_string)?;
        Ok(Some(request))
    }
}

/// One accepted reload, delivered to the supervisor.
pub(crate) struct ReloadCommand {
    pub(crate) reload_id: u64,
    pub(crate) target_generation: u64,
    pub(crate) config: Arc<ProxyConfig>,
    /// Rendered hash of the on-disk snapshot `config` was loaded from.
    ///
    /// Seeds the new generation's config watcher so a write that lands during
    /// preparation is neither lost nor permanently suppressed.
    pub(crate) config_snapshot_hash: Option<u64>,
    pub(crate) config_revision: String,
    pub(crate) request: ReloadRequest,
    /// Pre-patch bytes to restore when this reload rolls back or fails under
    /// `failure_policy=rollback`. `None` for reloads that did not write the file.
    pub(crate) rollback: Option<ConfigRollback>,
    /// Fires when an operator cancels this reload via the API.
    pub(crate) cancel: CancellationToken,
}

/// Why a reload could not be accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReloadSubmitError {
    /// Another reload already holds the single slot.
    InProgress(u64),
    /// The coordinator is shutting down or its command channel is closed.
    MaestroUnavailable,
}

/// Process-scoped handle used by the API to submit and observe reloads.
#[derive(Clone)]
pub(crate) struct ReloadControl {
    command_tx: mpsc::Sender<ReloadCommand>,
    status_store: Arc<ReloadStatusStore>,
    active_generation: Arc<AtomicU64>,
}

/// Supervisor-side end of the reload command channel.
pub(crate) struct ReloadCommandReceiver {
    command_rx: mpsc::Receiver<ReloadCommand>,
}

/// Reserved-but-not-yet-dispatched reload slot.
///
/// Reserving before any side effect is what keeps `PATCH /v1/config?reload=…`
/// honest: the old code wrote the merged config to disk and only then discovered
/// the coordinator was shutting down, answering 503 with the write already
/// committed. Holding a ticket means the 503 is decided *first*.
///
/// The ticket must be consumed by [`Self::dispatch`] or [`Self::abandon`];
/// dropping it releases the slot from a background task so a `?` on an
/// intermediate step can never wedge the API at 409 forever.
#[must_use = "a reserved reload slot must be dispatched or abandoned"]
pub(crate) struct ReloadTicket {
    control: ReloadControl,
    reload_id: u64,
    target_generation: u64,
    request: ReloadRequest,
    cancel: CancellationToken,
    settled: bool,
}

impl ReloadTicket {
    /// Hands the prepared command to the supervisor.
    pub(crate) async fn dispatch(
        mut self,
        config: Arc<ProxyConfig>,
        config_snapshot_hash: Option<u64>,
        config_revision: String,
        rollback: Option<ConfigRollback>,
    ) -> Result<ReloadAccepted, ReloadSubmitError> {
        self.settled = true;
        self.control
            .status_store
            .update(self.reload_id, |status| {
                status.config_revision = config_revision.clone();
            })
            .await;
        let command = ReloadCommand {
            reload_id: self.reload_id,
            target_generation: self.target_generation,
            config,
            config_snapshot_hash,
            config_revision: config_revision.clone(),
            request: self.request.clone(),
            rollback,
            cancel: self.cancel.clone(),
        };
        if self.control.command_tx.try_send(command).is_err() {
            self.control
                .status_store
                .finish(
                    self.reload_id,
                    ReloadPhase::Failed,
                    Some(ReloadError::Internal(
                        "maestro command channel is closed".to_string(),
                    )),
                )
                .await;
            return Err(ReloadSubmitError::MaestroUnavailable);
        }
        Ok(ReloadAccepted {
            reload_id: self.reload_id,
            target_generation: self.target_generation,
            config_revision,
            state: ReloadPhase::Accepted,
            mode: self.request.mode,
            failure_policy: self.request.failure_policy,
        })
    }

    /// Releases the slot without running a reload.
    pub(crate) async fn abandon(mut self, error: ReloadError) {
        self.settled = true;
        self.control
            .status_store
            .finish(self.reload_id, ReloadPhase::Failed, Some(error))
            .await;
    }
}

impl Drop for ReloadTicket {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        // Safety net for early returns: never leave the single reload slot
        // reserved, or every later submission answers 409 for the process
        // lifetime.
        warn!(
            reload_id = self.reload_id,
            "Reload slot released without dispatch"
        );
        let status_store = self.control.status_store.clone();
        let reload_id = self.reload_id;
        tokio::spawn(async move {
            status_store
                .finish(
                    reload_id,
                    ReloadPhase::Failed,
                    Some(ReloadError::Internal(
                        "reload was reserved but never dispatched".to_string(),
                    )),
                )
                .await;
        });
    }
}

impl ReloadControl {
    /// Creates the process-scoped coordinator channel and status store.
    pub(crate) fn channel(initial_generation: u64) -> (Self, ReloadCommandReceiver) {
        let (command_tx, command_rx) = mpsc::channel(RELOAD_COMMAND_CAPACITY);
        (
            Self {
                command_tx,
                status_store: Arc::new(ReloadStatusStore::default()),
                active_generation: Arc::new(AtomicU64::new(initial_generation)),
            },
            ReloadCommandReceiver { command_rx },
        )
    }

    /// Reserves the single reload slot before the caller performs side effects.
    ///
    /// The target generation id is allocated under the same lock that publishes
    /// it, so two submissions racing on the API cannot mint the same id and make
    /// `runtime_watch` treat the second cutover as a no-op.
    pub(crate) async fn reserve(
        &self,
        request: ReloadRequest,
    ) -> Result<ReloadTicket, ReloadSubmitError> {
        let cancel = CancellationToken::new();
        let status = self
            .status_store
            .reserve(&self.active_generation, request.clone(), cancel.clone())
            .await?;
        Ok(ReloadTicket {
            control: self.clone(),
            reload_id: status.reload_id,
            target_generation: status.target_generation,
            request,
            cancel,
            settled: false,
        })
    }

    /// Atomically reserves and enqueues one reload operation.
    pub(crate) async fn submit(
        &self,
        config: Arc<ProxyConfig>,
        config_snapshot_hash: Option<u64>,
        config_revision: String,
        request: ReloadRequest,
    ) -> Result<ReloadAccepted, ReloadSubmitError> {
        self.reserve(request)
            .await?
            .dispatch(config, config_snapshot_hash, config_revision, None)
            .await
    }

    /// Cancels the in-flight reload, collapsing drain to an immediate stop.
    ///
    /// Returns `false` when `reload_id` is not the active operation.
    pub(crate) async fn cancel(&self, reload_id: u64) -> bool {
        self.status_store.cancel(reload_id).await
    }

    /// Returns a retained reload status by identifier.
    pub(crate) async fn status(&self, reload_id: u64) -> Option<ReloadStatus> {
        self.status_store.get(reload_id).await
    }

    /// Returns the identifier of the currently active reload.
    pub(crate) async fn in_progress(&self) -> Option<u64> {
        self.status_store.active_reload_id().await
    }

    /// Rejects new commands while preserving an already accepted operation.
    pub(crate) async fn begin_shutdown(&self) {
        self.status_store.stop_accepting().await;
    }

    /// Records a non-terminal lifecycle phase.
    pub(crate) async fn mark_phase(&self, reload_id: u64, phase: ReloadPhase) {
        self.status_store.mark_phase(reload_id, phase).await;
    }

    /// Records process-owned fields deferred until the next process restart.
    pub(crate) async fn set_deferred_fields(&self, reload_id: u64, fields: Vec<String>) {
        self.status_store
            .update(reload_id, |status| status.deferred_fields = fields)
            .await;
    }

    /// Commits the active generation and completes the matching reload.
    pub(crate) async fn succeed(&self, reload_id: u64, generation: u64) {
        self.status_store
            .finish_success(reload_id, generation, &self.active_generation)
            .await;
    }

    /// Marks the matching reload as failed.
    pub(crate) async fn fail(&self, reload_id: u64, error: ReloadError) {
        self.status_store
            .finish(reload_id, ReloadPhase::Failed, Some(error))
            .await;
    }

    /// Marks the matching reload as rolled back.
    pub(crate) async fn rolled_back(&self, reload_id: u64, error: ReloadError) {
        self.status_store
            .finish(reload_id, ReloadPhase::RolledBack, Some(error))
            .await;
    }

    /// Appends a non-fatal warning to the matching reload status.
    pub(crate) async fn add_warning(&self, reload_id: u64, warning: impl Into<String>) {
        let warning = warning.into();
        self.status_store
            .update(reload_id, |status| status.warnings.push(warning))
            .await;
    }
}

impl ReloadCommandReceiver {
    /// Receives the next accepted reload command.
    pub(crate) async fn recv(&mut self) -> Option<ReloadCommand> {
        self.command_rx.recv().await
    }
}

#[cfg(test)]
#[path = "reload_tests.rs"]
mod tests;
