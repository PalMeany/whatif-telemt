use super::*;
use crate::config::ProxyConfig;
use crate::maestro::generation::test_runtime_generation;
use crate::maestro::reload::{ReloadRequest, ReloadSubmitError};
use tokio::sync::Notify;
use tracing_subscriber::{EnvFilter, Registry};

struct ReloadFixture {
    supervisor: Arc<ReloadSupervisor>,
    control: ReloadControl,
    command: ReloadCommand,
    old_runtime: Arc<RuntimeGeneration>,
    new_runtime: Arc<RuntimeGeneration>,
    runtime_watch_rx: watch::Receiver<Option<RuntimeWatchState>>,
}

fn runtime_log_filter() -> RuntimeLogFilter {
    let (_layer, handle) =
        tracing_subscriber::reload::Layer::<EnvFilter, Registry>::new(EnvFilter::new("info"));
    RuntimeLogFilter::new(handle, true)
}

/// Config that fails `ProxyConfig::validate()` deterministically, without any
/// network I/O, so `reload()` can be driven end to end against a real failure.
///
/// Zero users is the exact invariant that used to slip through
/// `POST /v1/system/reload`: fatal at startup, rejected by SIGHUP, but installed
/// live by a bare reload POST.
fn invalid_config() -> Arc<ProxyConfig> {
    let mut config = ProxyConfig::default();
    config.access.users.clear();
    Arc::new(config)
}

async fn fixture(request: ReloadRequest) -> ReloadFixture {
    fixture_with_teardown(request, None).await
}

async fn fixture_with_teardown(
    request: ReloadRequest,
    forced_middle_end_teardown: Option<MiddleEndTeardown>,
) -> ReloadFixture {
    let old_runtime = test_runtime_generation(1, ProxyConfig::default());
    let new_config = Arc::new(ProxyConfig::default());
    let new_runtime = test_runtime_generation(2, new_config.as_ref().clone());
    let active_runtime = Arc::new(ArcSwap::from(old_runtime.clone()));
    let (control, commands) = ReloadControl::channel(old_runtime.id);
    let accepted = control
        .submit(
            new_config.clone(),
            None,
            "revision".to_string(),
            request.clone(),
        )
        .await
        .unwrap();
    let (detected_ips_tx, _detected_ips_rx) = watch::channel((None, None));
    let (runtime_watch_tx, runtime_watch_rx) = watch::channel(Some(old_runtime.watch_state()));
    let supervisor = Arc::new(ReloadSupervisor {
        active_runtime,
        control: control.clone(),
        commands,
        config_path: PathBuf::new(),
        process: ProcessScope::new(&ProxyConfig::default()).await,
        detected_ips_tx,
        runtime_log_filter: runtime_log_filter(),
        runtime_watch_tx,
        deadlines: true,
        forced_middle_end_teardown,
    });
    let command = ReloadCommand {
        reload_id: accepted.reload_id,
        target_generation: accepted.target_generation,
        config: new_config,
        config_snapshot_hash: None,
        config_revision: accepted.config_revision,
        request,
        rollback: None,
        cancel: CancellationToken::new(),
    };
    ReloadFixture {
        supervisor,
        control,
        command,
        old_runtime,
        new_runtime,
        runtime_watch_rx,
    }
}

