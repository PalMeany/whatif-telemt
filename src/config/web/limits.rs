//! Resource ceilings, per-profile overrides, and carrier timeouts.

use serde::{Deserialize, Serialize};

use crate::error::{ProxyError, Result};

use super::defaults::*;
use super::{CarrierMode, MAX_CARRIER_BATCH_BYTES};

/// Process-wide resource ceilings for the WEB relay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebLimits {
    /// Largest accepted request header block.
    #[serde(default = "default_max_header_bytes")]
    pub max_header_bytes: usize,

    /// Largest accepted carrier request body.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,

    /// Largest accepted single frame payload.
    #[serde(default = "default_max_frame_payload")]
    pub max_frame_payload: usize,

    /// Target size of one downlink batch.
    #[serde(default = "default_carrier_batch_bytes")]
    pub carrier_batch_bytes: usize,

    /// Live streams one session may hold.
    #[serde(default = "default_max_streams_per_session")]
    pub max_streams_per_session: usize,

    /// Stream-id tombstones retained per session for close races.
    #[serde(default = "default_max_closed_stream_ids")]
    pub max_closed_stream_ids: usize,

    /// Queued bytes one session may charge.
    #[serde(default = "default_max_pending_per_session")]
    pub max_pending_per_session: usize,

    /// Queued bytes all sessions may charge together.
    #[serde(default = "default_max_pending_global")]
    pub max_pending_global: usize,

    /// Queued items one session may charge.
    #[serde(default = "default_max_pending_items_per_session")]
    pub max_pending_items_per_session: usize,

    /// Queued items all sessions may charge together.
    #[serde(default = "default_max_pending_items_global")]
    pub max_pending_items_global: usize,

    /// Sessions one client address may hold; `0` disables the per-IP ceiling.
    #[serde(default)]
    pub max_sessions_per_ip: usize,

    /// Live sessions across the process.
    #[serde(default = "default_max_sessions_global")]
    pub max_sessions_global: usize,

    /// Live streams across the process.
    #[serde(default = "default_max_streams_global")]
    pub max_streams_global: usize,

    /// Backend connections that may be establishing at once.
    #[serde(default = "default_max_backend_dials_in_flight")]
    pub max_backend_dials_in_flight: usize,

    /// Sustained session creation rate.
    #[serde(default = "default_new_sessions_per_minute")]
    pub new_sessions_per_minute: usize,

    /// Session creation burst.
    #[serde(default = "default_new_sessions_burst")]
    pub new_sessions_burst: usize,

    /// Sustained stream creation rate.
    #[serde(default = "default_new_streams_per_minute")]
    pub new_streams_per_minute: usize,

    /// Stream creation burst.
    #[serde(default = "default_new_streams_burst")]
    pub new_streams_burst: usize,

    /// Unconsumed bootstraps one client address may hold; `0` disables it.
    #[serde(default)]
    pub max_bootstraps_per_ip: usize,

    /// Unconsumed bootstraps across the process.
    #[serde(default = "default_max_bootstraps_global")]
    pub max_bootstraps_global: usize,

    /// Sustained bootstrap issuance rate.
    #[serde(default = "default_new_bootstraps_per_minute")]
    pub new_bootstraps_per_minute: usize,

    /// Bootstrap issuance burst.
    #[serde(default = "default_new_bootstraps_burst")]
    pub new_bootstraps_burst: usize,

    /// Largest number of capability profiles.
    #[serde(default = "default_max_profiles")]
    pub max_profiles: usize,
}

/// Per-profile overrides of the process-wide ceilings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebProfileLimits {
    /// Live sessions for this profile.
    #[serde(default)]
    pub max_sessions: usize,

    /// Live streams for this profile.
    #[serde(default)]
    pub max_streams: usize,

    /// Backend dials in flight for this profile.
    #[serde(default)]
    pub max_backend_dials_in_flight: usize,

    /// Sustained session creation rate for this profile.
    #[serde(default)]
    pub new_sessions_per_minute: usize,

    /// Session creation burst for this profile.
    #[serde(default)]
    pub new_sessions_burst: usize,

    /// Sustained stream creation rate for this profile.
    #[serde(default)]
    pub new_streams_per_minute: usize,

    /// Stream creation burst for this profile.
    #[serde(default)]
    pub new_streams_burst: usize,

    /// Live streams one session of this profile may hold.
    #[serde(default)]
    pub max_streams_per_session: usize,

    /// Queued bytes one session of this profile may charge.
    #[serde(default)]
    pub max_pending_per_session: usize,
}

