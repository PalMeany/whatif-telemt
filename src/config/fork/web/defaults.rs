//! Default values for every configurable WEB proxy field.

use ipnetwork::IpNetwork;

pub(super) fn default_backend() -> String {
    "internal".to_string()
}

pub(super) fn default_web_listen() -> String {
    "127.0.0.1:8080".to_string()
}

pub(super) fn default_web_admin_listen() -> String {
    "127.0.0.1:8081".to_string()
}

pub(super) fn default_trusted_proxies() -> Vec<IpNetwork> {
    vec![
        "127.0.0.0/8".parse().expect("valid loopback CIDR"),
        "::1/128".parse().expect("valid loopback CIDR"),
    ]
}

pub(super) fn default_max_header_bytes() -> usize {
    16 * 1024
}

pub(super) fn default_max_body_bytes() -> usize {
    2 * 1024 * 1024
}

pub(super) fn default_max_frame_payload() -> usize {
    1024 * 1024
}

pub(super) fn default_carrier_batch_bytes() -> usize {
    2 * 1024 * 1024
}

pub(super) fn default_max_streams_per_session() -> usize {
    128
}

pub(super) fn default_max_closed_stream_ids() -> usize {
    4096
}

pub(super) fn default_max_pending_per_session() -> usize {
    32 * 1024 * 1024
}

pub(super) fn default_max_pending_global() -> usize {
    512 * 1024 * 1024
}

pub(super) fn default_max_pending_items_per_session() -> usize {
    16 * 1024
}

pub(super) fn default_max_pending_items_global() -> usize {
    256 * 1024
}

/// Off by default: the per-address session ceiling counts *live* sessions, and
/// most clients of a censorship-circumvention proxy share a carrier-grade NAT
/// address with thousands of strangers. Any value low enough to bound one
/// attacker is low enough to lock out a whole mobile carrier. The session
/// creation rate limits bound the same abuse without that side effect, so this
/// is opt-in for deployments whose clients have addresses of their own.
pub(super) fn default_max_sessions_per_ip() -> usize {
    0
}

pub(super) fn default_max_sessions_global() -> usize {
    128
}

/// Carrier connections served at once.
///
/// Derived from the global stream ceiling rather than fixed: under a lanes
/// carrier every live stream owns a connection, so a cap below
/// `max_streams_global` would refuse streams the stream ceilings allow. The
/// headroom covers session creation and the shared-carrier polls beside them.
pub(super) fn default_max_carrier_connections() -> usize {
    default_max_streams_global() + 1024
}

pub(super) fn default_max_streams_global() -> usize {
    4096
}

pub(super) fn default_max_backend_dials_in_flight() -> usize {
    256
}

pub(super) fn default_new_sessions_per_minute() -> usize {
    600
}

pub(super) fn default_new_sessions_burst() -> usize {
    128
}

pub(super) fn default_new_streams_per_minute() -> usize {
    6000
}

pub(super) fn default_new_streams_burst() -> usize {
    512
}

/// Off by default, for the same reason as `default_max_sessions_per_ip`, and
/// with a worse failure mode: a refused bootstrap cannot be reported without
/// revealing that the capability was valid, so the client is served the
/// ordinary index and fails with no retry and no signal.
pub(super) fn default_max_bootstraps_per_ip() -> usize {
    0
}

pub(super) fn default_max_bootstraps_global() -> usize {
    512
}

pub(super) fn default_new_bootstraps_per_minute() -> usize {
    1200
}

pub(super) fn default_new_bootstraps_burst() -> usize {
    256
}

pub(super) fn default_max_profiles() -> usize {
    32
}

pub(super) fn default_backend_dial_ms() -> u64 {
    5_000
}

pub(super) fn default_long_poll_ms() -> u64 {
    25_000
}

pub(super) fn default_reconnect_grace_ms() -> u64 {
    120_000
}

pub(super) fn default_bootstrap_lifetime_ms() -> u64 {
    120_000
}

pub(super) fn default_read_header_ms() -> u64 {
    10_000
}

pub(super) fn default_body_read_ms() -> u64 {
    30_000
}

pub(super) fn default_idle_ms() -> u64 {
    75_000
}
