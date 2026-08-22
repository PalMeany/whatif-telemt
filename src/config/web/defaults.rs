//! Default values for every configurable WEB proxy field.

use ipnetwork::IpNetwork;

pub(super) fn default_true() -> bool {
    true
}

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

pub(super) fn default_max_sessions_per_ip() -> usize {
    16
}

pub(super) fn default_max_sessions_global() -> usize {
    128
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

pub(super) fn default_max_bootstraps_per_ip() -> usize {
    32
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
