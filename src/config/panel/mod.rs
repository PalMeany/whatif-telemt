//! Built-in web panel configuration.
//!
//! The panel is a control-plane surface, not a data-plane one: it serves an
//! embedded single-page application and a small JSON API that forwards to the
//! existing telemt Control API (`[server.api]`) of this node and of every node
//! linked into it. Nothing in this module touches the proxy hot path.
//!
//! Submodules:
//! - `cluster`: node federation roles, transport bounds, and replay window
//! - `tls`: optional in-process TLS termination for the panel listener
//! - `defaults`: default values for every configurable field

use ipnetwork::IpNetwork;
use serde::{Deserialize, Serialize};

use crate::error::{ProxyError, Result};

mod cluster;
mod defaults;
mod tls;

pub use cluster::{ClusterRole, PanelClusterConfig};
use defaults::{
    default_audit_max_bytes, default_audit_retention_days, default_header_read_timeout_ms,
    default_login_lockout_secs, default_login_max_attempts, default_max_connections,
    default_max_sessions_per_operator, default_max_sessions_total, default_panel_listen,
    default_password_hash_iterations, default_password_min_length,
    default_request_body_limit_bytes, default_request_timeout_ms,
    default_session_idle_timeout_secs, default_session_ttl_secs, default_true,
    default_trusted_proxies,
};

pub use tls::PanelTlsConfig;

/// Smallest password-hash work factor the panel will run with.
///
/// A lower iteration count is not a tuning choice, it is a downgrade of every
/// stored credential, so it is refused instead of warned about.
const MIN_PASSWORD_HASH_ITERATIONS: u32 = 100_000;

/// Largest password-hash work factor.
///
/// Login is on the request path: an operator who sets this to ten million turns
/// their own login into a denial of service.
const MAX_PASSWORD_HASH_ITERATIONS: u32 = 5_000_000;

/// Built-in web panel configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanelConfig {
    /// Enables the panel listener.
    #[serde(default)]
    pub enabled: bool,

    /// Panel bind address in `IP:PORT` form.
    #[serde(default = "default_panel_listen")]
    pub listen: String,

    /// Directory holding the panel store, audit log, and bootstrap credential.
    ///
    /// Empty resolves to `<config directory>/panel`.
    #[serde(default)]
    pub data_dir: String,

    /// Source networks allowed to reach the panel. Empty means "any source".
    #[serde(default)]
    pub whitelist: Vec<IpNetwork>,

    /// Front proxies allowed to assert a client address via `X-Forwarded-For`.
    #[serde(default = "default_trusted_proxies")]
    pub trusted_proxies: Vec<IpNetwork>,

    /// Base URL of the node's own Control API.
    ///
    /// Empty derives it from `server.api.listen`, mapping an unspecified bind
    /// address onto loopback.
    #[serde(default)]
    pub control_api_url: String,

    /// `Authorization` value used against the node's own Control API.
    ///
    /// Empty reuses `server.api.auth_header`, which is what a single-host
    /// deployment wants; it exists for the case where the panel reaches the
    /// Control API through a front proxy that rewrites the header.
    #[serde(default)]
    pub control_api_token: String,

    /// Absolute lifetime of one operator session, in seconds.
    #[serde(default = "default_session_ttl_secs")]
    pub session_ttl_secs: u64,

    /// Idle timeout of one operator session, in seconds.
    #[serde(default = "default_session_idle_timeout_secs")]
    pub session_idle_timeout_secs: u64,

    /// Concurrent sessions one operator may hold.
    #[serde(default = "default_max_sessions_per_operator")]
    pub max_sessions_per_operator: usize,

    /// Concurrent sessions the panel keeps across all operators.
    #[serde(default = "default_max_sessions_total")]
    pub max_sessions_total: usize,

    /// Failed logins tolerated before an account or address is locked out.
    #[serde(default = "default_login_max_attempts")]
    pub login_max_attempts: u32,

    /// Lockout duration applied after `login_max_attempts`, in seconds.
    #[serde(default = "default_login_lockout_secs")]
    pub login_lockout_secs: u64,

    /// Shortest password the panel accepts.
    #[serde(default = "default_password_min_length")]
    pub password_min_length: usize,

    /// PBKDF2-HMAC-SHA256 iterations used for new and rehashed passwords.
    #[serde(default = "default_password_hash_iterations")]
    pub password_hash_iterations: u32,

    /// Requires every operator to enrol TOTP before anything else is reachable.
    #[serde(default)]
    pub require_totp: bool,

    /// Maximum accepted panel request body size in bytes.
    #[serde(default = "default_request_body_limit_bytes")]
    pub request_body_limit_bytes: usize,

    /// Concurrent panel connections.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,

    /// Deadline for reading one request head, in milliseconds.
    #[serde(default = "default_header_read_timeout_ms")]
    pub header_read_timeout_ms: u64,

    /// Deadline for serving one panel request, in milliseconds.
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,

    /// Records every mutating action into the hash-chained audit log.
    #[serde(default = "default_true")]
    pub audit_enabled: bool,

    /// Days of audit history retained on rotation.
    #[serde(default = "default_audit_retention_days")]
    pub audit_retention_days: u64,

    /// Audit log size that triggers a rotation, in bytes.
    #[serde(default = "default_audit_max_bytes")]
    pub audit_max_bytes: u64,

    /// Optional in-process TLS termination.
    #[serde(default)]
    pub tls: PanelTlsConfig,

    /// Node federation settings.
    #[serde(default)]
    pub cluster: PanelClusterConfig,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: default_panel_listen(),
            data_dir: String::new(),
            whitelist: Vec::new(),
            trusted_proxies: default_trusted_proxies(),
            control_api_url: String::new(),
            control_api_token: String::new(),
            session_ttl_secs: default_session_ttl_secs(),
            session_idle_timeout_secs: default_session_idle_timeout_secs(),
            max_sessions_per_operator: default_max_sessions_per_operator(),
            max_sessions_total: default_max_sessions_total(),
            login_max_attempts: default_login_max_attempts(),
            login_lockout_secs: default_login_lockout_secs(),
            password_min_length: default_password_min_length(),
            password_hash_iterations: default_password_hash_iterations(),
            require_totp: false,
            request_body_limit_bytes: default_request_body_limit_bytes(),
            max_connections: default_max_connections(),
            header_read_timeout_ms: default_header_read_timeout_ms(),
            request_timeout_ms: default_request_timeout_ms(),
            audit_enabled: default_true(),
            audit_retention_days: default_audit_retention_days(),
            audit_max_bytes: default_audit_max_bytes(),
            tls: PanelTlsConfig::default(),
            cluster: PanelClusterConfig::default(),
        }
    }
}

