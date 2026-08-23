//! WEB proxy (bridge carrier) configuration.
//!
//! Mirrors the `tproxy-server` reference configuration: one public hostname,
//! an operator-owned public site, and one or more capability profiles that map
//! an MTProxy secret to a carrier mode and a stream backend. Telemt adds the
//! `internal` backend, which terminates demultiplexed streams inside this
//! process instead of dialing a separate MTProxy over loopback.
//!
//! Submodules:
//! - `limits`: resource ceilings, per-profile overrides, and timeouts
//! - `defaults`: default values for every configurable field

use std::net::SocketAddr;

use ipnetwork::IpNetwork;
use serde::{Deserialize, Serialize};

use crate::error::{ProxyError, Result};

mod defaults;
mod limits;

use defaults::{default_trusted_proxies, default_web_admin_listen, default_web_listen};
pub use limits::{WebLimits, WebProfileConfig, WebProfileLimits, WebTimeouts};

/// Largest downlink body a carrier may deliver. The desktop client's browser
/// fallback rejects loopback WebSocket messages above 2 MiB, so a larger relay
/// batch would kill that carrier.
pub const MAX_CARRIER_BATCH_BYTES: usize = 2 * 1024 * 1024;

/// Carrier transport selected by a profile and baked into its bridge page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CarrierMode {
    /// Serialized HTTPS uplink plus one long-poll downlink.
    #[default]
    Https,
    /// One independent HTTPS request lane per logical stream.
    HttpsLanes,
    /// One multiplexed WebSocket for the whole session.
    Websocket,
    /// One WebSocket per logical stream.
    WebsocketLanes,
}

impl CarrierMode {
    /// Wire name used in `X-Carrier-Mode` and in the bridge page.
    pub fn as_str(self) -> &'static str {
        match self {
            CarrierMode::Https => "https",
            CarrierMode::HttpsLanes => "https-lanes",
            CarrierMode::Websocket => "websocket",
            CarrierMode::WebsocketLanes => "websocket-lanes",
        }
    }

    /// True when the mode keeps independent per-stream carrier lanes.
    pub fn uses_lanes(self) -> bool {
        matches!(self, CarrierMode::HttpsLanes | CarrierMode::WebsocketLanes)
    }
}

/// Where an accepted logical stream is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebBackend {
    /// Terminate the stream inside this telemt process with no socket hop.
    Internal,
    /// Dial a numeric loopback MTProxy, matching the reference deployment.
    Loopback(SocketAddr),
}

impl WebBackend {
    /// Parses the configured `backend` value.
    pub fn parse(value: &str) -> Result<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("internal") {
            return Ok(WebBackend::Internal);
        }
        let addr: SocketAddr = trimmed
            .parse()
            .map_err(|_| ProxyError::Config(format!("web backend '{trimmed}' is not ip:port")))?;
        if !addr.ip().is_loopback() {
            return Err(ProxyError::Config(
                "web backend must be `internal` or a numeric loopback address".to_string(),
            ));
        }
        if addr.port() == 0 {
            return Err(ProxyError::Config(
                "web backend port must not be 0".to_string(),
            ));
        }
        Ok(WebBackend::Loopback(addr))
    }
}

