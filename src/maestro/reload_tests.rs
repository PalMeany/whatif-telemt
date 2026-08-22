use super::*;

#[test]
fn request_defaults_to_instant_keep_new() {
    let request: ReloadRequest = serde_json::from_str("{}").unwrap();
    assert_eq!(request, ReloadRequest::default());
    assert_eq!(request.validate(), Ok(()));
}
#[test]
fn drain_requires_bounded_timeout() {
    let missing = ReloadRequest {
        mode: ReloadMode::Drain,
        ..ReloadRequest::default()
    };
    assert!(missing.validate().is_err());
    let valid = ReloadRequest {
        mode: ReloadMode::Drain,
        timeout_secs: Some(30),
        ..ReloadRequest::default()
    };
    assert_eq!(valid.validate(), Ok(()));
}

#[test]
fn patch_query_parses_reload_policy() {
    let request =
        ReloadRequest::from_query(Some("reload=drain&timeout_secs=30&failure_policy=rollback"))
            .unwrap()
            .unwrap();
    assert_eq!(request.mode, ReloadMode::Drain);
    assert_eq!(request.timeout_secs, Some(30));
    assert_eq!(request.failure_policy, ReloadFailurePolicy::Rollback);
    assert!(ReloadRequest::from_query(Some("timeout_secs=30")).is_err());
}

#[test]
fn status_uses_documented_deferred_process_fields_key() {
    let status = ReloadStatus {
        reload_id: 1,
        target_generation: 2,
        config_revision: "revision".to_string(),
        state: ReloadPhase::Succeeded,
        mode: ReloadMode::Instant,
        failure_policy: ReloadFailurePolicy::KeepNew,
        requested_at_epoch_secs: 10,
        started_at_epoch_secs: Some(11),
        finished_at_epoch_secs: Some(12),
        deferred_fields: vec!["server.listeners".to_string()],
        warnings: Vec::new(),
        error: None,
        error_kind: None,
    };
    let value = serde_json::to_value(status).unwrap();

    assert_eq!(
        value["deferred_process_fields"],
        serde_json::json!(["server.listeners"])
    );
    assert!(value.get("deferred_fields").is_none());
}

#[tokio::test]
async fn coordinator_rejects_concurrent_reload_and_releases_terminal_slot() {
    let (control, mut receiver) = ReloadControl::channel(1);
    let first = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "rev-1".to_string(),
            ReloadRequest::default(),
        )
        .await
        .unwrap();
    let _command = receiver.recv().await.unwrap();
    let second = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "rev-2".to_string(),
            ReloadRequest::default(),
        )
        .await;
    assert_eq!(second, Err(ReloadSubmitError::InProgress(first.reload_id)));
    control
        .succeed(first.reload_id, first.target_generation)
        .await;
    let third = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "rev-3".to_string(),
            ReloadRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(third.reload_id, first.reload_id + 1);
}

#[tokio::test]
async fn terminal_outcomes_release_slot_and_only_success_advances_generation() {
    let (control, mut receiver) = ReloadControl::channel(7);

    let failed = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "rev-failed".to_string(),
            ReloadRequest::default(),
        )
        .await
        .unwrap();
    let _command = receiver.recv().await.unwrap();
    control
        .mark_phase(failed.reload_id, ReloadPhase::Preparing)
        .await;
    control
        .fail(
            failed.reload_id,
            ReloadError::Probe("prepare failed".to_string()),
        )
        .await;
    let failed_status = control.status(failed.reload_id).await.unwrap();
    assert_eq!(failed_status.state, ReloadPhase::Failed);
    assert_eq!(failed_status.error.as_deref(), Some("prepare failed"));
    assert_eq!(failed_status.error_kind, Some("probe_failed"));
    assert!(failed_status.started_at_epoch_secs.is_some());
    assert!(failed_status.finished_at_epoch_secs.is_some());

    let rolled_back = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "rev-rollback".to_string(),
            ReloadRequest::default(),
        )
        .await
        .unwrap();
    let _command = receiver.recv().await.unwrap();
    assert_eq!(rolled_back.target_generation, 8);
    control
        .rolled_back(
            rolled_back.reload_id,
            ReloadError::RevisionChanged("revision changed".to_string()),
        )
        .await;

    let succeeded = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "rev-success".to_string(),
            ReloadRequest::default(),
        )
        .await
        .unwrap();
    let _command = receiver.recv().await.unwrap();
    assert_eq!(succeeded.target_generation, 8);
    control
        .succeed(succeeded.reload_id, succeeded.target_generation)
        .await;

    let next = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "rev-next".to_string(),
            ReloadRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(next.target_generation, 9);
}