struct DropSignal(Arc<Notify>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[test]
fn revision_gate_proceeds_only_on_verified_match() {
    assert_eq!(
        revision_gate_action(
            "accepted",
            Ok("accepted".to_string()),
            ReloadFailurePolicy::Rollback,
        ),
        RevisionGateAction::Proceed
    );
}

#[test]
fn revision_gate_applies_failure_policy_to_mismatch_and_read_error() {
    let results: [Result<String, ReloadError>; 2] = [
        Ok("changed".to_string()),
        Err(ReloadError::Internal("read failed".to_string())),
    ];
    for result in results {
        assert!(matches!(
            revision_gate_action("accepted", result.clone(), ReloadFailurePolicy::KeepNew),
            RevisionGateAction::Warn(_)
        ));
        assert!(matches!(
            revision_gate_action("accepted", result, ReloadFailurePolicy::Rollback),
            RevisionGateAction::Rollback(_)
        ));
    }
}

#[tokio::test]
async fn revision_rollback_keeps_old_generation_and_cleans_candidate() {
    let fixture = fixture(ReloadRequest {
        failure_policy: ReloadFailurePolicy::Rollback,
        ..ReloadRequest::default()
    })
    .await;
    let candidate_dropped = Arc::new(Notify::new());
    let candidate_drop = candidate_dropped.clone();
    assert!(fixture.new_runtime.spawn_session(async move {
        let _drop_signal = DropSignal(candidate_drop);
        std::future::pending::<()>().await;
    }));
    tokio::task::yield_now().await;

    fixture
        .supervisor
        .activate_prepared(
            &fixture.command,
            fixture.old_runtime.clone(),
            PreparedRuntime {
                generation: fixture.new_runtime.clone(),
                detected_ips: (None, None),
            },
            RevisionGateAction::Rollback("revision changed".to_string()),
            &CancellationToken::new(),
        )
        .await;

    tokio::time::timeout(Duration::from_secs(1), candidate_dropped.notified())
        .await
        .unwrap();
    assert_eq!(fixture.supervisor.active_runtime.load().id, 1);
    assert_eq!(
        fixture
            .runtime_watch_rx
            .borrow()
            .as_ref()
            .unwrap()
            .generation_id,
        1
    );
    // The old generation never stopped admitting, so no traffic was dropped in
    // the window that used to exist between closing admission and rolling back.
    assert!(fixture.old_runtime.spawn_session(async {}));
    let status = fixture.control.status(1).await.unwrap();
    assert_eq!(status.state, ReloadPhase::RolledBack);
    assert_eq!(status.error_kind, Some("revision_changed"));
    fixture.old_runtime.stop_sessions().await;
}

#[tokio::test]
async fn rollback_restores_the_pre_patch_config_file() {
    // `PATCH /v1/config?reload=…&failure_policy=rollback` commits the merged
    // config before the reload runs; a rolled-back reload has to put it back or
    // the proxy keeps enforcing (and restarts into) the rejected config.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let previous = "[censorship]\ntls_domain = \"before.example\"\n";
    let written = "[censorship]\ntls_domain = \"after.example\"\n";
    tokio::fs::write(&path, written).await.unwrap();

    let mut fixture = fixture(ReloadRequest {
        failure_policy: ReloadFailurePolicy::Rollback,
        ..ReloadRequest::default()
    })
    .await;
    fixture.command.rollback = Some(ConfigRollback {
        path: path.clone(),
        previous_content: previous.to_string(),
        written_revision: crate::api::config_store::compute_revision(written),
    });

    fixture
        .supervisor
        .activate_prepared(
            &fixture.command,
            fixture.old_runtime.clone(),
            PreparedRuntime {
                generation: fixture.new_runtime.clone(),
                detected_ips: (None, None),
            },
            RevisionGateAction::Rollback("revision changed".to_string()),
            &CancellationToken::new(),
        )
        .await;

    assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), previous);
    let status = fixture.control.status(1).await.unwrap();
    assert_eq!(status.state, ReloadPhase::RolledBack);
    assert!(status.warnings.is_empty(), "{:?}", status.warnings);
    fixture.old_runtime.stop_sessions().await;
}

#[tokio::test]
async fn rollback_does_not_clobber_a_concurrent_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let written = "[censorship]\ntls_domain = \"after.example\"\n";
    let concurrent = "[censorship]\ntls_domain = \"someone-else.example\"\n";
    tokio::fs::write(&path, concurrent).await.unwrap();

    let mut fixture = fixture(ReloadRequest {
        failure_policy: ReloadFailurePolicy::Rollback,
        ..ReloadRequest::default()
    })
    .await;
    fixture.command.rollback = Some(ConfigRollback {
        path: path.clone(),
        previous_content: "[censorship]\ntls_domain = \"before.example\"\n".to_string(),
        written_revision: crate::api::config_store::compute_revision(written),
    });

    fixture
        .supervisor
        .activate_prepared(
            &fixture.command,
            fixture.old_runtime.clone(),
            PreparedRuntime {
                generation: fixture.new_runtime.clone(),
                detected_ips: (None, None),
            },
            RevisionGateAction::Rollback("revision changed".to_string()),
            &CancellationToken::new(),
        )
        .await;

    assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), concurrent);
    let status = fixture.control.status(1).await.unwrap();
    assert_eq!(status.warnings.len(), 1, "{:?}", status.warnings);
    assert!(status.warnings[0].contains("were not restored"));
    fixture.old_runtime.stop_sessions().await;
}

