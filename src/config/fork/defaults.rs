//! Default values for the `[fork]` configuration section.
//!
//! Every fork-only feature defaults to the behaviour this fork has shipped so
//! far, so an existing deployment keeps working when it adds nothing. The three
//! feature subsections that did not exist before (`[fork.prometheus]`,
//! `[fork.telegram]`, `[fork.api]`) default to off.

use ipnetwork::IpNetwork;

/// Loopback-only access list shared by the panel and any future fork listener.
pub(super) fn default_loopback_whitelist() -> Vec<IpNetwork> {
    vec![
        "127.0.0.1/32".parse().expect("valid IPv4 loopback network"),
        "::1/128".parse().expect("valid IPv6 loopback network"),
    ]
}

/// Fork features are on unless an operator turns them off.
pub(super) fn default_true() -> bool {
    true
}

/// Path the built-in Prometheus panel is served on.
pub(super) fn default_panel_path() -> String {
    "/panel".to_string()
}

/// Browser refresh cadence of the panel, in seconds.
pub(super) fn default_panel_refresh_secs() -> u16 {
    5
}

/// Samples the panel keeps per series before dropping the oldest.
pub(super) fn default_panel_history_points() -> u16 {
    120
}

/// Telegram Bot API origin, overridable for a local Bot API server.
pub(super) fn default_telegram_api_base() -> String {
    "https://api.telegram.org".to_string()
}

/// `getUpdates` long-poll timeout, in seconds.
pub(super) fn default_telegram_poll_timeout_secs() -> u16 {
    25
}

/// Per-request timeout applied to every Bot API call, in seconds.
pub(super) fn default_telegram_request_timeout_secs() -> u16 {
    30
}

/// Operations a single bulk request may carry.
pub(super) fn default_bulk_max_operations() -> usize {
    100
}

/// Wall-clock budget for one bulk request, in seconds.
pub(super) fn default_bulk_timeout_secs() -> u16 {
    10
}