impl WebProfileLimits {
    /// Fills unset (`0`) overrides from the process-wide ceilings.
    pub fn with_defaults(&self, global: &WebLimits) -> WebProfileLimits {
        let mut result = self.clone();
        if result.max_sessions == 0 {
            result.max_sessions = global.max_sessions_global;
        }
        if result.max_streams == 0 {
            result.max_streams = global.max_streams_global;
        }
        if result.max_backend_dials_in_flight == 0 {
            result.max_backend_dials_in_flight =
                global.max_backend_dials_in_flight.min(result.max_streams);
        }
        if result.new_sessions_per_minute == 0 {
            result.new_sessions_per_minute = global.new_sessions_per_minute;
        }
        if result.new_sessions_burst == 0 {
            result.new_sessions_burst = global.new_sessions_burst;
        }
        if result.new_streams_per_minute == 0 {
            result.new_streams_per_minute = global.new_streams_per_minute;
        }
        if result.new_streams_burst == 0 {
            result.new_streams_burst = global.new_streams_burst;
        }
        if result.max_streams_per_session == 0 {
            result.max_streams_per_session = global.max_streams_per_session.min(result.max_streams);
        }
        if result.max_pending_per_session == 0 {
            result.max_pending_per_session = global.max_pending_per_session;
        }
        result
    }
}

/// One capability profile: an MTProxy secret bound to a backend and carrier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebProfileConfig {
    /// Operator-visible profile name, also used for per-profile accounting.
    pub name: String,

    /// MTProxy secret in hex or base64url form.
    pub secret: String,

    /// `internal` or a numeric loopback `ip:port`.
    #[serde(default = "default_backend")]
    pub backend: String,

    /// Carrier mode for this profile; falls back to `web.carrier_mode`.
    #[serde(default)]
    pub carrier_mode: Option<CarrierMode>,

    /// Per-profile ceilings.
    #[serde(default)]
    pub limits: WebProfileLimits,
}

/// Timeouts controlling carrier liveness and session lifetime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebTimeouts {
    /// Deadline for connecting a loopback backend.
    #[serde(default = "default_backend_dial_ms")]
    pub backend_dial_ms: u64,

    /// Long-poll parking period, also the WebSocket ping cadence.
    #[serde(default = "default_long_poll_ms")]
    pub long_poll_ms: u64,

    /// Idle period after which a session without carrier activity is closed.
    #[serde(default = "default_reconnect_grace_ms")]
    pub reconnect_grace_ms: u64,

    /// Bootstrap token lifetime.
    #[serde(default = "default_bootstrap_lifetime_ms")]
    pub bootstrap_lifetime_ms: u64,

    /// Deadline for reading a request header block.
    #[serde(default = "default_read_header_ms")]
    pub read_header_ms: u64,

    /// Deadline for reading or discarding a request body.
    #[serde(default = "default_body_read_ms")]
    pub body_read_ms: u64,

    /// Keep-alive idle period for a carrier connection.
    #[serde(default = "default_idle_ms")]
    pub idle_ms: u64,
}