#[tokio::test]
async fn drain_publishes_new_generation_before_old_sessions_finish() {
    let mut fixture = fixture(ReloadRequest {
        mode: ReloadMode::Drain,
        timeout_secs: Some(30),
        ..ReloadRequest::default()
    })
    .await;
    let old_started = Arc::new(Notify::new());
    let old_release = Arc::new(Notify::new());
    let started = old_started.clone();
    let release = old_release.clone();
    assert!(fixture.old_runtime.spawn_session(async move {
        started.notify_one();
        release.notified().await;
    }));
    old_started.notified().await;

    let supervisor = fixture.supervisor.clone();
    let old_runtime = fixture.old_runtime.clone();
    let new_runtime = fixture.new_runtime.clone();
    let command = fixture.command;
    let activation = tokio::spawn(async move {
        supervisor
            .activate_prepared(
                &command,
                old_runtime,
                PreparedRuntime {
                    generation: new_runtime,
                    detected_ips: (None, None),
                },
                RevisionGateAction::Proceed,
                &CancellationToken::new(),
            )
            .await;
    });

    fixture.runtime_watch_rx.changed().await.unwrap();
    assert_eq!(
        fixture
            .runtime_watch_rx
            .borrow()
            .as_ref()
            .unwrap()
            .generation_id,
        2
    );
    assert!(!activation.is_finished());
    assert!(!fixture.old_runtime.spawn_session(async {}));

    old_release.notify_one();
    activation.await.unwrap();
    assert_eq!(
        fixture.control.status(1).await.unwrap().state,
        ReloadPhase::Succeeded
    );
    fixture.new_runtime.stop_sessions().await;
}

#[tokio::test(start_paused = true)]
async fn drain_timeout_and_me_close_timeout_each_record_a_warning() {
    // With `me_pool: None` in every fixture the Middle-End branch used to be a
    // guaranteed no-op, so the second warning could never be observed.
    let mut fixture = fixture_with_teardown(
        ReloadRequest {
            mode: ReloadMode::Drain,
            timeout_secs: Some(1),
            ..ReloadRequest::default()
        },
        Some(MiddleEndTeardown::CloseTimedOut),
    )
    .await;
    let dropped = Arc::new(Notify::new());
    let drop_signal = dropped.clone();
    assert!(fixture.old_runtime.spawn_session(async move {
        let _drop_signal = DropSignal(drop_signal);
        std::future::pending::<()>().await;
    }));
    tokio::task::yield_now().await;

    let supervisor = fixture.supervisor.clone();
    let old_runtime = fixture.old_runtime.clone();
    let new_runtime = fixture.new_runtime.clone();
    let command = fixture.command;
    let activation = tokio::spawn(async move {
        supervisor
            .activate_prepared(
                &command,
                old_runtime,
                PreparedRuntime {
                    generation: new_runtime,
                    detected_ips: (None, None),
                },
                RevisionGateAction::Proceed,
                &CancellationToken::new(),
            )
            .await;
    });
    fixture.runtime_watch_rx.changed().await.unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    activation.await.unwrap();

    dropped.notified().await;
    let status = fixture.control.status(1).await.unwrap();
    assert_eq!(status.state, ReloadPhase::Succeeded);
    assert_eq!(status.warnings.len(), 2, "{:?}", status.warnings);
    assert!(status.warnings[0].contains("exceeded drain timeout"));
    assert!(status.warnings[1].contains("Middle-End close broadcast timed out"));
    fixture.new_runtime.stop_sessions().await;
}

