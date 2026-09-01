//! Default values for every configurable panel field.

use ipnetwork::IpNetwork;

pub(super) fn default_panel_listen() -> String {
    "127.0.0.1:8443".to_string()
}

pub(super) fn default_trusted_proxies() -> Vec<IpNetwork> {
    vec![
        "127.0.0.0/8".parse().expect("valid loopback CIDR"),
        "::1/128".parse().expect("valid loopback CIDR"),
    ]
}

pub(super) fn default_session_ttl_secs() -> u64 {
    12 * 60 * 60
}

pub(super) fn default_session_idle_timeout_secs() -> u64 {
    30 * 60
}

pub(super) fn default_max_sessions_per_operator() -> usize {
    8
}

pub(super) fn default_max_sessions_total() -> usize {
    512
}

pub(super) fn default_login_max_attempts() -> u32 {
    5
}

pub(super) fn default_login_lockout_secs() -> u64 {
    900
}

pub(super) fn default_password_min_length() -> usize {
    12
}

pub(super) fn default_password_hash_iterations() -> u32 {
    600_000
}

pub(super) fn default_request_body_limit_bytes() -> usize {
    256 * 1024
}

pub(super) fn default_max_connections() -> usize {
    256
}

pub(super) fn default_header_read_timeout_ms() -> u64 {
    10_000
}

pub(super) fn default_request_timeout_ms() -> u64 {
    30_000
}

pub(super) fn default_audit_retention_days() -> u64 {
    90
}

pub(super) fn default_audit_max_bytes() -> u64 {
    64 * 1024 * 1024
}

pub(super) fn default_cluster_request_timeout_ms() -> u64 {
    10_000
}

pub(super) fn default_cluster_clock_skew_secs() -> u64 {
    60
}

pub(super) fn default_cluster_nonce_capacity() -> usize {
    8192
}

pub(super) fn default_cluster_poll_interval_secs() -> u64 {
    30
}

pub(super) fn default_true() -> bool {
    true
}