impl WebLimits {
    pub(super) fn validate(&self) -> Result<()> {
        if self.max_header_bytes < 4096
            || self.max_body_bytes < 1024
            || self.max_frame_payload == 0
            || self.max_frame_payload > crate::web::frame::MAX_PAYLOAD
            || self.carrier_batch_bytes < 256 * 1024
            || self.carrier_batch_bytes > self.max_body_bytes
        {
            return Err(ProxyError::Config(
                "invalid web HTTP or frame limits".to_string(),
            ));
        }
        if self.carrier_batch_bytes > MAX_CARRIER_BATCH_BYTES {
            return Err(ProxyError::Config(
                "web.limits.carrier_batch_bytes must not exceed the 2 MiB carrier message cap"
                    .to_string(),
            ));
        }
        let positive = [
            self.max_streams_per_session,
            self.max_closed_stream_ids,
            self.max_pending_per_session,
            self.max_pending_global,
            self.max_pending_items_per_session,
            self.max_pending_items_global,
            self.max_sessions_global,
            self.max_streams_global,
            self.max_backend_dials_in_flight,
            self.new_sessions_per_minute,
            self.new_sessions_burst,
            self.new_streams_per_minute,
            self.new_streams_burst,
            self.max_bootstraps_global,
            self.new_bootstraps_per_minute,
            self.new_bootstraps_burst,
            self.max_profiles,
        ];
        if positive.contains(&0) {
            return Err(ProxyError::Config(
                "all web resource limits must be positive".to_string(),
            ));
        }
        if self.max_pending_global < self.max_pending_per_session
            || self.max_pending_items_global < self.max_pending_items_per_session
            || self.max_sessions_global < self.max_sessions_per_ip
            || self.max_streams_global < self.max_streams_per_session
            || self.max_streams_global < self.max_backend_dials_in_flight
            || self.max_bootstraps_global < self.max_bootstraps_per_ip
        {
            return Err(ProxyError::Config(
                "global web limits must not be smaller than per-session or per-IP limits"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

impl WebProfileLimits {
    pub(super) fn validate(&self, global: &WebLimits, name: &str) -> Result<()> {
        let checks = [
            (
                "max_sessions",
                self.max_sessions,
                global.max_sessions_global,
            ),
            ("max_streams", self.max_streams, global.max_streams_global),
            (
                "max_backend_dials_in_flight",
                self.max_backend_dials_in_flight,
                global.max_backend_dials_in_flight,
            ),
            (
                "new_sessions_per_minute",
                self.new_sessions_per_minute,
                global.new_sessions_per_minute,
            ),
            (
                "new_sessions_burst",
                self.new_sessions_burst,
                global.new_sessions_burst,
            ),
            (
                "new_streams_per_minute",
                self.new_streams_per_minute,
                global.new_streams_per_minute,
            ),
            (
                "new_streams_burst",
                self.new_streams_burst,
                global.new_streams_burst,
            ),
            (
                "max_streams_per_session",
                self.max_streams_per_session,
                global.max_streams_per_session,
            ),
            (
                "max_pending_per_session",
                self.max_pending_per_session,
                global.max_pending_per_session,
            ),
        ];
        for (field, value, limit) in checks {
            if value > limit {
                return Err(ProxyError::Config(format!(
                    "web profile '{name}': {field} must be between 0 and {limit}"
                )));
            }
        }
        let resolved = self.with_defaults(global);
        if resolved.max_streams_per_session > resolved.max_streams {
            return Err(ProxyError::Config(format!(
                "web profile '{name}': max_streams_per_session must not exceed max_streams"
            )));
        }
        if resolved.max_backend_dials_in_flight > resolved.max_streams {
            return Err(ProxyError::Config(format!(
                "web profile '{name}': max_backend_dials_in_flight must not exceed max_streams"
            )));
        }
        Ok(())
    }
}

impl WebTimeouts {
    pub(super) fn validate(&self) -> Result<()> {
        let values = [
            self.backend_dial_ms,
            self.long_poll_ms,
            self.reconnect_grace_ms,
            self.bootstrap_lifetime_ms,
            self.read_header_ms,
            self.body_read_ms,
            self.idle_ms,
        ];
        if values.contains(&0) {
            return Err(ProxyError::Config(
                "all web timeouts must be positive".to_string(),
            ));
        }
        Ok(())
    }
}

impl Default for WebLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: default_max_header_bytes(),
            max_body_bytes: default_max_body_bytes(),
            max_frame_payload: default_max_frame_payload(),
            carrier_batch_bytes: default_carrier_batch_bytes(),
            max_streams_per_session: default_max_streams_per_session(),
            max_closed_stream_ids: default_max_closed_stream_ids(),
            max_pending_per_session: default_max_pending_per_session(),
            max_pending_global: default_max_pending_global(),
            max_pending_items_per_session: default_max_pending_items_per_session(),
            max_pending_items_global: default_max_pending_items_global(),
            max_sessions_per_ip: 0,
            max_sessions_global: default_max_sessions_global(),
            max_streams_global: default_max_streams_global(),
            max_backend_dials_in_flight: default_max_backend_dials_in_flight(),
            new_sessions_per_minute: default_new_sessions_per_minute(),
            new_sessions_burst: default_new_sessions_burst(),
            new_streams_per_minute: default_new_streams_per_minute(),
            new_streams_burst: default_new_streams_burst(),
            max_bootstraps_per_ip: 0,
            max_bootstraps_global: default_max_bootstraps_global(),
            new_bootstraps_per_minute: default_new_bootstraps_per_minute(),
            new_bootstraps_burst: default_new_bootstraps_burst(),
            max_profiles: default_max_profiles(),
        }
    }
}

impl Default for WebTimeouts {
    fn default() -> Self {
        Self {
            backend_dial_ms: default_backend_dial_ms(),
            long_poll_ms: default_long_poll_ms(),
            reconnect_grace_ms: default_reconnect_grace_ms(),
            bootstrap_lifetime_ms: default_bootstrap_lifetime_ms(),
            read_header_ms: default_read_header_ms(),
            body_read_ms: default_body_read_ms(),
            idle_ms: default_idle_ms(),
        }
    }
}
