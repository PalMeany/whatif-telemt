use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::config::{ProxyConfig, SynLimitMode};
use crate::maestro::generation::RuntimeWatchState;

mod command;
mod iptables;
mod model;
mod nftables;
mod pf;

use self::command::has_firewall_privileges;
use self::model::{SynLimitNamespace, synlimit_namespace, synlimit_targets};

static ACTIVE_SYNLIMIT_NAMESPACE: Mutex<Option<SynLimitNamespace>> = Mutex::new(None);

/// Process-owned lifecycle handle for the SYN limiter reconciler.
pub(crate) struct SynlimitController {
    shutdown: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

impl SynlimitController {
    /// Stops config observation after any in-flight reconcile completes.
    pub(crate) async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = self.join.await;
    }
}

/// Spawns the process-scoped SYN limiter reconciler for active generations.
pub(crate) fn spawn_synlimit_controller(
    runtime_watch_rx: watch::Receiver<Option<RuntimeWatchState>>,
) -> SynlimitController {
    let shutdown = CancellationToken::new();
    let join = if !cfg!(target_os = "linux") {
        tokio::spawn(watch_active_runtime_configs(
            runtime_watch_rx,
            shutdown.clone(),
            |_generation_id, cfg| async move {
                if has_synlimit_config(&cfg) {
                    warn!(
                        "SYN limiter is configured but unsupported on this OS; skipping netfilter rules"
                    );
                }
            },
        ))
    } else {
        tokio::spawn(watch_active_runtime_configs(
            runtime_watch_rx,
            shutdown.clone(),
            |_generation_id, cfg| async move {
                if let Err(error) = reconcile_synlimit_rules(&cfg).await {
                    warn!("SYN limiter reconcile failed: {error}");
                }
            },
        ))
    };
    SynlimitController { shutdown, join }
}

async fn watch_active_runtime_configs<F, Fut>(
    mut runtime_watch_rx: watch::Receiver<Option<RuntimeWatchState>>,
    shutdown: CancellationToken,
    mut on_config: F,
) where
    F: FnMut(u64, Arc<ProxyConfig>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut current = loop {
        if let Some(state) = runtime_watch_rx.borrow().clone() {
            break state;
        }
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            changed = runtime_watch_rx.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
    };
    if shutdown.is_cancelled() {
        return;
    }
    let initial_config = current.config_rx.borrow().clone();
    on_config(current.generation_id, initial_config).await;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            changed = runtime_watch_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let Some(next) = runtime_watch_rx.borrow().clone() else {
                    continue;
                };
                if next.generation_id != current.generation_id {
                    current = next;
                    let config = current.config_rx.borrow().clone();
                    on_config(current.generation_id, config).await;
                }
            }
            changed = current.config_rx.changed() => {
                if changed.is_err() {
                        let Some(next) = wait_for_new_runtime(
                            &mut runtime_watch_rx,
                            current.generation_id,
                            &shutdown,
                        ).await else {
                        break;
                    };
                    current = next;
                    let config = current.config_rx.borrow().clone();
                    on_config(current.generation_id, config).await;
                    continue;
                }
                let active_generation_id = runtime_watch_rx
                    .borrow()
                    .as_ref()
                    .map(|state| state.generation_id);
                if active_generation_id == Some(current.generation_id) {
                    let cfg = current.config_rx.borrow_and_update().clone();
                    on_config(current.generation_id, cfg).await;
                }
            }
        }
    }
}

async fn wait_for_new_runtime(
    runtime_watch_rx: &mut watch::Receiver<Option<RuntimeWatchState>>,
    previous_generation_id: u64,
    shutdown: &CancellationToken,
) -> Option<RuntimeWatchState> {
    loop {
        if let Some(state) = runtime_watch_rx.borrow().clone()
            && state.generation_id != previous_generation_id
        {
            return Some(state);
        }
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return None,
            changed = runtime_watch_rx.changed() => {
                if changed.is_err() {
                    return None;
                }
            }
        }
    }
}

/// True when any listener asks for the SYN limiter.
fn has_synlimit_config(cfg: &ProxyConfig) -> bool {
    cfg.server
        .listeners
        .iter()
        .any(|listener| !matches!(listener.synlimit, SynLimitMode::Off))
}

