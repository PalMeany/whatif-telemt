use std::future::Future;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{RwLock, watch};
use tokio_util::sync::CancellationToken;

use crate::config::ProxyConfig;
use crate::crypto::SecureRandom;
use crate::ip_tracker::UserIpTracker;
use crate::network::dns_overrides::DnsOverrides;
use crate::network::probe::{decide_network_capabilities, run_probe};
use crate::proxy::route_mode::{RelayRouteMode, RouteRuntimeController};
use crate::proxy::shared_state::ProxySharedState;
use crate::startup::StartupTracker;
use crate::stats::beobachten::BeobachtenStore;
use crate::stats::telemetry::TelemetryPolicy;
use crate::stats::{ReplayChecker, Stats};
use crate::transport::UpstreamManager;
use crate::transport::middle_proxy::MePool;

use super::admission;
use super::generation::{RuntimeGeneration, RuntimeTaskScope};
use super::process_scope::ProcessScope;
use super::reload::ReloadError;
use super::runtime_tasks::RuntimeLogFilter;
use super::{me_startup, runtime_tasks, tls_bootstrap};

pub(crate) struct PreparedRuntime {
    pub(crate) generation: Arc<RuntimeGeneration>,
    pub(crate) detected_ips: (Option<IpAddr>, Option<IpAddr>),
}

