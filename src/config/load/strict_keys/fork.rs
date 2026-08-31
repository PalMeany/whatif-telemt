//! Strict-key allowlists for the `[fork]` section.
//!
//! Kept apart from the telemt allowlists so the two stay legible: everything
//! here is fork-only, and everything in the parent module mirrors upstream.

/// Direct children of `[fork]`.
pub(super) const FORK_CONFIG_KEYS: &[&str] = &[
    "enabled",
    "web_implementation",
    "runtime",
    "web",
    "prometheus",
    "telegram",
    "api",
];

/// Keys of `[fork.runtime]`.
pub(super) const FORK_RUNTIME_CONFIG_KEYS: &[&str] = &[
    "process_admission_budget",
    "process_buffer_pool",
    "process_uptime_clock",
    "reload_cancel",
    "reload_deadlines",
    "reload_config_rollback",
    "reload_validate_candidate",
    "reload_error_kind",
    "reload_config_snapshot_hash",
    "me_writer_teardown",
    "tls_front_cache_budget_release",
    "synlimit_generation_reconciler",
    "shutdown_unbind_listeners_first",
    "session_admission_closed_metric",
    "user_delete_forgets_quota",
    "rust_log_survives_reload",
];

/// Keys of `[fork.prometheus]`.
pub(super) const FORK_PROMETHEUS_CONFIG_KEYS: &[&str] = &[
    "enabled",
    "path",
    "listen",
    "whitelist",
    "refresh_secs",
    "history_points",
    "title",
    "show_users",
];

/// Keys of `[fork.telegram]`.
pub(super) const FORK_TELEGRAM_CONFIG_KEYS: &[&str] = &[
    "enabled",
    "token",
    "admins",
    "allow_mutations",
    "api_base",
    "poll_timeout_secs",
    "request_timeout_secs",
    "notify_chats",
];

/// Keys of `[fork.api]`.
pub(super) const FORK_API_CONFIG_KEYS: &[&str] =
    &["bulk_enabled", "bulk_max_operations", "bulk_timeout_secs"];

/// Keys of `[fork.web]`, this fork's own WEB proxy transport.
pub(super) const FORK_WEB_CONFIG_KEYS: &[&str] = &[
    "enabled",
    "listen",
    "admin_listen",
    "hostname",
    "public_dir",
    "public_upstream",
    "carrier_mode",
    "derive_user_profiles",
    "trusted_proxies",
    "limits",
    "timeouts",
    "profiles",
];

/// Keys of `[fork.web.limits]`.
pub(super) const FORK_WEB_LIMITS_CONFIG_KEYS: &[&str] = &[
    "max_header_bytes",
    "max_body_bytes",
    "max_frame_payload",
    "carrier_batch_bytes",
    "max_streams_per_session",
    "max_closed_stream_ids",
    "max_pending_per_session",
    "max_pending_global",
    "max_pending_items_per_session",
    "max_pending_items_global",
    "max_sessions_per_ip",
    "max_sessions_global",
    "max_streams_global",
    "max_backend_dials_in_flight",
    "max_carrier_connections",
    "new_sessions_per_minute",
    "new_sessions_burst",
    "new_streams_per_minute",
    "new_streams_burst",
    "max_bootstraps_per_ip",
    "max_bootstraps_global",
    "new_bootstraps_per_minute",
    "new_bootstraps_burst",
    "max_profiles",
];

/// Keys of `[fork.web.timeouts]`.
pub(super) const FORK_WEB_TIMEOUTS_CONFIG_KEYS: &[&str] = &[
    "backend_dial_ms",
    "long_poll_ms",
    "reconnect_grace_ms",
    "bootstrap_lifetime_ms",
    "read_header_ms",
    "body_read_ms",
    "idle_ms",
];

/// Keys of one `[[fork.web.profiles]]` entry.
pub(super) const FORK_WEB_PROFILE_CONFIG_KEYS: &[&str] =
    &["name", "secret", "backend", "carrier_mode", "limits"];

/// Keys of `[[fork.web.profiles]].limits`.
pub(super) const FORK_WEB_PROFILE_LIMITS_CONFIG_KEYS: &[&str] = &[
    "max_sessions",
    "max_streams",
    "max_backend_dials_in_flight",
    "max_carrier_connections",
    "new_sessions_per_minute",
    "new_sessions_burst",
    "new_streams_per_minute",
    "new_streams_burst",
    "max_streams_per_session",
    "max_pending_per_session",
];

/// Every fork allowlist, for the "did you mean" suggestion index.
pub(super) const FORK_KEY_GROUPS: &[&[&str]] = &[
    FORK_CONFIG_KEYS,
    FORK_RUNTIME_CONFIG_KEYS,
    FORK_PROMETHEUS_CONFIG_KEYS,
    FORK_TELEGRAM_CONFIG_KEYS,
    FORK_API_CONFIG_KEYS,
    FORK_WEB_CONFIG_KEYS,
    FORK_WEB_LIMITS_CONFIG_KEYS,
    FORK_WEB_PROFILE_CONFIG_KEYS,
    FORK_WEB_PROFILE_LIMITS_CONFIG_KEYS,
    FORK_WEB_TIMEOUTS_CONFIG_KEYS,
];