/// WEB proxy relay configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebConfig {
    /// Enables the WEB proxy listener.
    #[serde(default)]
    pub enabled: bool,

    /// Carrier listener address. Keep it loopback behind a TLS front proxy.
    #[serde(default = "default_web_listen")]
    pub listen: String,

    /// Admin listener address for `/healthz`, `/readyz` and `/metrics`.
    /// An empty string disables the admin listener.
    #[serde(default = "default_web_admin_listen")]
    pub admin_listen: String,

    /// Public hostname clients configure, in lowercase ASCII/IDNA form.
    #[serde(default)]
    pub hostname: String,

    /// Directory of the operator-owned static site, loaded once at start-up.
    #[serde(default)]
    pub public_dir: Option<String>,

    /// Loopback HTTP application serving the public site instead of a directory.
    #[serde(default)]
    pub public_upstream: Option<String>,

    /// Carrier mode used by profiles that do not select one.
    #[serde(default)]
    pub carrier_mode: CarrierMode,

    /// Derives one profile per `[access.users]` entry so every telemt user can
    /// reach the proxy through the WEB carrier with their existing secret.
    ///
    /// Off by default, and deliberately so: the secrets in `[access.users]` are
    /// the ones operators publish in `tg://proxy` links, and enabling `[web]`
    /// should not silently turn every one of them into a bridge capability as
    /// well. Neither reference derives profiles from anything but its own
    /// dedicated profile source. Turning it on is a decision; the resolved
    /// count is bounded by `web.limits.max_profiles` either way.
    #[serde(default)]
    pub derive_user_profiles: bool,

    /// Front proxies allowed to assert a client address via `X-Forwarded-For`.
    #[serde(default = "default_trusted_proxies")]
    pub trusted_proxies: Vec<IpNetwork>,

    /// Process-wide ceilings.
    #[serde(default)]
    pub limits: WebLimits,

    /// Carrier timeouts.
    #[serde(default)]
    pub timeouts: WebTimeouts,

    /// Explicit capability profiles.
    #[serde(default)]
    pub profiles: Vec<WebProfileConfig>,
}

impl WebConfig {
    /// Validates the WEB relay configuration when it is enabled.
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        crate::web::capability::validate_hostname(&self.hostname)
            .map_err(|reason| ProxyError::Config(format!("web.hostname: {reason}")))?;
        let carrier = parse_listen(&self.listen, "web.listen")?;
        // A carrier reachable off-host, with no off-host proxy trusted to
        // forward for it, is a plaintext relay whose bridge capabilities and
        // session bearers are readable by anything that can route to it — and
        // every request would be accounted to the front proxy's own address.
        // The reference refuses a non-loopback listener outright; this refuses
        // the combination that makes one indefensible.
        if !carrier.ip().is_loopback()
            && self
                .trusted_proxies
                .iter()
                .all(|network| network.ip().is_loopback())
        {
            return Err(ProxyError::Config(
                "web.listen is not a loopback address, so web.trusted_proxies must name the front \
                 proxy that reaches it. Bind web.listen to 127.0.0.1, or add the front proxy's \
                 address or network to web.trusted_proxies."
                    .to_string(),
            ));
        }
        if !self.admin_listen.is_empty() {
            let admin = parse_listen(&self.admin_listen, "web.admin_listen")?;
            if !admin.ip().is_loopback() {
                return Err(ProxyError::Config(
                    "web.admin_listen must be a loopback address".to_string(),
                ));
            }
            if self.admin_listen == self.listen {
                return Err(ProxyError::Config(
                    "web.listen and web.admin_listen must differ".to_string(),
                ));
            }
        }
        let has_dir = self.public_dir.as_deref().is_some_and(|v| !v.is_empty());
        let has_upstream = self
            .public_upstream
            .as_deref()
            .is_some_and(|v| !v.is_empty());
        if has_dir == has_upstream {
            return Err(ProxyError::Config(
                "exactly one of web.public_dir or web.public_upstream is required".to_string(),
            ));
        }
        if let Some(upstream) = self.public_upstream.as_deref().filter(|v| !v.is_empty()) {
            validate_public_upstream(upstream)?;
        }
        self.limits.validate()?;
        self.timeouts.validate()?;
        if self.profiles.len() > self.limits.max_profiles {
            return Err(ProxyError::Config(format!(
                "web.profiles must contain at most {} entries",
                self.limits.max_profiles
            )));
        }
        let mut names = std::collections::HashSet::new();
        for profile in &self.profiles {
            if profile.name.is_empty() || profile.name.len() > 64 {
                return Err(ProxyError::Config(
                    "web profile name must contain 1-64 characters".to_string(),
                ));
            }
            if !names.insert(profile.name.as_str()) {
                return Err(ProxyError::Config(format!(
                    "duplicate web profile name '{}'",
                    profile.name
                )));
            }
            WebBackend::parse(&profile.backend)?;
            crate::web::capability::decode_secret(&profile.secret).map_err(|reason| {
                ProxyError::Config(format!("web profile '{}': {reason}", profile.name))
            })?;
            profile.limits.validate(&self.limits, &profile.name)?;
        }
        if self.profiles.is_empty() && !self.derive_user_profiles {
            return Err(ProxyError::Config(
                "web requires at least one profile or web.derive_user_profiles=true".to_string(),
            ));
        }
        Ok(())
    }

    /// Resolves the effective carrier mode for one profile entry.
    pub fn profile_carrier_mode(&self, profile: &WebProfileConfig) -> CarrierMode {
        profile.carrier_mode.unwrap_or(self.carrier_mode)
    }
}