#[tokio::test]
async fn stale_success_cannot_advance_generation_or_release_active_reload() {
    let (control, mut receiver) = ReloadControl::channel(3);
    let active = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "rev-active".to_string(),
            ReloadRequest::default(),
        )
        .await
        .unwrap();
    let _command = receiver.recv().await.unwrap();

    control.succeed(active.reload_id + 100, 99).await;

    assert_eq!(control.in_progress().await, Some(active.reload_id));
    control
        .fail(
            active.reload_id,
            ReloadError::Internal("expected failure".to_string()),
        )
        .await;
    let next = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "rev-next".to_string(),
            ReloadRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(next.target_generation, 4);
}

#[tokio::test]
async fn status_history_retains_only_the_latest_entries() {
    let (control, mut receiver) = ReloadControl::channel(1);
    let mut reload_ids = Vec::new();
    for index in 0..=RELOAD_HISTORY_CAPACITY {
        let accepted = control
            .submit(
                Arc::new(ProxyConfig::default()),
                None,
                format!("rev-{index}"),
                ReloadRequest::default(),
            )
            .await
            .unwrap();
        let _command = receiver.recv().await.unwrap();
        reload_ids.push(accepted.reload_id);
        control
            .fail(
                accepted.reload_id,
                ReloadError::Internal("expected failure".to_string()),
            )
            .await;
    }

    assert!(control.status(reload_ids[0]).await.is_none());
    assert!(control.status(reload_ids[1]).await.is_some());
    assert!(control.status(*reload_ids.last().unwrap()).await.is_some());
}

#[tokio::test]
async fn closed_command_channel_marks_reload_failed_and_releases_slot() {
    let (control, receiver) = ReloadControl::channel(1);
    drop(receiver);

    let result = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "rev-closed".to_string(),
            ReloadRequest::default(),
        )
        .await;

    assert_eq!(result, Err(ReloadSubmitError::MaestroUnavailable));
    assert_eq!(control.in_progress().await, None);
    let status = control.status(1).await.unwrap();
    assert_eq!(status.state, ReloadPhase::Failed);
    assert_eq!(
        status.error.as_deref(),
        Some("maestro command channel is closed")
    );
}

#[tokio::test]
async fn shutdown_gate_rejects_new_commands_without_disturbing_active_status() {
    let (control, mut receiver) = ReloadControl::channel(4);
    let active = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "rev-active".to_string(),
            ReloadRequest::default(),
        )
        .await
        .unwrap();
    let _command = receiver.recv().await.unwrap();

    control.begin_shutdown().await;
    let rejected = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "rev-rejected".to_string(),
            ReloadRequest::default(),
        )
        .await;

    assert_eq!(rejected, Err(ReloadSubmitError::MaestroUnavailable));
    assert_eq!(control.in_progress().await, Some(active.reload_id));
    control
        .fail(
            active.reload_id,
            ReloadError::Internal("shutdown test".to_string()),
        )
        .await;
}