/// Installs the complete startup SYN-limiter ruleset before accept loops start.
pub(crate) async fn reconcile_synlimit_rules(cfg: &ProxyConfig) -> Result<(), String> {
    let targets = synlimit_targets(cfg);
    if targets.is_empty() {
        return Ok(());
    }
    if !has_firewall_privileges() {
        return Err(
            "SYN limiter requires root or CAP_NET_ADMIN for startup and shutdown".to_string(),
        );
    }
    let namespace = synlimit_namespace(&targets)
        .ok_or_else(|| "SYN limiter namespace could not be derived".to_string())?;

    if clear_synlimit_rules_for_namespace(&namespace).await? {
        warn!("Removed stale SYN limiter rules left by a previous run before startup");
    }

    let apply_result = async {
        if targets.has_iptables_targets() {
            iptables::apply_synlimit_rules(&targets, &namespace).await?;
        }
        if targets.has_nft_targets() {
            nftables::apply_synlimit_rules(&targets, &namespace).await?;
        }
        if targets.has_pf_targets() {
            pf::apply_synlimit_rules(&targets, &namespace).await?;
        }
        Ok::<(), String>(())
    }
    .await;
    if let Err(apply_error) = apply_result {
        return match clear_synlimit_rules_for_namespace(&namespace).await {
            Ok(_) => Err(apply_error),
            Err(cleanup_error) => Err(format!(
                "{apply_error}; candidate cleanup failed: {cleanup_error}"
            )),
        };
    }

    if let Err(error) = set_active_synlimit_namespace(namespace.clone()) {
        return match clear_synlimit_rules_for_namespace(&namespace).await {
            Ok(_) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; candidate cleanup failed: {cleanup_error}"
            )),
        };
    }
    Ok(())
}

/// Removes the ruleset installed by the current process, if any.
pub(crate) async fn clear_synlimit_rules_all_backends() -> Result<bool, String> {
    let Some(namespace) = active_synlimit_namespace()? else {
        return Ok(false);
    };
    let removed = clear_synlimit_rules_for_namespace(&namespace).await?;
    clear_active_synlimit_namespace(&namespace)?;
    Ok(removed)
}

async fn clear_synlimit_rules_for_namespace(namespace: &SynLimitNamespace) -> Result<bool, String> {
    if !has_firewall_privileges() {
        return Err("SYN limiter cleanup requires root or CAP_NET_ADMIN privileges".to_string());
    }

    let mut errors = Vec::new();
    let mut removed = false;
    match nftables::clear_rules_all_families(namespace).await {
        Ok(value) => removed |= value,
        Err(error) => errors.push(error),
    }
    match iptables::clear_rules_for_binary("iptables", namespace).await {
        Ok(value) => removed |= value,
        Err(error) => errors.push(error),
    }
    match iptables::clear_rules_for_binary("ip6tables", namespace).await {
        Ok(value) => removed |= value,
        Err(error) => errors.push(error),
    }
    match pf::clear_rules(namespace).await {
        Ok(value) => removed |= value,
        Err(error) => errors.push(error),
    }

    if errors.is_empty() {
        Ok(removed)
    } else {
        Err(errors.join("; "))
    }
}

fn set_active_synlimit_namespace(next: SynLimitNamespace) -> Result<(), String> {
    match ACTIVE_SYNLIMIT_NAMESPACE.lock() {
        Ok(mut active) => {
            if active.is_some() {
                return Err("SYN limiter namespace is already active".to_string());
            }
            *active = Some(next);
            Ok(())
        }
        Err(error) => Err(format!(
            "failed to update active SYN limiter namespace: {error}"
        )),
    }
}

fn active_synlimit_namespace() -> Result<Option<SynLimitNamespace>, String> {
    match ACTIVE_SYNLIMIT_NAMESPACE.lock() {
        Ok(active) => Ok(active.clone()),
        Err(error) => Err(format!(
            "failed to read active SYN limiter namespace: {error}"
        )),
    }
}

fn clear_active_synlimit_namespace(expected: &SynLimitNamespace) -> Result<(), String> {
    match ACTIVE_SYNLIMIT_NAMESPACE.lock() {
        Ok(mut active) => {
            if active.as_ref() == Some(expected) {
                *active = None;
            }
            Ok(())
        }
        Err(error) => Err(format!(
            "failed to update active SYN limiter namespace: {error}"
        )),
    }
}
