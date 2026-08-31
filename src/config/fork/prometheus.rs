//! Configuration of the built-in Prometheus panel.
//!
//! The panel is a single self-contained HTML document served next to the
//! existing `/metrics` endpoint. It carries no external references: it scrapes
//! the same exposition this process already renders and draws it client-side,
//! so it works on a host with no outbound network and adds no dependency.

use ipnetwork::IpNetwork;
use serde::{Deserialize, Serialize};

use crate::error::{ProxyError, Result};

use super::defaults::{
    default_loopback_whitelist, default_panel_history_points, default_panel_path,
    default_panel_refresh_secs,
};

/// Largest browser-side history a panel may be asked to retain.
const MAX_HISTORY_POINTS: u16 = 1440;

/// Built-in Prometheus panel settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkPrometheusConfig {
    /// Serves the panel. Off by default.
    #[serde(default)]
    pub enabled: bool,

    /// Path the panel document is served on.
    #[serde(default = "default_panel_path")]
    pub path: String,

    /// Dedicated listener for the panel, as `ip:port`.
    ///
    /// Empty shares the metrics listener (`server.metrics_listen` or
    /// `server.metrics_port`), which is the common case: the panel then
    /// inherits that listener's whitelist and connection budget.
    #[serde(default)]
    pub listen: String,

    /// Networks allowed to reach a dedicated panel listener.
    ///
    /// Ignored while `listen` is empty, because the metrics listener applies
    /// `server.metrics_whitelist` before any HTTP is parsed.
    #[serde(default = "default_loopback_whitelist")]
    pub whitelist: Vec<IpNetwork>,

    /// Seconds between two browser-side scrapes.
    #[serde(default = "default_panel_refresh_secs")]
    pub refresh_secs: u16,

    /// Samples the browser keeps per series before dropping the oldest.
    #[serde(default = "default_panel_history_points")]
    pub history_points: u16,

    /// Heading shown at the top of the panel.
    #[serde(default)]
    pub title: String,

    /// Renders per-user series.
    ///
    /// Off by default: the per-user families carry usernames as labels, and a
    /// panel is a wider audience than a scrape target.
    #[serde(default)]
    pub show_users: bool,
}

impl Default for ForkPrometheusConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_panel_path(),
            listen: String::new(),
            whitelist: default_loopback_whitelist(),
            refresh_secs: default_panel_refresh_secs(),
            history_points: default_panel_history_points(),
            title: String::new(),
            show_users: false,
        }
    }
}

impl ForkPrometheusConfig {
    /// Validates the panel settings when it is enabled.
    pub(super) fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !self.path.starts_with('/') || self.path.len() > 128 {
            return Err(ProxyError::Config(
                "fork.prometheus.path must start with '/' and be at most 128 characters"
                    .to_string(),
            ));
        }
        if self.path == "/metrics" || self.path == "/beobachten" {
            return Err(ProxyError::Config(
                "fork.prometheus.path must not shadow /metrics or /beobachten".to_string(),
            ));
        }
        if !self.listen.is_empty() {
            let addr: std::net::SocketAddr = self.listen.parse().map_err(|_| {
                ProxyError::Config(format!(
                    "fork.prometheus.listen '{}' is not ip:port",
                    self.listen
                ))
            })?;
            if addr.port() == 0 {
                return Err(ProxyError::Config(
                    "fork.prometheus.listen port must not be 0".to_string(),
                ));
            }
            if !addr.ip().is_loopback() && self.whitelist.is_empty() {
                return Err(ProxyError::Config(
                    "fork.prometheus.listen is reachable off-host, so fork.prometheus.whitelist must not \
                     be empty"
                        .to_string(),
                ));
            }
        }
        if self.refresh_secs == 0 {
            return Err(ProxyError::Config(
                "fork.prometheus.refresh_secs must be > 0".to_string(),
            ));
        }
        if self.history_points == 0 || self.history_points > MAX_HISTORY_POINTS {
            return Err(ProxyError::Config(format!(
                "fork.prometheus.history_points must be 1..={MAX_HISTORY_POINTS}"
            )));
        }
        Ok(())
    }
}