impl PanelConfig {
    /// Validates the panel configuration when it is enabled.
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let listen = self.listen.parse::<std::net::SocketAddr>().map_err(|_| {
            ProxyError::Config("panel.listen must be a numeric ip:port".to_string())
        })?;
        if listen.port() == 0 {
            return Err(ProxyError::Config(
                "panel.listen port must not be 0".to_string(),
            ));
        }
        // A panel on a routable address without TLS publishes its session
        // cookie, its CSRF token, and every secret it renders to anything on
        // the path. The two ways out are in-process TLS or a front proxy that
        // is named here, which is also what makes `X-Forwarded-For` meaningful.
        if !listen.ip().is_loopback() && !self.tls.enabled && !self.has_off_host_trusted_proxy() {
            return Err(ProxyError::Config(
                "panel.listen is not a loopback address, so either panel.tls.enabled must be true \
                 or panel.trusted_proxies must name the TLS front proxy that reaches it"
                    .to_string(),
            ));
        }
        self.tls.validate()?;
        self.cluster.validate()?;
        if !self.control_api_url.is_empty() {
            validate_control_api_url(&self.control_api_url)?;
        }
        if !(60..=30 * 24 * 3_600).contains(&self.session_ttl_secs) {
            return Err(ProxyError::Config(
                "panel.session_ttl_secs must be within [60, 2592000]".to_string(),
            ));
        }
        if !(60..=self.session_ttl_secs).contains(&self.session_idle_timeout_secs) {
            return Err(ProxyError::Config(
                "panel.session_idle_timeout_secs must be within [60, panel.session_ttl_secs]"
                    .to_string(),
            ));
        }
        if !(1..=128).contains(&self.max_sessions_per_operator) {
            return Err(ProxyError::Config(
                "panel.max_sessions_per_operator must be within [1, 128]".to_string(),
            ));
        }
        if self.max_sessions_total < self.max_sessions_per_operator
            || self.max_sessions_total > 65_536
        {
            return Err(ProxyError::Config(
                "panel.max_sessions_total must be within [panel.max_sessions_per_operator, 65536]"
                    .to_string(),
            ));
        }
        if !(1..=100).contains(&self.login_max_attempts) {
            return Err(ProxyError::Config(
                "panel.login_max_attempts must be within [1, 100]".to_string(),
            ));
        }
        if !(1..=86_400).contains(&self.login_lockout_secs) {
            return Err(ProxyError::Config(
                "panel.login_lockout_secs must be within [1, 86400]".to_string(),
            ));
        }
        if !(8..=256).contains(&self.password_min_length) {
            return Err(ProxyError::Config(
                "panel.password_min_length must be within [8, 256]".to_string(),
            ));
        }
        if !(MIN_PASSWORD_HASH_ITERATIONS..=MAX_PASSWORD_HASH_ITERATIONS)
            .contains(&self.password_hash_iterations)
        {
            return Err(ProxyError::Config(format!(
                "panel.password_hash_iterations must be within [{}, {}]",
                MIN_PASSWORD_HASH_ITERATIONS, MAX_PASSWORD_HASH_ITERATIONS
            )));
        }
        if !(1_024..=8 * 1_024 * 1_024).contains(&self.request_body_limit_bytes) {
            return Err(ProxyError::Config(
                "panel.request_body_limit_bytes must be within [1024, 8388608]".to_string(),
            ));
        }
        if !(1..=65_536).contains(&self.max_connections) {
            return Err(ProxyError::Config(
                "panel.max_connections must be within [1, 65536]".to_string(),
            ));
        }
        if !(1_000..=120_000).contains(&self.header_read_timeout_ms) {
            return Err(ProxyError::Config(
                "panel.header_read_timeout_ms must be within [1000, 120000]".to_string(),
            ));
        }
        if !(1_000..=600_000).contains(&self.request_timeout_ms) {
            return Err(ProxyError::Config(
                "panel.request_timeout_ms must be within [1000, 600000]".to_string(),
            ));
        }
        if !(1..=3_650).contains(&self.audit_retention_days) {
            return Err(ProxyError::Config(
                "panel.audit_retention_days must be within [1, 3650]".to_string(),
            ));
        }
        if !(64 * 1_024..=4 * 1_024 * 1_024 * 1_024).contains(&self.audit_max_bytes) {
            return Err(ProxyError::Config(
                "panel.audit_max_bytes must be within [65536, 4294967296]".to_string(),
            ));
        }
        Ok(())
    }

    /// True when at least one trusted proxy sits somewhere other than this host.
    ///
    /// The default list is loopback-only, which cannot be the front proxy of a
    /// listener that is itself not on loopback.
    fn has_off_host_trusted_proxy(&self) -> bool {
        self.trusted_proxies
            .iter()
            .any(|network| !network.ip().is_loopback())
    }
}

