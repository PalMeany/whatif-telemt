//! Control-plane operations for callers that are not the HTTP API.
//!
//! The Telegram bot performs the same user operations the API does, against
//! the same files, so it must not have its own copy of them and must not race
//! the API while writing. This exposes one process-owned handle that shares the
//! API's mutation lock and reuses the batch engine behind `POST /v1/bulk`, so
//! there is exactly one implementation of "what a user operation does".

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::{Mutex, watch};

use crate::config::ProxyConfig;
use crate::maestro::generation::RuntimeGeneration;

pub(crate) use super::bulk::BulkAction;
use super::bulk::{apply_operation, run_runtime_effects};
use super::config_store::{load_config_from_disk, save_access_sections_to_disk};
use super::users::build_user_links;

/// Process-owned control-plane handle.
#[derive(Clone)]
pub(crate) struct ControlPlane {
    /// Root configuration file every mutation is written through.
    config_path: PathBuf,
    /// Shared with the HTTP API, so the two serialise against each other.
    mutation_lock: Arc<Mutex<()>>,
    /// Live runtime, read per call rather than captured.
    active_runtime: Arc<ArcSwap<RuntimeGeneration>>,
    /// Public addresses detected at start-up, used to render links.
    detected_ips_rx: watch::Receiver<(Option<IpAddr>, Option<IpAddr>)>,
}

/// One user as the control plane reports them.
pub(crate) struct UserSummary {
    /// Configured username.
    pub(crate) name: String,
    /// Whether `access.user_enabled` leaves them admitted.
    pub(crate) enabled: bool,
    /// Live connections attributed to them by the running generation.
    pub(crate) connections: u64,
    /// Bytes charged against their quota so far.
    pub(crate) used_bytes: u64,
    /// Configured quota, when they have one.
    pub(crate) quota_bytes: Option<u64>,
}

impl ControlPlane {
    /// Builds the handle from the state the process already owns.
    pub(crate) fn new(
        config_path: PathBuf,
        mutation_lock: Arc<Mutex<()>>,
        active_runtime: Arc<ArcSwap<RuntimeGeneration>>,
        detected_ips_rx: watch::Receiver<(Option<IpAddr>, Option<IpAddr>)>,
    ) -> Self {
        Self {
            config_path,
            mutation_lock,
            active_runtime,
            detected_ips_rx,
        }
    }

    /// Returns the configuration the running generation is serving.
    pub(crate) fn runtime_config(&self) -> Arc<ProxyConfig> {
        self.active_runtime.load().config()
    }

    /// Returns the generation currently serving traffic.
    pub(crate) fn runtime(&self) -> Arc<RuntimeGeneration> {
        self.active_runtime.load_full()
    }

    /// Lists users as they stand on disk, with live counters attached.
    ///
    /// Read from disk rather than from the runtime so a user added seconds ago
    /// is listed before the config watcher has picked the change up.
    pub(crate) async fn list_users(&self) -> Result<Vec<UserSummary>, String> {
        let cfg = self.load().await?;
        let runtime = self.runtime();
        let mut names = cfg.access.users.keys().cloned().collect::<Vec<_>>();
        names.sort();
        Ok(names
            .into_iter()
            .map(|name| UserSummary {
                enabled: cfg.access.is_user_enabled(&name),
                connections: runtime.stats.get_user_curr_connects(&name),
                used_bytes: runtime.stats.get_user_quota_used(&name),
                quota_bytes: cfg.access.user_data_quota.get(&name).copied(),
                name,
            })
            .collect())
    }

    /// Renders the `tg://` and `t.me` links for one user.
    ///
    /// Reuses the API's own resolver so a bot and the API never disagree about
    /// which host and port a link should name.
    pub(crate) async fn user_links(&self, user: &str) -> Result<Vec<String>, String> {
        let cfg = self.load().await?;
        let secret = cfg
            .access
            .users
            .get(user)
            .ok_or_else(|| "User not found".to_string())?;
        let (detected_v4, detected_v6) = *self.detected_ips_rx.borrow();
        let links = build_user_links(&cfg, secret, detected_v4, detected_v6);
        let mut urls = links.classic;
        urls.extend(links.secure);
        urls.extend(links.tls);
        urls.extend(links.tls_domains.into_iter().map(|entry| entry.link));
        Ok(urls)
    }

    /// Applies one user operation and writes it.
    ///
    /// Returns the secret the operation issued, when it issued one.
    pub(crate) async fn apply(
        &self,
        action: BulkAction,
        user: Option<String>,
        body: Option<serde_json::Value>,
    ) -> Result<Option<String>, String> {
        let guard = self.mutation_lock.lock().await;
        let mut cfg = self.load().await?;
        let applied = apply_operation(&mut cfg, action, user, body).map_err(|rejected| {
            // The bot renders this straight to a chat, so the API's stable code
            // is carried alongside the sentence an operator reads.
            format!("{} ({})", rejected.message, rejected.code)
        })?;
        cfg.validate()
            .map_err(|error| format!("config validation failed: {error}"))?;
        save_access_sections_to_disk(&self.config_path, &cfg, &applied.sections)
            .await
            .map_err(|failure| failure.message)?;
        drop(guard);

        let runtime = self.runtime();
        run_runtime_effects(
            applied.effects,
            &runtime.stats,
            &runtime.ip_tracker,
            &runtime.proxy_shared,
            runtime
                .config()
                .fork
                .runtime_switches()
                .user_delete_forgets_quota,
        )
        .await;
        Ok(applied.secret)
    }

    async fn load(&self) -> Result<ProxyConfig, String> {
        load_config_from_disk(&self.config_path)
            .await
            .map_err(|failure| failure.message)
    }
}