#[tokio::test]
async fn concurrent_reservations_never_share_a_generation_id() {
    // Reading `active_generation` outside the status mutex let two submissions
    // observe the same value; `runtime_watch` then treats the second cutover as
    // a no-op because the generation id did not change.
    const ROUNDS: u64 = 256;
    let (control, mut receiver) = ReloadControl::channel(1);
    let mut seen = std::collections::HashSet::new();

    for _ in 0..ROUNDS {
        let racer = control.clone();
        let contender =
            tokio::spawn(async move { racer.reserve(ReloadRequest::default()).await.ok() });
        let mine = control.reserve(ReloadRequest::default()).await.ok();
        let theirs = contender.await.unwrap();

        // Exactly one of the two wins the single slot.
        let winner = match (mine, theirs) {
            (Some(ticket), None) | (None, Some(ticket)) => ticket,
            (Some(_), Some(_)) => panic!("two reservations held the single reload slot"),
            (None, None) => panic!("neither reservation acquired the free slot"),
        };
        let accepted = winner
            .dispatch(
                Arc::new(ProxyConfig::default()),
                None,
                "rev".to_string(),
                None,
            )
            .await
            .unwrap();
        assert!(
            seen.insert(accepted.target_generation),
            "generation {} was handed out twice",
            accepted.target_generation
        );
        let _command = receiver.recv().await.unwrap();
        control
            .succeed(accepted.reload_id, accepted.target_generation)
            .await;
    }

    assert_eq!(seen.len() as u64, ROUNDS);
}

#[tokio::test]
async fn dropping_a_ticket_releases_the_slot_instead_of_wedging_the_api() {
    // The API reserves before writing to disk; an early `?` on any intermediate
    // step must not leave every later submission answering 409 forever.
    let (control, _receiver) = ReloadControl::channel(1);
    let ticket = control.reserve(ReloadRequest::default()).await.unwrap();
    assert!(control.in_progress().await.is_some());

    drop(ticket);
    for _ in 0..64 {
        if control.in_progress().await.is_none() {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(control.in_progress().await, None);
    let status = control.status(1).await.unwrap();
    assert_eq!(status.state, ReloadPhase::Failed);
    assert_eq!(status.error_kind, Some("internal"));
}

#[tokio::test]
async fn abandoned_ticket_reports_the_failure_and_frees_the_slot() {
    let (control, _receiver) = ReloadControl::channel(1);
    let ticket = control.reserve(ReloadRequest::default()).await.unwrap();

    ticket
        .abandon(ReloadError::ConfigInvalid("no users".to_string()))
        .await;

    assert_eq!(control.in_progress().await, None);
    let status = control.status(1).await.unwrap();
    assert_eq!(status.state, ReloadPhase::Failed);
    assert_eq!(status.error_kind, Some("config_invalid"));
    assert!(control.reserve(ReloadRequest::default()).await.is_ok());
}

#[tokio::test]
async fn cancel_targets_only_the_active_reload() {
    let (control, mut receiver) = ReloadControl::channel(1);
    let accepted = control
        .submit(
            Arc::new(ProxyConfig::default()),
            None,
            "rev".to_string(),
            ReloadRequest::default(),
        )
        .await
        .unwrap();
    let command = receiver.recv().await.unwrap();

    assert!(!control.cancel(accepted.reload_id + 1).await);
    assert!(!command.cancel.is_cancelled());

    assert!(control.cancel(accepted.reload_id).await);
    assert!(command.cancel.is_cancelled());

    control
        .fail(accepted.reload_id, ReloadError::Cancelled)
        .await;
    assert!(!control.cancel(accepted.reload_id).await);
}

#[test]
fn error_kinds_are_distinguishable_for_every_failure_class() {
    let kinds = [
        ReloadError::ConfigInvalid("x".into()).kind(),
        ReloadError::DnsOverrides("x".into()).kind(),
        ReloadError::Probe("x".into()).kind(),
        ReloadError::TlsBootstrap("x".into()).kind(),
        ReloadError::MiddleEndUnavailable("x".into()).kind(),
        ReloadError::RevisionChanged("x".into()).kind(),
        ReloadError::Timeout("x".into()).kind(),
        ReloadError::Cancelled.kind(),
        ReloadError::Internal("x".into()).kind(),
    ];
    let unique: std::collections::HashSet<_> = kinds.iter().collect();
    assert_eq!(unique.len(), kinds.len(), "error kinds must not collide");
}
