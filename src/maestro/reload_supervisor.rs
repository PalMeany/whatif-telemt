use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::generation::{RuntimeGeneration, RuntimeWatchState};
use super::process_scope::ProcessScope;
use super::reload::{
    ConfigRollback, ReloadCommand, ReloadCommandReceiver, ReloadControl, ReloadError,
    ReloadFailurePolicy, ReloadMode, ReloadPhase,
};
use super::runtime_build::{PreparedRuntime, deferred_process_fields, prepare_runtime};
use super::runtime_tasks::RuntimeLogFilter;

/// Hard ceiling on candidate preparation.
///
/// `prepare_runtime` runs a network probe, a TLS front bootstrap and Middle-End
/// initialisation, each of which can stall on an unreachable peer, and ME retry
/// loops are unbounded by default. Without a deadline a wedged preparation keeps
/// the reload slot reserved for the process lifetime (every later submission
/// answers 409) and parks `quiesce()` forever, which turns SIGTERM into
/// SIGKILL and skips both the SYN-limiter cleanup and the quota-state save.
const PREPARE_TIMEOUT: Duration = Duration::from_secs(180);

/// Budget for the Middle-End `RPC_CLOSE_CONN` broadcast during teardown.
const ME_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long shutdown waits for a reserved-but-undelivered reload command.
///
/// The API reserves the reload slot before it touches the config file, so
/// `in_progress()` can be `Some` while the command is still in flight — or while
/// a request that reserved a slot is about to abandon it. Bounded so the loop
/// cannot park on a command that will never arrive.
const SHUTDOWN_COMMAND_GRACE: Duration = Duration::from_secs(1);

/// Budget for joining the supervisor task during shutdown.
///
/// Bounded so `perform_shutdown` always reaches `clear_synlimit_rules_all_backends`
/// and `save_quota_state`, even if a reload is mid-drain.
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(20);

/// Outcome of retiring one generation's background tasks and Middle-End pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MiddleEndTeardown {
    /// The generation had no Middle-End pool to close.
    NoPool,
    /// Close broadcast finished and every writer task was cancelled.
    Completed,
    /// The close broadcast exceeded its budget; writers were cancelled anyway.
    CloseTimedOut,
}

pub(crate) struct ReloadSupervisor {
    active_runtime: Arc<ArcSwap<RuntimeGeneration>>,
    control: ReloadControl,
    commands: ReloadCommandReceiver,
    config_path: PathBuf,
    process: Arc<ProcessScope>,
    detected_ips_tx: watch::Sender<(Option<std::net::IpAddr>, Option<std::net::IpAddr>)>,
    runtime_log_filter: RuntimeLogFilter,
    runtime_watch_tx: watch::Sender<Option<RuntimeWatchState>>,
    /// Whether `fork.runtime.reload_deadlines` bounds the reload state machine.
    ///
    /// Read once at start-up: the deadlines exist to keep shutdown reachable,
    /// so a reload must not be able to remove its own ceiling mid-flight.
    deadlines: bool,
    /// Test-only override that forces a Middle-End teardown outcome.
    ///
    /// Fixtures cannot build a real `MePool`, so without this the teardown
    /// branch (and its warning) is unreachable in every test.
    #[cfg(test)]
    forced_middle_end_teardown: Option<MiddleEndTeardown>,
}

/// Process-owned handle that quiesces reloads before shutdown snapshots the runtime.
pub(crate) struct ReloadSupervisorHandle {
    control: ReloadControl,
    shutdown: CancellationToken,
    join: tokio::task::JoinHandle<()>,
    /// Whether the quiesce budget applies.
    deadlines: bool,
}

