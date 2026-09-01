//! Node federation settings: how one panel reaches, or is reached by, another.

use ipnetwork::IpNetwork;
use serde::{Deserialize, Serialize};

use crate::error::{ProxyError, Result};

use super::defaults::{
    default_cluster_clock_skew_secs, default_cluster_nonce_capacity,
    default_cluster_poll_interval_secs, default_cluster_request_timeout_ms,
};

/// Role this node plays inside a federation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ClusterRole {
    /// Manages only itself; no inbound cluster endpoint, no linked nodes.
    #[default]
    Standalone,
    /// Drives linked nodes but exposes no inbound cluster endpoint.
    Master,
    /// Exposes the inbound cluster endpoint and is driven by a master.
    Agent,
    /// Both: a master that can itself be linked into another master.
    MasterAgent,
}

impl ClusterRole {
    /// Wire name used in the panel API and in the UI.
    pub fn as_str(self) -> &'static str {
        match self {
            ClusterRole::Standalone => "standalone",
            ClusterRole::Master => "master",
            ClusterRole::Agent => "agent",
            ClusterRole::MasterAgent => "master-agent",
        }
    }

    /// True when this node may hold linked nodes and drive them.
    pub fn is_master(self) -> bool {
        matches!(self, ClusterRole::Master | ClusterRole::MasterAgent)
    }

    /// True when this node answers signed inbound cluster requests.
    pub fn is_agent(self) -> bool {
        matches!(self, ClusterRole::Agent | ClusterRole::MasterAgent)
    }
}

/// Federation configuration for the panel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelClusterConfig {
    /// Enables the federation features of the panel.
    #[serde(default)]
    pub enabled: bool,

    /// Role played by this node.
    #[serde(default)]
    pub role: ClusterRole,

    /// Human-readable name shown in the master's node list.
    ///
    /// Empty falls back to the system hostname, then to the node identifier.
    #[serde(default)]
    pub node_name: String,

    /// Public base URL a master uses to reach this node's cluster endpoint.
    ///
    /// Only used to render the link string an operator copies into a master;
    /// nothing dials it locally.
    #[serde(default)]
    pub advertise_url: String,

    /// Source networks allowed to reach `/cluster/v1`.
    ///
    /// Empty means "any source", which is only defensible because every request
    /// still has to carry a valid signature. Naming the master's address here
    /// keeps unauthenticated traffic off the endpoint entirely.
    #[serde(default)]
    pub allow_from: Vec<IpNetwork>,

    /// Deadline for one outbound request to a linked node.
    #[serde(default = "default_cluster_request_timeout_ms")]
    pub request_timeout_ms: u64,

    /// Accepted clock difference between master and agent, in seconds.
    #[serde(default = "default_cluster_clock_skew_secs")]
    pub clock_skew_secs: u64,

    /// Replay-window nonces retained per node.
    #[serde(default = "default_cluster_nonce_capacity")]
    pub nonce_capacity: usize,

    /// Interval between background health polls of linked nodes.
    #[serde(default = "default_cluster_poll_interval_secs")]
    pub poll_interval_secs: u64,
}

impl Default for PanelClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            role: ClusterRole::default(),
            node_name: String::new(),
            advertise_url: String::new(),
            allow_from: Vec::new(),
            request_timeout_ms: default_cluster_request_timeout_ms(),
            clock_skew_secs: default_cluster_clock_skew_secs(),
            nonce_capacity: default_cluster_nonce_capacity(),
            poll_interval_secs: default_cluster_poll_interval_secs(),
        }
    }
}

impl PanelClusterConfig {
    /// Validates the federation configuration when it is enabled.
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.role == ClusterRole::Standalone {
            return Err(ProxyError::Config(
                "panel.cluster.enabled requires panel.cluster.role to be master, agent, or \
                 master-agent"
                    .to_string(),
            ));
        }
        if self.node_name.len() > 64 {
            return Err(ProxyError::Config(
                "panel.cluster.node_name must contain at most 64 characters".to_string(),
            ));
        }
        if !self.advertise_url.is_empty() {
            validate_advertise_url(&self.advertise_url)?;
        }
        if !(1_000..=120_000).contains(&self.request_timeout_ms) {
            return Err(ProxyError::Config(
                "panel.cluster.request_timeout_ms must be within [1000, 120000]".to_string(),
            ));
        }
        if !(5..=600).contains(&self.clock_skew_secs) {
            return Err(ProxyError::Config(
                "panel.cluster.clock_skew_secs must be within [5, 600]".to_string(),
            ));
        }
        if !(256..=1_048_576).contains(&self.nonce_capacity) {
            return Err(ProxyError::Config(
                "panel.cluster.nonce_capacity must be within [256, 1048576]".to_string(),
            ));
        }
        if !(5..=3_600).contains(&self.poll_interval_secs) {
            return Err(ProxyError::Config(
                "panel.cluster.poll_interval_secs must be within [5, 3600]".to_string(),
            ));
        }
        Ok(())
    }
}

/// Rejects an advertise URL a master could not use, or should not be handed.
///
/// `http` is accepted only for a loopback host: a master that reaches an agent
/// in plaintext over any routable path hands the link key's protected payloads
/// to whatever sits on that path.
fn validate_advertise_url(raw: &str) -> Result<()> {
    let parsed = url::Url::parse(raw).map_err(|error| {
        ProxyError::Config(format!("panel.cluster.advertise_url is not a URL: {error}"))
    })?;
    match parsed.scheme() {
        "https" => {}
        "http" => {
            let loopback = parsed.host_str().map(is_loopback_host).unwrap_or(false);
            if !loopback {
                return Err(ProxyError::Config(
                    "panel.cluster.advertise_url may only use http for a loopback host".to_string(),
                ));
            }
        }
        other => {
            return Err(ProxyError::Config(format!(
                "panel.cluster.advertise_url scheme '{other}' is not supported"
            )));
        }
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(ProxyError::Config(
            "panel.cluster.advertise_url must contain a host".to_string(),
        ));
    }
    Ok(())
}

/// True when the host component names the local machine.
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> PanelClusterConfig {
        PanelClusterConfig {
            enabled: true,
            role: ClusterRole::Agent,
            ..PanelClusterConfig::default()
        }
    }

    #[test]
    fn standalone_role_is_refused_when_enabled() {
        let mut config = enabled();
        config.role = ClusterRole::Standalone;
        assert!(config.validate().is_err());
    }

    #[test]
    fn advertise_url_requires_https_off_host() {
        let mut config = enabled();
        config.advertise_url = "http://192.0.2.10:8443".to_string();
        assert!(config.validate().is_err());
        config.advertise_url = "http://127.0.0.1:8443".to_string();
        assert!(config.validate().is_ok());
        config.advertise_url = "https://node.example.com".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn role_predicates_match_wire_names() {
        assert!(ClusterRole::MasterAgent.is_master());
        assert!(ClusterRole::MasterAgent.is_agent());
        assert!(!ClusterRole::Master.is_agent());
        assert!(!ClusterRole::Agent.is_master());
        assert_eq!(ClusterRole::MasterAgent.as_str(), "master-agent");
    }
}