/// Rejects a Control API URL the panel could not use safely.
fn validate_control_api_url(raw: &str) -> Result<()> {
    let parsed = url::Url::parse(raw).map_err(|error| {
        ProxyError::Config(format!("panel.control_api_url is not a URL: {error}"))
    })?;
    match parsed.scheme() {
        "https" => {}
        "http" => {
            let loopback = parsed
                .host_str()
                .and_then(|host| {
                    host.trim_start_matches('[')
                        .trim_end_matches(']')
                        .parse::<std::net::IpAddr>()
                        .ok()
                })
                .map(|ip| ip.is_loopback())
                .unwrap_or_else(|| parsed.host_str() == Some("localhost"));
            if !loopback {
                return Err(ProxyError::Config(
                    "panel.control_api_url may only use http for a loopback host".to_string(),
                ));
            }
        }
        other => {
            return Err(ProxyError::Config(format!(
                "panel.control_api_url scheme '{other}' is not supported"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> PanelConfig {
        PanelConfig {
            enabled: true,
            ..PanelConfig::default()
        }
    }

    #[test]
    fn defaults_validate_when_enabled() {
        assert!(enabled().validate().is_ok());
    }

    #[test]
    fn disabled_config_skips_every_bound() {
        let config = PanelConfig {
            listen: "not-an-address".to_string(),
            session_ttl_secs: 0,
            ..PanelConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn routable_listener_requires_tls_or_a_front_proxy() {
        let mut config = enabled();
        config.listen = "0.0.0.0:8443".to_string();
        assert!(config.validate().is_err());

        let mut with_tls = config.clone();
        with_tls.tls = PanelTlsConfig {
            enabled: true,
            cert_path: "cert.pem".to_string(),
            key_path: "key.pem".to_string(),
        };
        assert!(with_tls.validate().is_ok());

        let mut with_proxy = config;
        with_proxy
            .trusted_proxies
            .push("192.0.2.0/24".parse().expect("cidr"));
        assert!(with_proxy.validate().is_ok());
    }

    #[test]
    fn idle_timeout_may_not_exceed_the_absolute_lifetime() {
        let mut config = enabled();
        config.session_ttl_secs = 600;
        config.session_idle_timeout_secs = 1_200;
        assert!(config.validate().is_err());
    }

    #[test]
    fn password_hash_work_factor_may_not_be_downgraded() {
        let mut config = enabled();
        config.password_hash_iterations = 1_000;
        assert!(config.validate().is_err());
    }

    #[test]
    fn control_api_url_may_only_be_plaintext_on_loopback() {
        let mut config = enabled();
        config.control_api_url = "http://203.0.113.9:9091".to_string();
        assert!(config.validate().is_err());
        config.control_api_url = "http://127.0.0.1:9091".to_string();
        assert!(config.validate().is_ok());
    }
}