impl ReloadSupervisorHandle {
    /// Stops new submissions and waits for the accepted reload to finish.
    ///
    /// Bounded and abortable: a reload draining on an operator-supplied hour-long
    /// timeout must not be able to hold the shutdown path open, because
    /// everything after this call — SYN-limiter rule cleanup and the only
    /// `save_quota_state` call in the tree — is skipped if systemd escalates to
    /// SIGKILL first.
    pub(crate) async fn quiesce(self) {
        self.control.begin_shutdown().await;
        self.shutdown.cancel();
        let mut join = self.join;
        if !self.deadlines {
            if let Err(error) = (&mut join).await {
                warn!(error = %error, "Reload supervisor failed while quiescing");
            }
            return;
        }
        match tokio::time::timeout(QUIESCE_TIMEOUT, &mut join).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!(error = %error, "Reload supervisor failed while quiescing");
            }
            Err(_) => {
                warn!(
                    timeout_secs = QUIESCE_TIMEOUT.as_secs(),
                    "Reload supervisor exceeded its quiesce budget; aborting it"
                );
                join.abort();
                let _ = join.await;
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RevisionGateAction {
    Proceed,
    Warn(String),
    Rollback(String),
}

fn revision_gate_action(
    accepted_revision: &str,
    current_revision: Result<String, ReloadError>,
    failure_policy: ReloadFailurePolicy,
) -> RevisionGateAction {
    let warning = match current_revision {
        Ok(current) if current == accepted_revision => return RevisionGateAction::Proceed,
        Ok(current) => format!(
            "config revision changed during preparation: accepted={} current={}",
            accepted_revision, current
        ),
        Err(error) => format!("config revision verification failed: {}", error),
    };
    match failure_policy {
        ReloadFailurePolicy::KeepNew => RevisionGateAction::Warn(warning),
        ReloadFailurePolicy::Rollback => RevisionGateAction::Rollback(warning),
    }
}

async fn stop_background_and_middle_end(generation: &RuntimeGeneration) -> MiddleEndTeardown {
    generation.stop_background_tasks().await;
    let Some(pool) = generation.current_me_pool().await else {
        return MiddleEndTeardown::NoPool;
    };
    let outcome =
        match tokio::time::timeout(ME_CLOSE_TIMEOUT, pool.shutdown_send_close_conn_all()).await {
            Ok(_) => MiddleEndTeardown::Completed,
            Err(_) => MiddleEndTeardown::CloseTimedOut,
        };
    // The close broadcast only signals *clients*. The writer lifecycle tasks are
    // detached spawns whose sole cancellation source is `MeWriter::cancel`, and
    // a retired generation never calls `remove_writer_with_mode`, so without
    // this the tasks and their TCP connections to the middle proxies survive
    // every reload and accumulate at `pool_size` sockets per generation.
    let cancelled = pool.shutdown().await;
    if cancelled > 0 {
        info!(
            cancelled_writers = cancelled,
            "Retired generation Middle-End writers cancelled"
        );
    }
    outcome
}

/// Restores the pre-patch config file for a reload that did not take effect.
///
/// Skipped when the file no longer matches what this reload wrote, so a
/// concurrent editor is never clobbered. Returns a warning to attach to the
/// reload status when the restore could not be completed.
async fn restore_rollback(rollback: &ConfigRollback) -> Option<String> {
    match crate::api::config_store::restore_config_if_unchanged(
        &rollback.path,
        &rollback.written_revision,
        &rollback.previous_content,
    )
    .await
    {
        Ok(true) => {
            info!(
                path = %rollback.path.display(),
                "Rolled back config file to its pre-patch contents"
            );
            None
        }
        Ok(false) => Some(format!(
            "config file {} changed after this reload wrote it; pre-patch contents were not restored",
            rollback.path.display()
        )),
        Err(error) => Some(format!(
            "failed to restore pre-patch config {}: {}",
            rollback.path.display(),
            error
        )),
    }
}

impl ReloadSupervisor {
    #[allow(clippy::too_many_arguments)]
    /// Starts the process-scoped reload supervisor and returns its shutdown owner.
    pub(crate) fn spawn(
        active_runtime: Arc<ArcSwap<RuntimeGeneration>>,
        control: ReloadControl,
        commands: ReloadCommandReceiver,
        config_path: PathBuf,
        process: Arc<ProcessScope>,
        detected_ips_tx: watch::Sender<(Option<std::net::IpAddr>, Option<std::net::IpAddr>)>,
        runtime_log_filter: RuntimeLogFilter,
        runtime_watch_tx: watch::Sender<Option<RuntimeWatchState>>,
        deadlines: bool,
    ) -> ReloadSupervisorHandle {
        let supervisor = Self {
            active_runtime,
            control,
            commands,
            config_path,
            process,
            detected_ips_tx,
            runtime_log_filter,
            runtime_watch_tx,
            deadlines,
            #[cfg(test)]
            forced_middle_end_teardown: None,
        };
        let control = supervisor.control.clone();
        let shutdown = CancellationToken::new();
        let join = tokio::spawn(supervisor.run(shutdown.clone()));
        ReloadSupervisorHandle {
            control,
            shutdown,
            join,
            deadlines,
        }
    }

    async fn run(mut self, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    // A slot may be reserved with its command still in flight;
                    // run it so the reload terminates instead of leaving the
                    // status stuck in a non-terminal phase. `reload` observes
                    // the same token, so this collapses immediately.
                    if self.control.in_progress().await.is_some() {
                        let grace = if self.deadlines {
                            SHUTDOWN_COMMAND_GRACE
                        } else {
                            Duration::from_secs(0)
                        };
                        if let Ok(Some(command)) =
                            tokio::time::timeout(grace, self.commands.recv()).await
                        {
                            self.reload(command, &shutdown).await;
                        }
                    }
                    break;
                }
                command = self.commands.recv() => {
                    let Some(command) = command else {
                        break;
                    };
                    self.reload(command, &shutdown).await;
                }
            }
        }
    }

    /// Terminates `command` as failed, restoring the config file first when the
    /// caller asked for rollback semantics.
    async fn fail(&self, command: &ReloadCommand, error: ReloadError) {
        if command.request.failure_policy == ReloadFailurePolicy::Rollback
            && let Some(rollback) = command.rollback.as_ref()
            && let Some(warning) = restore_rollback(rollback).await
        {
            warn!(reload_id = command.reload_id, warning = %warning);
            self.control.add_warning(command.reload_id, warning).await;
        }
        self.control.fail(command.reload_id, error).await;
    }

    /// Terminates `command` as rolled back, restoring the config file first.
    async fn roll_back(&self, command: &ReloadCommand, error: ReloadError) {
        if let Some(rollback) = command.rollback.as_ref()
            && let Some(warning) = restore_rollback(rollback).await
        {
            warn!(reload_id = command.reload_id, warning = %warning);
            self.control.add_warning(command.reload_id, warning).await;
        }
        self.control.rolled_back(command.reload_id, error).await;
    }

    /// Discards a prepared-but-unused candidate, reporting teardown problems.
    async fn discard_candidate(&self, reload_id: u64, candidate: &RuntimeGeneration) {
        candidate.stop_sessions().await;
        if self.middle_end_teardown(candidate).await == MiddleEndTeardown::CloseTimedOut {
            let warning = format!(
                "candidate generation {} Middle-End close broadcast timed out during cleanup",
                candidate.id
            );
            warn!(reload_id, warning = %warning);
            self.control.add_warning(reload_id, warning).await;
        }
    }

    #[cfg(test)]
    async fn middle_end_teardown(&self, generation: &RuntimeGeneration) -> MiddleEndTeardown {
        if let Some(forced) = self.forced_middle_end_teardown {
            generation.stop_background_tasks().await;
            return forced;
        }
        stop_background_and_middle_end(generation).await
    }

    #[cfg(not(test))]
    async fn middle_end_teardown(&self, generation: &RuntimeGeneration) -> MiddleEndTeardown {
        stop_background_and_middle_end(generation).await
    }

    async fn reload(&self, command: ReloadCommand, shutdown: &CancellationToken) {
        // One token for both cancellation sources: process shutdown (parent) and
        // an operator `DELETE /v1/system/reload/{id}` (bridged below). Every
        // unbounded wait in this state machine hangs off it.
        let cancel = shutdown.child_token();
        let bridge = {
            let cancel = cancel.clone();
            let requested = command.cancel.clone();
            tokio::spawn(async move {
                requested.cancelled().await;
                cancel.cancel();
            })
        };

        self.run_reload(&command, &cancel).await;
        bridge.abort();
    }

    async fn run_reload(&self, command: &ReloadCommand, cancel: &CancellationToken) {
        self.control
            .mark_phase(command.reload_id, ReloadPhase::Preparing)
            .await;
        let old_runtime = self.active_runtime.load_full();
        // Baseline is always "the config this process is running" so the field
        // means exactly one thing everywhere it is reported.
        let deferred = deferred_process_fields(&old_runtime.config(), &command.config);
        self.control
            .set_deferred_fields(command.reload_id, deferred)
            .await;

        let prepare_cancel = cancel.child_token();
        let deadline_hit = Arc::new(AtomicBool::new(false));
        let deadline = self.deadlines.then(|| {
            let prepare_cancel = prepare_cancel.clone();
            let deadline_hit = deadline_hit.clone();
            tokio::spawn(async move {
                tokio::time::sleep(PREPARE_TIMEOUT).await;
                deadline_hit.store(true, Ordering::Release);
                prepare_cancel.cancel();
            })
        });
        let prepared = prepare_runtime(
            command.target_generation,
            command.config.as_ref().clone(),
            command.config_snapshot_hash,
            &self.config_path,
            self.process.clone(),
            self.runtime_log_filter.clone(),
            prepare_cancel,
        )
        .await;
        if let Some(deadline) = deadline {
            deadline.abort();
        }

        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(ReloadError::Cancelled) if deadline_hit.load(Ordering::Acquire) => {
                self.fail(
                    command,
                    ReloadError::Timeout(format!(
                        "runtime preparation exceeded {}s",
                        PREPARE_TIMEOUT.as_secs()
                    )),
                )
                .await;
                return;
            }
            Err(error) => {
                self.fail(command, error).await;
                return;
            }
        };

        if cancel.is_cancelled() {
            self.discard_candidate(command.reload_id, &prepared.generation)
                .await;
            self.fail(command, ReloadError::Cancelled).await;
            return;
        }

        let revision_action = revision_gate_action(
            &command.config_revision,
            crate::api::config_store::current_revision_for_maestro(&self.config_path)
                .await
                .map_err(ReloadError::Internal),
            command.request.failure_policy,
        );
        self.activate_prepared(command, old_runtime, prepared, revision_action, cancel)
            .await;
    }

    async fn activate_prepared(
        &self,
        command: &ReloadCommand,
        old_runtime: Arc<RuntimeGeneration>,
        prepared: PreparedRuntime,
        revision_action: RevisionGateAction,
        cancel: &CancellationToken,
    ) {
        match revision_action {
            RevisionGateAction::Proceed => {}
            RevisionGateAction::Warn(warning) => {
                self.control.add_warning(command.reload_id, warning).await;
            }
            RevisionGateAction::Rollback(warning) => {
                self.discard_candidate(command.reload_id, &prepared.generation)
                    .await;
                self.runtime_log_filter
                    .apply_reload(&old_runtime.config().general.log_level);
                self.roll_back(command, ReloadError::RevisionChanged(warning))
                    .await;
                return;
            }
        }

        self.control
            .mark_phase(command.reload_id, ReloadPhase::Activating)
            .await;
        let new_runtime = prepared.generation;
        // Admission stays open until the swap itself. There is nothing left to
        // fail between here and `swap`: DNS overrides are generation-owned and
        // were validated and installed during preparation, so closing early
        // would only create a window where accepted connections are dropped and
        // then have to be reopened on rollback.
        old_runtime.stop_accepting_sessions();
        let replaced = self.active_runtime.swap(new_runtime.clone());
        self.detected_ips_tx.send_replace(prepared.detected_ips);
        // Retune the process-scoped budgets in place; a second envelope per
        // generation is exactly what let a drain reload run at 2x the cap.
        self.process.publish_generation(
            &new_runtime.config(),
            new_runtime.stats.clone(),
            new_runtime.proxy_shared.clone(),
        );
        self.runtime_log_filter
            .apply_reload(&new_runtime.config().general.log_level);
        self.runtime_watch_tx
            .send_replace(Some(new_runtime.watch_state()));

        info!(
            reload_id = command.reload_id,
            old_generation = replaced.id,
            new_generation = new_runtime.id,
            config_revision = %command.config_revision,
            "Runtime generation activated"
        );

        match command.request.mode {
            ReloadMode::Instant => {
                replaced.stop_sessions().await;
            }
            ReloadMode::Drain => {
                self.control
                    .mark_phase(command.reload_id, ReloadPhase::Draining)
                    .await;
                let timeout = Duration::from_secs(
                    command
                        .request
                        .timeout_secs
                        .expect("validated drain request must carry timeout_secs"),
                );
                let warning = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        replaced.stop_sessions().await;
                        Some(format!(
                            "generation {} drain was cut short by shutdown or cancellation; \
                             remaining sessions were cancelled",
                            replaced.id
                        ))
                    }
                    drained = replaced.drain_sessions(timeout) => (!drained).then(|| format!(
                        "generation {} exceeded drain timeout; remaining sessions were cancelled",
                        replaced.id
                    )),
                };
                if let Some(warning) = warning {
                    warn!(reload_id = command.reload_id, warning = %warning);
                    self.control.add_warning(command.reload_id, warning).await;
                }
            }
        }

        if self.middle_end_teardown(&replaced).await == MiddleEndTeardown::CloseTimedOut {
            let warning = format!(
                "generation {} Middle-End close broadcast timed out",
                replaced.id
            );
            warn!(reload_id = command.reload_id, warning = %warning);
            self.control.add_warning(command.reload_id, warning).await;
        }
        self.control
            .succeed(command.reload_id, new_runtime.id)
            .await;
    }
}

#[cfg(test)]
#[path = "reload_supervisor_tests.rs"]
mod tests;