/// Awaits `future` unless `cancel` fires first, tearing down `scope` on abort.
///
/// Every long stage of preparation (probe, TLS bootstrap, Middle-End init) can
/// block indefinitely on an unreachable peer. Racing them against the token is
/// only safe if the tasks already spawned into the candidate's scope are stopped
/// on the way out, otherwise a cancelled reload leaks a whole generation's
/// background tasks.
async fn cancellable<F>(
    cancel: &CancellationToken,
    scope: &RuntimeTaskScope,
    future: F,
) -> Result<F::Output, ReloadError>
where
    F: Future,
{
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            scope.stop().await;
            Err(ReloadError::Cancelled)
        }
        value = future => Ok(value),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_runtime(
    generation_id: u64,
    config: ProxyConfig,
    config_snapshot_hash: Option<u64>,
    config_path: &Path,
    process: Arc<ProcessScope>,
    runtime_log_filter: RuntimeLogFilter,
    cancel: CancellationToken,
) -> Result<PreparedRuntime, ReloadError> {
    // Re-validate the candidate here as well as at the API edge: this is the
    // last barrier before `active_runtime.swap`, and every other promotion path
    // (startup, SIGHUP, PATCH) validates.
    config
        .validate()
        .map_err(|error| ReloadError::ConfigInvalid(error.to_string()))?;

    let started_at_epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let startup_tracker = Arc::new(StartupTracker::new(started_at_epoch_secs));
    let task_scope = RuntimeTaskScope::new();
    // Process-scoped start instant: a reload must not reset `telemt_uptime_seconds`.
    let stats = Arc::new(Stats::with_quota_store(
        process.quota_store(),
        process.started_at(),
    ));
    stats.apply_telemetry_policy(TelemetryPolicy::from_config(&config.general.telemetry));

    let upstream_manager = Arc::new(
        UpstreamManager::new(
            config.upstreams.clone(),
            config.general.upstream_connect_retry_attempts,
            config.general.upstream_connect_retry_backoff_ms,
            config.general.upstream_connect_budget_ms,
            config.general.tg_connect,
            config.general.upstream_unhealthy_fail_threshold,
            config.general.upstream_connect_failfast_hard_errors,
            stats.clone(),
        )
        .with_dns_overrides(&config.network.dns_overrides)
        .map_err(|error| {
            ReloadError::DnsOverrides(format!("DNS override preparation failed: {}", error))
        })?,
    );
    let ip_tracker = Arc::new(UserIpTracker::new());
    ip_tracker
        .load_limits(
            config.access.user_max_unique_ips_global_each,
            &config.access.user_max_unique_ips,
        )
        .await;
    ip_tracker
        .set_limit_policy(
            config.access.user_max_unique_ips_mode,
            config.access.user_max_unique_ips_window_secs,
        )
        .await;

    // The Direct envelope and the buffer pool are process-scoped: a drain reload
    // that minted its own would commit two full envelopes at once. Retune the
    // ceiling here, off the cutover path, since resolving it reads system memory.
    process.retune_direct_buffer_budget(&config).await;
    let proxy_shared =
        ProxySharedState::new_with_direct_buffer_budget(process.direct_buffer_budget());
    proxy_shared.set_dns_overrides(
        DnsOverrides::from_entries(&config.network.dns_overrides).map_err(|error| {
            ReloadError::DnsOverrides(format!("DNS override preparation failed: {}", error))
        })?,
    );
    proxy_shared.apply_user_enabled_config(&config.access.user_enabled);
    proxy_shared.traffic_limiter.apply_policy(
        config.access.user_rate_limits.clone(),
        config.access.cidr_rate_limits.clone(),
    );

    let probe = cancellable(
        &cancel,
        &task_scope,
        run_probe(
            &config.network,
            &config.upstreams,
            config.general.middle_proxy_nat_probe,
            config.general.stun_nat_probe_concurrency,
        ),
    )
    .await?
    .map_err(|error| ReloadError::Probe(format!("network probe failed: {}", error)))?;
    let decision =
        decide_network_capabilities(&config.network, &probe, config.general.middle_proxy_nat_ip);
    let prefer_ipv6 = decision.prefer_ipv6();

    let mut tls_domains = Vec::with_capacity(1 + config.censorship.tls_domains.len());
    tls_domains.push(config.censorship.tls_domain.clone());
    for domain in &config.censorship.tls_domains {
        if !tls_domains.contains(domain) {
            tls_domains.push(domain.clone());
        }
    }
    let tls_cache = cancellable(
        &cancel,
        &task_scope,
        tls_bootstrap::bootstrap_tls_front(
            &config,
            &tls_domains,
            upstream_manager.clone(),
            &startup_tracker,
            task_scope.clone(),
            tls_bootstrap::TlsBootstrapPolicy::RequireReady,
        ),
    )
    .await?
    .map_err(|error| ReloadError::TlsBootstrap(error.to_string()))?;

    let beobachten = Arc::new(BeobachtenStore::new());
    let rng = Arc::new(SecureRandom::new());
    let route_mode = if !config.general.use_middle_proxy || config.general.me2dc_fallback {
        RelayRouteMode::Direct
    } else {
        RelayRouteMode::Middle
    };
    let route_runtime = Arc::new(RouteRuntimeController::new(route_mode));
    let me_pool_runtime = Arc::new(RwLock::new(None::<Arc<MePool>>));
    let (me_ready_tx, me_ready_rx) = watch::channel(0_u64);
    let direct_first_startup = config.general.use_middle_proxy && config.general.me2dc_fallback;
    let me_pool = if direct_first_startup {
        None
    } else {
        cancellable(
            &cancel,
            &task_scope,
            me_startup::initialize_me_pool(
                config.general.use_middle_proxy,
                &config,
                &decision,
                &probe,
                &startup_tracker,
                upstream_manager.clone(),
                rng.clone(),
                stats.clone(),
                me_pool_runtime.clone(),
                me_ready_tx.clone(),
                task_scope.clone(),
            ),
        )
        .await?
    };
    if strict_middle_proxy_unavailable(
        config.general.use_middle_proxy,
        direct_first_startup,
        me_pool.is_some(),
    ) {
        task_scope.stop().await;
        return Err(ReloadError::MiddleEndUnavailable(
            "Middle-End pool is required but did not become ready during reload preparation"
                .to_string(),
        ));
    }
    if cancel.is_cancelled() {
        task_scope.stop().await;
        return Err(ReloadError::Cancelled);
    }

    let config = Arc::new(config);
    let replay_checker = Arc::new(ReplayChecker::new(
        config.access.replay_check_len,
        Duration::from_secs(config.access.replay_window_secs),
    ));
    let buffer_pool = process.buffer_pool();
    let max_connections = process.connection_limiter().semaphore();
    let watches = runtime_tasks::spawn_runtime_tasks(
        &config,
        config_path,
        config_snapshot_hash,
        &probe,
        prefer_ipv6,
        decision.ipv4_dc,
        decision.ipv6_dc,
        &startup_tracker,
        stats.clone(),
        upstream_manager.clone(),
        replay_checker.clone(),
        me_pool.clone(),
        rng.clone(),
        ip_tracker.clone(),
        beobachten.clone(),
        me_pool.clone(),
        proxy_shared.clone(),
        me_ready_tx.clone(),
        task_scope.clone(),
    )
    .await;
    let config_rx = watches.config_rx;
    runtime_log_filter.spawn_watcher(watches.log_level_rx, task_scope.clone());
    let initial_admission_open = !config.general.use_middle_proxy || me_pool.is_some();
    let (admission_tx, admission_rx) = watch::channel(initial_admission_open);
    admission::configure_admission_gate(
        &config,
        me_pool.clone(),
        me_pool_runtime.clone(),
        route_runtime.clone(),
        &admission_tx,
        config_rx.clone(),
        me_ready_rx,
        task_scope.clone(),
    )
    .await;

    if direct_first_startup {
        let config_bg = config.clone();
        let decision_bg = decision.clone();
        let probe_bg = probe.clone();
        let startup_tracker_bg = startup_tracker.clone();
        let upstream_manager_bg = upstream_manager.clone();
        let rng_bg = rng.clone();
        let stats_bg = stats.clone();
        let me_pool_runtime_bg = me_pool_runtime.clone();
        let me_ready_tx_bg = me_ready_tx.clone();
        let config_rx_bg = config_rx.clone();
        let task_scope_bg = task_scope.clone();
        let retry_limit = config.general.me_init_retry_attempts;
        task_scope.spawn(async move {
            let mut attempt = 0_u32;
            loop {
                attempt = attempt.saturating_add(1);
                let pool = me_startup::initialize_me_pool(
                    true,
                    config_bg.as_ref(),
                    &decision_bg,
                    &probe_bg,
                    &startup_tracker_bg,
                    upstream_manager_bg.clone(),
                    rng_bg.clone(),
                    stats_bg.clone(),
                    me_pool_runtime_bg.clone(),
                    me_ready_tx_bg.clone(),
                    task_scope_bg.clone(),
                )
                .await;
                if let Some(pool) = pool {
                    runtime_tasks::spawn_middle_proxy_runtime_tasks(
                        config_bg.as_ref(),
                        config_rx_bg,
                        pool,
                        rng_bg,
                        me_ready_tx_bg,
                        task_scope_bg,
                    );
                    break;
                }
                if retry_limit > 0 && attempt >= retry_limit {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }

    let conntrack_scope = task_scope.clone();
    task_scope.spawn(crate::conntrack_control::run_conntrack_controller(
        config_rx.clone(),
        stats.clone(),
        proxy_shared.clone(),
        conntrack_scope.cancellation_token(),
    ));
    let generation = RuntimeGeneration::new(
        generation_id,
        config_rx,
        admission_rx,
        stats,
        upstream_manager,
        replay_checker,
        buffer_pool,
        rng,
        me_pool,
        me_pool_runtime,
        route_runtime,
        tls_cache,
        ip_tracker,
        beobachten,
        proxy_shared,
        max_connections,
        task_scope,
    );
    drop(admission_tx);

    Ok(PreparedRuntime {
        generation,
        detected_ips: (
            probe.detected_ipv4.map(IpAddr::V4),
            probe.detected_ipv6.map(IpAddr::V6),
        ),
    })
}

fn strict_middle_proxy_unavailable(
    use_middle_proxy: bool,
    direct_first_startup: bool,
    pool_available: bool,
) -> bool {
    use_middle_proxy && !direct_first_startup && !pool_available
}

/// Process-owned fields that differ between two configurations.
///
/// The baseline is always "the configuration this process is currently running"
/// versus "the candidate". Callers must not substitute a different baseline
/// (such as disk-before versus disk-after) — the field is reported under one
/// name in `PATCH /v1/config` and in `GET /v1/system/reload/{id}`, and it has to
/// answer the same question in both.
pub(crate) fn deferred_process_fields(
    running: &ProxyConfig,
    candidate: &ProxyConfig,
) -> Vec<String> {
    let (old, new) = (running, candidate);
    let mut fields = Vec::new();
    if old.server.port != new.server.port
        || old.server.proxy_protocol != new.server.proxy_protocol
        || old.server.listen_backlog != new.server.listen_backlog
        || serde_json::to_value(&old.server.listeners).ok()
            != serde_json::to_value(&new.server.listeners).ok()
    {
        fields.push("server.listeners".to_string());
    }
    if old.server.listen_unix_sock != new.server.listen_unix_sock
        || old.server.listen_unix_sock_perm != new.server.listen_unix_sock_perm
    {
        fields.push("server.listen_unix_sock".to_string());
    }
    if old.server.api.listen != new.server.api.listen
        || old.server.api.enabled != new.server.api.enabled
    {
        fields.push("server.api.listen".to_string());
    }
    if old.server.metrics_listen != new.server.metrics_listen
        || old.server.metrics_port != new.server.metrics_port
    {
        fields.push("server.metrics_listen".to_string());
    }
    if old.general.quota_state_path != new.general.quota_state_path {
        fields.push("general.quota_state_path".to_string());
    }
    if old.general.disable_colors != new.general.disable_colors {
        fields.push("general.disable_colors".to_string());
    }
    if old.general.data_path != new.general.data_path {
        fields.push("general.data_path".to_string());
    }
    if serde_json::to_value(&old.logging).ok() != serde_json::to_value(&new.logging).ok() {
        fields.push("logging".to_string());
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_socket_and_logging_changes_are_deferred() {
        let old = ProxyConfig::default();
        let mut new = old.clone();
        new.server.listen_backlog = new.server.listen_backlog.saturating_add(1);
        new.general.disable_colors = !new.general.disable_colors;

        let fields = deferred_process_fields(&old, &new);
        assert!(fields.contains(&"server.listeners".to_string()));
        assert!(fields.contains(&"general.disable_colors".to_string()));
    }

    #[test]
    fn runtime_only_change_does_not_require_process_rebind() {
        let old = ProxyConfig::default();
        let mut new = old.clone();
        new.censorship.tls_domain = "reload.example".to_string();
        assert!(deferred_process_fields(&old, &new).is_empty());
    }

    #[test]
    fn strict_middle_proxy_requires_a_prepared_pool() {
        assert!(strict_middle_proxy_unavailable(true, false, false));
        assert!(!strict_middle_proxy_unavailable(true, false, true));
        assert!(!strict_middle_proxy_unavailable(true, true, false));
        assert!(!strict_middle_proxy_unavailable(false, false, false));
    }

    #[tokio::test]
    async fn prepare_rejects_a_config_that_fails_validation() {
        // Zero users is fatal at startup and rejected by SIGHUP; the reload path
        // has to reject it too, before anything reaches the activation barrier.
        let mut config = ProxyConfig::default();
        config.access.users.clear();
        let process = ProcessScope::new(&config).await;
        let (_layer, handle) = tracing_subscriber::reload::Layer::<
            tracing_subscriber::EnvFilter,
            tracing_subscriber::Registry,
        >::new(tracing_subscriber::EnvFilter::new("info"));

        let error = prepare_runtime(
            2,
            config,
            None,
            Path::new("config.toml"),
            process,
            RuntimeLogFilter::new(handle),
            CancellationToken::new(),
        )
        .await
        .err()
        .expect("an invalid config must never reach the activation barrier");

        assert_eq!(error.kind(), "config_invalid");
        assert!(error.to_string().contains("No users configured"));
    }

    #[tokio::test]
    async fn prepare_bails_out_immediately_when_already_cancelled() {
        let config = ProxyConfig::default();
        let process = ProcessScope::new(&config).await;
        let (_layer, handle) = tracing_subscriber::reload::Layer::<
            tracing_subscriber::EnvFilter,
            tracing_subscriber::Registry,
        >::new(tracing_subscriber::EnvFilter::new("info"));
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = prepare_runtime(
            2,
            config,
            None,
            Path::new("config.toml"),
            process,
            RuntimeLogFilter::new(handle),
            cancel,
        )
        .await
        .err()
        .expect("a cancelled preparation must not produce a candidate");

        assert_eq!(error.kind(), "cancelled");
    }
}