#[tokio::test]
async fn cancelled_drain_collapses_to_an_immediate_stop() {
    // Shutdown must not have to wait out an operator-supplied drain window:
    // everything after `quiesce()` (SYN-limiter cleanup, quota persistence) is
    // skipped if systemd escalates to SIGKILL first.
    let mut fixture = fixture(ReloadRequest {
        mode: ReloadMode::Drain,
        timeout_secs: Some(3_600),
        ..ReloadRequest::default()
    })
    .await;
    let dropped = Arc::new(Notify::new());
    let drop_signal = dropped.clone();
    assert!(fixture.old_runtime.spawn_session(async move {
        let _drop_signal = DropSignal(drop_signal);
        std::future::pending::<()>().await;
    }));
    tokio::task::yield_now().await;

    let cancel = CancellationToken::new();
    let supervisor = fixture.supervisor.clone();
    let old_runtime = fixture.old_runtime.clone();
    let new_runtime = fixture.new_runtime.clone();
    let command = fixture.command;
    let activation_cancel = cancel.clone();
    let activation = tokio::spawn(async move {
        supervisor
            .activate_prepared(
                &command,
                old_runtime,
                PreparedRuntime {
                    generation: new_runtime,
                    detected_ips: (None, None),
                },
                RevisionGateAction::Proceed,
                &activation_cancel,
            )
            .await;
    });
    fixture.runtime_watch_rx.changed().await.unwrap();
    cancel.cancel();

    tokio::time::timeout(Duration::from_secs(5), activation)
        .await
        .expect("a cancelled drain must not wait out its timeout")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), dropped.notified())
        .await
        .unwrap();
    let status = fixture.control.status(1).await.unwrap();
    assert_eq!(status.state, ReloadPhase::Succeeded);
    assert_eq!(status.warnings.len(), 1, "{:?}", status.warnings);
    assert!(status.warnings[0].contains("cut short"));
    fixture.new_runtime.stop_sessions().await;
}

#[tokio::test]
async fn quiesce_joins_idle_supervisor_and_rejects_later_submissions() {
    let runtime = test_runtime_generation(1, ProxyConfig::default());
    let active_runtime = Arc::new(ArcSwap::from(runtime.clone()));
    let (control, commands) = ReloadControl::channel(runtime.id);
    let (detected_ips_tx, _detected_ips_rx) = watch::channel((None, None));
    let (runtime_watch_tx, _runtime_watch_rx) = watch::channel(Some(runtime.watch_state()));
    let handle = ReloadSupervisor::spawn(
        active_runtime,
        control.clone(),
        commands,
        PathBuf::new(),
        ProcessScope::new(&ProxyConfig::default()).await,
        detected_ips_tx,
        runtime_log_filter(),
        runtime_watch_tx,
        true,
    );

    tokio::time::timeout(Duration::from_secs(1), handle.quiesce())
        .await
        .unwrap();
    let result = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "revision".to_string(),
            ReloadRequest::default(),
        )
        .await;

    assert_eq!(result, Err(ReloadSubmitError::MaestroUnavailable));
    runtime.stop_sessions().await;
}

#[tokio::test]
async fn quiesce_returns_within_its_budget_with_a_reload_in_flight() {
    // The previous test submitted *after* quiescing, so quiesce-while-in-flight
    // — the case that actually parks shutdown — was never exercised.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    // Unreadable path for the revision gate is irrelevant here: the reload is
    // expected to fail long before that, in preparation.
    tokio::fs::write(&config_path, "[censorship]\n")
        .await
        .unwrap();

    let runtime = test_runtime_generation(1, ProxyConfig::default());
    let active_runtime = Arc::new(ArcSwap::from(runtime.clone()));
    let (control, commands) = ReloadControl::channel(runtime.id);
    let (detected_ips_tx, _detected_ips_rx) = watch::channel((None, None));
    let (runtime_watch_tx, _runtime_watch_rx) = watch::channel(Some(runtime.watch_state()));
    let handle = ReloadSupervisor::spawn(
        active_runtime,
        control.clone(),
        commands,
        config_path,
        ProcessScope::new(&ProxyConfig::default()).await,
        detected_ips_tx,
        runtime_log_filter(),
        runtime_watch_tx,
        true,
    );

    // An invalid candidate keeps preparation deterministic and offline while
    // still guaranteeing the supervisor is mid-command when quiesce lands.
    control
        .submit(
            invalid_config(),
            None,
            "revision".to_string(),
            ReloadRequest::default(),
        )
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(30), handle.quiesce())
        .await
        .expect("quiesce must return within a bounded budget");
    runtime.stop_sessions().await;
}

