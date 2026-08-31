//! The `[fork]` configuration section.
//!
//! Everything this fork adds on top of telemt is configured here and nowhere
//! else. A configuration file written for stock telemt therefore keeps its
//! exact meaning: no key outside `[fork]` behaves differently because this is
//! a fork, and deleting `[fork]` entirely leaves a working proxy.
//!
//! Submodules:
//! - `api`: fork-only control-plane API surface (bulk requests)
//! - `defaults`: default values for every field in this section
//! - `legacy`: migration of the pre-`[fork]` `[web]` schema
//! - `prometheus`: the built-in metrics panel
//! - `runtime`: switches for fork-only runtime behaviour
//! - `telegram`: the Telegram admin bot
//! - `web`: this fork's own WEB proxy transport

use serde::{Deserialize, Serialize};

use crate::error::{ProxyError, Result};

pub mod api;
mod defaults;
pub(crate) mod legacy;
pub mod prometheus;
pub mod runtime;
pub mod telegram;
pub mod web;

pub use api::ForkApiConfig;
pub use prometheus::ForkPrometheusConfig;
pub use runtime::ForkRuntimeConfig;
pub use telegram::ForkTelegramConfig;

use web::WebConfig as ForkWebConfig;

/// Runtime behaviour with every fork-only switch off.
///
/// Handed out instead of the configured switches while `[fork] enabled` is
/// false, so one key turns the whole fork off without touching the rest of the
/// section.
const RUNTIME_ALL_OFF: ForkRuntimeConfig = ForkRuntimeConfig {
    process_admission_budget: false,
    process_buffer_pool: false,
    process_uptime_clock: false,
    reload_cancel: false,
    reload_deadlines: false,
    reload_config_rollback: false,
    reload_validate_candidate: false,
    reload_error_kind: false,
    reload_config_snapshot_hash: false,
    me_writer_teardown: false,
    tls_front_cache_budget_release: false,
    synlimit_generation_reconciler: false,
    shutdown_unbind_listeners_first: false,
    session_admission_closed_metric: false,
    user_delete_forgets_quota: false,
    rust_log_survives_reload: false,
};

/// Which WEB proxy implementation an operator wants to run.
///
/// Two exist and they are not the same transport: telemt's own, configured
/// under `[web]` and bound through a `[[server.listeners]]` entry with
/// `transport = "web"`, and this fork's, configured under `[fork.web]` with
/// its own listener. They can run side by side as long as they do not want the
/// same address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebImplementation {
    /// Runs whichever implementations the rest of the configuration enables.
    #[default]
    Auto,
    /// Runs telemt's own WEB transport and refuses `[fork.web] enabled`.
    Telemt,
    /// Runs this fork's WEB transport and refuses `transport = "web"`.
    Fork,
    /// Runs both.
    Both,
    /// Runs neither, whatever `[web]` and `[fork.web]` say.
    Off,
}

impl WebImplementation {
    /// Wire name used in log lines and API responses.
    pub fn as_str(self) -> &'static str {
        match self {
            WebImplementation::Auto => "auto",
            WebImplementation::Telemt => "telemt",
            WebImplementation::Fork => "fork",
            WebImplementation::Both => "both",
            WebImplementation::Off => "off",
        }
    }
}

/// Everything this fork adds on top of telemt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkConfig {
    /// Master switch for every fork-only feature.
    ///
    /// False makes the process behave like stock telemt: the runtime switches
    /// below all read as off, and the fork's WEB transport, panel, bot and
    /// bulk API stay down regardless of their own `enabled` keys.
    #[serde(default = "defaults::default_true")]
    pub enabled: bool,

    /// Which WEB proxy implementation to run.
    #[serde(default)]
    pub web_implementation: WebImplementation,

    /// Fork-only runtime behaviour.
    #[serde(default)]
    pub runtime: ForkRuntimeConfig,

    /// This fork's own WEB proxy transport.
    #[serde(default)]
    pub web: ForkWebConfig,

    /// The built-in Prometheus panel.
    #[serde(default)]
    pub prometheus: ForkPrometheusConfig,

    /// The Telegram admin bot.
    #[serde(default)]
    pub telegram: ForkTelegramConfig,

    /// Fork-only control-plane API surface.
    #[serde(default)]
    pub api: ForkApiConfig,
}