fn parse_listen(value: &str, field: &str) -> Result<SocketAddr> {
    value
        .parse::<SocketAddr>()
        .map_err(|_| ProxyError::Config(format!("{field} must be a numeric ip:port address")))
}

fn validate_public_upstream(raw: &str) -> Result<()> {
    let rest = raw.strip_prefix("http://").ok_or_else(|| {
        ProxyError::Config(
            "web.public_upstream must use http on a numeric loopback address".to_string(),
        )
    })?;
    if rest.contains('/') || rest.contains('?') || rest.contains('#') || rest.contains('@') {
        return Err(ProxyError::Config(
            "web.public_upstream must contain only scheme, loopback address, and port".to_string(),
        ));
    }
    let addr = parse_listen(rest, "web.public_upstream")?;
    if !addr.ip().is_loopback() || addr.port() == 0 {
        return Err(ProxyError::Config(
            "web.public_upstream must be a numeric loopback address".to_string(),
        ));
    }
    Ok(())
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: default_web_listen(),
            admin_listen: default_web_admin_listen(),
            hostname: String::new(),
            public_dir: None,
            public_upstream: None,
            carrier_mode: CarrierMode::default(),
            derive_user_profiles: false,
            trusted_proxies: default_trusted_proxies(),
            limits: WebLimits::default(),
            timeouts: WebTimeouts::default(),
            profiles: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_config() -> WebConfig {
        WebConfig {
            enabled: true,
            hostname: "proxy.example.com".to_string(),
            public_dir: Some("site".to_string()),
            // Explicit: deriving a bridge capability for every `[access.users]`
            // secret is opt-in, so a config that names no profile source is
            // deliberately invalid.
            derive_user_profiles: true,
            ..WebConfig::default()
        }
    }

    #[test]
    fn defaults_validate_when_enabled() {
        assert!(enabled_config().validate().is_ok());
    }

    #[test]
    fn a_profile_source_must_be_chosen_explicitly() {
        let mut config = enabled_config();
        config.derive_user_profiles = false;
        let error = config.validate().expect_err("no profile source");
        assert!(error.to_string().contains("derive_user_profiles"));
    }

    #[test]
    fn requires_exactly_one_public_source() {
        let mut config = enabled_config();
        config.public_upstream = Some("http://127.0.0.1:3000".to_string());
        assert!(config.validate().is_err());
        config.public_dir = None;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_non_loopback_backend() {
        assert!(WebBackend::parse("8.8.8.8:443").is_err());
        assert_eq!(WebBackend::parse("internal").unwrap(), WebBackend::Internal);
        assert!(matches!(
            WebBackend::parse("127.0.0.1:2398").unwrap(),
            WebBackend::Loopback(_)
        ));
    }

    #[test]
    fn rejects_oversized_carrier_batch() {
        let mut config = enabled_config();
        config.limits.max_body_bytes = 4 * 1024 * 1024;
        config.limits.carrier_batch_bytes = 4 * 1024 * 1024;
        assert!(config.validate().is_err());
    }

    #[test]
    fn profile_limits_inherit_global_ceilings() {
        let global = WebLimits::default();
        let resolved = WebProfileLimits::default().with_defaults(&global);
        assert_eq!(resolved.max_sessions, global.max_sessions_global);
        assert_eq!(resolved.max_streams, global.max_streams_global);
        assert_eq!(
            resolved.max_streams_per_session,
            global.max_streams_per_session
        );
    }
}