#[tokio::test]
async fn real_reload_releases_the_slot_when_preparation_fails() {
    // End-to-end through `reload()` — not `activate_prepared` with a hand-built
    // candidate. Deleting the `control.fail(...)` in the supervisor makes this
    // test fail instead of leaving production wedged at 409 forever.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    tokio::fs::write(&config_path, "[censorship]\ntls_domain = \"example.com\"\n")
        .await
        .unwrap();

    let runtime = test_runtime_generation(1, ProxyConfig::default());
    let active_runtime = Arc::new(ArcSwap::from(runtime.clone()));
    let (control, commands) = ReloadControl::channel(runtime.id);
    let (detected_ips_tx, _detected_ips_rx) = watch::channel((None, None));
    let (runtime_watch_tx, _runtime_watch_rx) = watch::channel(Some(runtime.watch_state()));
    let supervisor = ReloadSupervisor {
        active_runtime: active_runtime.clone(),
        control: control.clone(),
        commands,
        config_path,
        process: ProcessScope::new(&ProxyConfig::default()).await,
        detected_ips_tx,
        runtime_log_filter: runtime_log_filter(),
        runtime_watch_tx,
        deadlines: true,
        forced_middle_end_teardown: None,
    };

    let accepted = control
        .submit(
            invalid_config(),
            None,
            "revision".to_string(),
            ReloadRequest::default(),
        )
        .await
        .unwrap();

    let shutdown = CancellationToken::new();
    let mut supervisor = supervisor;
    let command = supervisor.commands.recv().await.unwrap();
    supervisor.reload(command, &shutdown).await;

    let status = control.status(accepted.reload_id).await.unwrap();
    assert_eq!(status.state, ReloadPhase::Failed);
    assert_eq!(status.error_kind, Some("config_invalid"));
    assert_eq!(
        active_runtime.load().id,
        1,
        "a failed preparation must not swap the active runtime"
    );

    // The slot is free: a follow-up submission is accepted.
    assert_eq!(control.in_progress().await, None);
    let follow_up = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "revision".to_string(),
            ReloadRequest::default(),
        )
        .await
        .expect("the reload slot must be released after a failed preparation");
    assert_eq!(follow_up.reload_id, accepted.reload_id + 1);
    runtime.stop_sessions().await;
}

#[tokio::test]
async fn failed_preparation_under_rollback_policy_restores_the_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let previous = "[censorship]\ntls_domain = \"before.example\"\n";
    let written = "[censorship]\ntls_domain = \"after.example\"\n";
    tokio::fs::write(&config_path, written).await.unwrap();

    let runtime = test_runtime_generation(1, ProxyConfig::default());
    let active_runtime = Arc::new(ArcSwap::from(runtime.clone()));
    let (control, commands) = ReloadControl::channel(runtime.id);
    let (detected_ips_tx, _detected_ips_rx) = watch::channel((None, None));
    let (runtime_watch_tx, _runtime_watch_rx) = watch::channel(Some(runtime.watch_state()));
    let mut supervisor = ReloadSupervisor {
        active_runtime,
        control: control.clone(),
        commands,
        config_path: config_path.clone(),
        process: ProcessScope::new(&ProxyConfig::default()).await,
        detected_ips_tx,
        runtime_log_filter: runtime_log_filter(),
        runtime_watch_tx,
        deadlines: true,
        forced_middle_end_teardown: None,
    };

    let ticket = control
        .reserve(ReloadRequest {
            failure_policy: ReloadFailurePolicy::Rollback,
            ..ReloadRequest::default()
        })
        .await
        .unwrap();
    ticket
        .dispatch(
            invalid_config(),
            None,
            crate::api::config_store::compute_revision(written),
            Some(ConfigRollback {
                path: config_path.clone(),
                previous_content: previous.to_string(),
                written_revision: crate::api::config_store::compute_revision(written),
            }),
        )
        .await
        .unwrap();

    let command = supervisor.commands.recv().await.unwrap();
    supervisor.reload(command, &CancellationToken::new()).await;

    assert_eq!(
        tokio::fs::read_to_string(&config_path).await.unwrap(),
        previous,
        "rollback must undo the patch the API already committed"
    );
    assert_eq!(control.status(1).await.unwrap().state, ReloadPhase::Failed);
    runtime.stop_sessions().await;
}
