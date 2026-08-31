//! Retained status history for in-process runtime reloads.
//!
//! Owns the single-slot reservation invariant: at most one non-terminal reload
//! exists at a time, and the generation id it will activate is allocated under
//! the same lock that publishes it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::reload::{ReloadFailurePolicy, ReloadMode, ReloadRequest, ReloadSubmitError};
use super::reload_error::ReloadError;

pub(super) const RELOAD_HISTORY_CAPACITY: usize = 32;

/// Observable phase of one reload operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReloadPhase {
    Accepted,
    Preparing,
    Activating,
    Draining,
    Succeeded,
    RolledBack,
    Failed,
}

impl ReloadPhase {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            ReloadPhase::Succeeded | ReloadPhase::RolledBack | ReloadPhase::Failed
        )
    }
}

/// Bounded public status for one reload operation.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ReloadStatus {
    pub(crate) reload_id: u64,
    pub(crate) target_generation: u64,
    pub(crate) config_revision: String,
    pub(crate) state: ReloadPhase,
    pub(crate) mode: ReloadMode,
    pub(crate) failure_policy: ReloadFailurePolicy,
    pub(crate) requested_at_epoch_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) started_at_epoch_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) finished_at_epoch_secs: Option<u64>,
    #[serde(
        rename = "deferred_process_fields",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub(crate) deferred_fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    /// Stable slug for `error`, so a client can branch on the failure class
    /// instead of pattern-matching a human-readable message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error_kind: Option<&'static str>,
}

/// Accepted operation metadata returned before asynchronous preparation starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReloadAccepted {
    pub(crate) reload_id: u64,
    pub(crate) target_generation: u64,
    pub(crate) config_revision: String,
    pub(crate) state: ReloadPhase,
    pub(crate) mode: ReloadMode,
    pub(crate) failure_policy: ReloadFailurePolicy,
}

struct ReloadStatusState {
    next_reload_id: u64,
    active_reload_id: Option<u64>,
    /// Cancellation handle for `active_reload_id`, exposed by `DELETE /v1/system/reload/{id}`.
    active_cancel: Option<CancellationToken>,
    statuses: VecDeque<ReloadStatus>,
    accepting_commands: bool,
}

impl Default for ReloadStatusState {
    fn default() -> Self {
        Self {
            next_reload_id: 0,
            active_reload_id: None,
            active_cancel: None,
            statuses: VecDeque::new(),
            accepting_commands: true,
        }
    }
}

/// Bounded, mutex-guarded history of reload operations.
///
/// The mutex is the single-slot invariant: reservation, generation-id
/// allocation, cancellation handle and terminal transitions all happen under it,
/// so two submissions can never observe the same free slot or the same
/// `active_generation`.
#[derive(Default)]
pub(super) struct ReloadStatusStore {
    state: Mutex<ReloadStatusState>,
}

impl ReloadStatusStore {
    /// Fires the cancellation handle of `reload_id` when it is the active reload.
    pub(super) async fn cancel(&self, reload_id: u64) -> bool {
        let state = self.state.lock().await;
        if state.active_reload_id != Some(reload_id) {
            return false;
        }
        match state.active_cancel.as_ref() {
            Some(cancel) => {
                cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Identifier of the reload currently holding the slot, if any.
    pub(super) async fn active_reload_id(&self) -> Option<u64> {
        self.state.lock().await.active_reload_id
    }

    /// Stops accepting reservations without disturbing the active operation.
    pub(super) async fn stop_accepting(&self) {
        self.state.lock().await.accepting_commands = false;
    }

    pub(super) async fn reserve(
        &self,
        active_generation: &AtomicU64,
        request: ReloadRequest,
        cancel: CancellationToken,
    ) -> Result<ReloadStatus, ReloadSubmitError> {
        let mut state = self.state.lock().await;
        if !state.accepting_commands {
            return Err(ReloadSubmitError::MaestroUnavailable);
        }
        if let Some(reload_id) = state.active_reload_id {
            return Err(ReloadSubmitError::InProgress(reload_id));
        }
        state.next_reload_id = state.next_reload_id.saturating_add(1).max(1);
        let reload_id = state.next_reload_id;
        // Allocated under the same lock that publishes it: reading
        // `active_generation` outside would let two submissions observe the same
        // value and mint duplicate generation ids, which `runtime_watch` reads
        // as "nothing changed" and skips the cutover event for.
        let target_generation = active_generation.load(Ordering::Acquire).saturating_add(1);
        let status = ReloadStatus {
            reload_id,
            target_generation,
            config_revision: String::new(),
            state: ReloadPhase::Accepted,
            mode: request.mode,
            failure_policy: request.failure_policy,
            requested_at_epoch_secs: now_epoch_secs(),
            started_at_epoch_secs: None,
            finished_at_epoch_secs: None,
            deferred_fields: Vec::new(),
            warnings: Vec::new(),
            error: None,
            error_kind: None,
        };
        state.active_reload_id = Some(reload_id);
        state.active_cancel = Some(cancel);
        state.statuses.push_back(status.clone());
        while state.statuses.len() > RELOAD_HISTORY_CAPACITY {
            state.statuses.pop_front();
        }
        Ok(status)
    }

    pub(super) async fn get(&self, reload_id: u64) -> Option<ReloadStatus> {
        self.state
            .lock()
            .await
            .statuses
            .iter()
            .find(|status| status.reload_id == reload_id)
            .cloned()
    }

    pub(super) async fn mark_phase(&self, reload_id: u64, phase: ReloadPhase) {
        self.update(reload_id, |status| {
            status.state = phase;
            if status.started_at_epoch_secs.is_none() && phase != ReloadPhase::Accepted {
                status.started_at_epoch_secs = Some(now_epoch_secs());
            }
        })
        .await;
    }

    pub(super) async fn finish(
        &self,
        reload_id: u64,
        phase: ReloadPhase,
        error: Option<ReloadError>,
    ) {
        debug_assert!(phase.is_terminal());
        let mut state = self.state.lock().await;
        if let Some(status) = state
            .statuses
            .iter_mut()
            .find(|status| status.reload_id == reload_id)
        {
            status.state = phase;
            status.error_kind = crate::fork::switches::reload_error_kind()
                .then(|| error.as_ref().map(ReloadError::kind))
                .flatten();
            status.error = error.map(|error| error.to_string());
            status.finished_at_epoch_secs = Some(now_epoch_secs());
        }
        if state.active_reload_id == Some(reload_id) {
            state.active_reload_id = None;
            state.active_cancel = None;
        }
    }

    pub(super) async fn finish_success(
        &self,
        reload_id: u64,
        generation: u64,
        active_generation: &AtomicU64,
    ) {
        let mut state = self.state.lock().await;
        if state.active_reload_id != Some(reload_id) {
            return;
        }
        let Some(status) = state
            .statuses
            .iter_mut()
            .find(|status| status.reload_id == reload_id)
        else {
            return;
        };
        status.state = ReloadPhase::Succeeded;
        status.error = None;
        status.error_kind = None;
        status.finished_at_epoch_secs = Some(now_epoch_secs());
        active_generation.store(generation, Ordering::Release);
        state.active_reload_id = None;
        state.active_cancel = None;
    }

    pub(super) async fn update(&self, reload_id: u64, update: impl FnOnce(&mut ReloadStatus)) {
        let mut state = self.state.lock().await;
        if let Some(status) = state
            .statuses
            .iter_mut()
            .find(|status| status.reload_id == reload_id)
        {
            update(status);
        }
    }
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