impl Default for ForkConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            web_implementation: WebImplementation::default(),
            runtime: ForkRuntimeConfig::default(),
            web: ForkWebConfig::default(),
            prometheus: ForkPrometheusConfig::default(),
            telegram: ForkTelegramConfig::default(),
            api: ForkApiConfig::default(),
        }
    }
}

impl ForkConfig {
    /// Runtime switches as they actually apply, master switch folded in.
    ///
    /// Call sites must read behaviour through this rather than through the
    /// `runtime` field, which carries what the operator wrote.
    pub fn runtime_switches(&self) -> &ForkRuntimeConfig {
        if self.enabled {
            &self.runtime
        } else {
            &RUNTIME_ALL_OFF
        }
    }

    /// True while this fork's own WEB transport should run.
    pub fn web_enabled(&self) -> bool {
        self.enabled
            && self.web.enabled
            && matches!(
                self.web_implementation,
                WebImplementation::Auto | WebImplementation::Fork | WebImplementation::Both
            )
    }

    /// True while telemt's own WEB transport should run.
    ///
    /// `requested` is what the rest of the configuration asks for: a `[web]`
    /// section that is enabled and a listener carrying `transport = "web"`.
    pub fn telemt_web_enabled(&self, requested: bool) -> bool {
        if !self.enabled {
            return requested;
        }
        match self.web_implementation {
            WebImplementation::Auto | WebImplementation::Telemt | WebImplementation::Both => {
                requested
            }
            WebImplementation::Fork | WebImplementation::Off => false,
        }
    }

    /// True while the built-in Prometheus panel should be served.
    pub fn prometheus_enabled(&self) -> bool {
        self.enabled && self.prometheus.enabled
    }

    /// True while the Telegram admin bot should run.
    pub fn telegram_enabled(&self) -> bool {
        self.enabled && self.telegram.enabled
    }

    /// True while `POST /v1/bulk` should be served.
    pub fn bulk_enabled(&self) -> bool {
        self.enabled && self.api.bulk_enabled
    }

    /// Validates every enabled fork feature.
    ///
    /// `telemt_web_requested` reports whether the rest of the configuration
    /// asks for telemt's own WEB transport, which this section can veto.
    pub fn validate(&self, telemt_web_requested: bool) -> Result<()> {
        self.validate_selection(telemt_web_requested)?;
        if !self.enabled {
            // A disabled section still has to parse, but nothing it configures
            // will run, so its per-feature rules are not the operator's
            // problem yet.
            return Ok(());
        }
        self.web.validate()?;
        self.prometheus.validate()?;
        self.telegram.validate()?;
        self.api.validate()?;
        Ok(())
    }

    /// Refuses a `web_implementation` that contradicts what is configured.
    ///
    /// A silent override is the failure an operator cannot see: the unit stays
    /// green while the transport they configured never binds.
    ///
    /// Runs before either transport validates its own section, so an operator
    /// who picked one implementation is not first told to finish configuring
    /// the other.
    pub(crate) fn validate_selection(&self, telemt_web_requested: bool) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        match self.web_implementation {
            WebImplementation::Auto | WebImplementation::Both => Ok(()),
            WebImplementation::Telemt if self.web.enabled => Err(ProxyError::Config(
                "fork.web_implementation = \"telemt\" but [fork.web] enabled = true; set \
                 fork.web_implementation = \"both\" to run both transports, or disable [fork.web]"
                    .to_string(),
            )),
            WebImplementation::Fork if telemt_web_requested => Err(ProxyError::Config(
                "fork.web_implementation = \"fork\" but telemt's WEB transport is configured \
                 ([web] enabled with a server.listeners entry using transport = \"web\"); set \
                 fork.web_implementation = \"both\" to run both transports"
                    .to_string(),
            )),
            WebImplementation::Off if self.web.enabled || telemt_web_requested => {
                Err(ProxyError::Config(
                    "fork.web_implementation = \"off\" but a WEB transport is configured; remove \
                     the transport configuration or choose an implementation"
                        .to_string(),
                ))
            }
            _ => Ok(()),
        }
    }
}
