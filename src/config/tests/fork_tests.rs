//! `[fork]` loaded through the production `ProxyConfig::load` path.
//!
//! These cover the three things the section has to get right: a configuration
//! written for stock telemt still means what it meant, an existing fork
//! configuration keeps working, and every fork feature can actually be turned
//! off.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::ProxyConfig;
use crate::config::WebImplementation;

fn write_temp_config(contents: &str) -> PathBuf {
    // A nanosecond clock alone is not guaranteed to differ between two tests
    // running in parallel, so the counter is what keeps one test from loading
    // another's file.
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time must be after unix epoch")
        .as_nanos();
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("telemt-load-fork-{nonce}-{sequence}.toml"));
    fs::write(&path, contents).expect("temp config write must succeed");
    path
}

/// A minimal document plus whatever the caller appends.
fn base_config(extra: &str) -> String {
    format!(
        r#"
[access.users]
alice = "000102030405060708090a0b0c0d0e0f"
{extra}
"#
    )
}

fn load_ok(toml: &str) -> ProxyConfig {
    let path = write_temp_config(toml);
    let config = ProxyConfig::load(&path).expect("configuration must load");
    let _ = fs::remove_file(&path);
    config
}

fn load_err(toml: &str) -> String {
    let path = write_temp_config(toml);
    let error = ProxyConfig::load(&path).expect_err("configuration must be refused");
    let _ = fs::remove_file(&path);
    error.to_string()
}

#[test]
fn a_config_without_a_fork_section_keeps_every_fork_behaviour() {
    // The point of the section is that it is optional. Leaving it out must not
    // silently disable the runtime hardening this fork has always applied.
    let config = load_ok(&base_config(""));

    assert!(config.fork.enabled);
    assert_eq!(config.fork.web_implementation, WebImplementation::Auto);
    let switches = config.fork.runtime_switches();
    assert!(switches.process_admission_budget);
    assert!(switches.reload_cancel);
    assert!(switches.rust_log_survives_reload);
    assert!(switches.shutdown_unbind_listeners_first);
}

#[test]
fn a_config_without_a_fork_section_leaves_every_new_feature_off() {
    let config = load_ok(&base_config(""));

    assert!(!config.fork.web_enabled());
    assert!(!config.fork.prometheus_enabled());
    assert!(!config.fork.telegram_enabled());
    assert!(!config.fork.bulk_enabled());
}

#[test]
fn the_master_switch_turns_every_fork_runtime_deviation_off() {
    // One key has to be enough to make the process behave like stock telemt,
    // because that is the only way an operator can bisect a fork-only change.
    let config = load_ok(&base_config(
        r#"
[fork]
enabled = false
"#,
    ));

    let switches = config.fork.runtime_switches();
    assert!(!switches.process_admission_budget);
    assert!(!switches.process_buffer_pool);
    assert!(!switches.process_uptime_clock);
    assert!(!switches.reload_cancel);
    assert!(!switches.reload_deadlines);
    assert!(!switches.reload_config_rollback);
    assert!(!switches.reload_validate_candidate);
    assert!(!switches.reload_error_kind);
    assert!(!switches.reload_config_snapshot_hash);
    assert!(!switches.me_writer_teardown);
    assert!(!switches.tls_front_cache_budget_release);
    assert!(!switches.synlimit_generation_reconciler);
    assert!(!switches.shutdown_unbind_listeners_first);
    assert!(!switches.session_admission_closed_metric);
    assert!(!switches.user_delete_forgets_quota);
    assert!(!switches.rust_log_survives_reload);
}

#[test]
fn the_master_switch_also_keeps_an_enabled_fork_feature_down() {
    let config = load_ok(&base_config(
        r#"
[fork]
enabled = false

[fork.prometheus]
enabled = true

[fork.api]
bulk_enabled = true
"#,
    ));

    assert!(config.fork.prometheus.enabled, "the operator's key is kept");
    assert!(
        !config.fork.prometheus_enabled(),
        "but the feature does not run"
    );
    assert!(!config.fork.bulk_enabled());
}

#[test]
fn one_runtime_switch_can_be_turned_off_on_its_own() {
    let config = load_ok(&base_config(
        r#"
[fork.runtime]
reload_cancel = false
"#,
    ));

    let switches = config.fork.runtime_switches();
    assert!(!switches.reload_cancel);
    assert!(
        switches.reload_deadlines,
        "an unnamed switch keeps its default"
    );
}

#[test]
fn an_unknown_key_under_fork_is_refused_under_strict_config() {
    let message = load_err(&base_config(
        r#"
[general]
config_strict = true

[fork.runtime]
reload_cancle = false
"#,
    ));

    assert!(
        message.contains("fork.runtime.reload_cancle"),
        "unexpected message: {message}"
    );
    assert!(
        message.contains("reload_cancel"),
        "the nearest known key must be suggested: {message}"
    );
}

#[test]
fn a_legacy_fork_web_section_is_migrated_into_fork_web() {
    // Before telemt grew its own WEB transport this fork owned `[web]`, and an
    // existing deployment must not have to edit its configuration to upgrade.
    let config = load_ok(&base_config(
        r#"
[web]
enabled = true
hostname = "proxy.example.com"
public_dir = "/var/empty/telemt-web-site"
derive_user_profiles = true
carrier_mode = "websocket"
"#,
    ));

    assert!(config.fork.web.enabled);
    assert_eq!(config.fork.web.hostname, "proxy.example.com");
    assert!(config.fork.web_enabled());
}

#[test]
fn a_web_section_mixing_both_transports_is_refused() {
    // Guessing here would bind the wrong transport, so the loader names both
    // sets of keys and asks the operator to split them.
    let message = load_err(&base_config(
        r#"
[web]
enabled = true
hostname = "proxy.example.com"
public_dir = "/var/empty/telemt-web-site"
vhosts = []
"#,
    ));

    assert!(
        message.contains("hostname") && message.contains("vhosts"),
        "the message must name the keys from both schemas: {message}"
    );
}

#[test]
fn a_legacy_web_section_next_to_fork_web_is_refused() {
    let message = load_err(&base_config(
        r#"
[web]
enabled = true
hostname = "legacy.example.com"
public_dir = "/var/empty/telemt-web-site"
derive_user_profiles = true

[fork.web]
enabled = true
hostname = "current.example.com"
public_dir = "/var/empty/telemt-web-site"
derive_user_profiles = true
"#,
    ));

    assert!(
        message.contains("[fork.web]") && message.contains("[web]"),
        "unexpected message: {message}"
    );
}

#[test]
fn web_implementation_fork_refuses_a_telemt_web_listener() {
    // A silent override is the failure an operator cannot see: the unit stays
    // green while the transport they configured never binds.
    let message = load_err(&base_config(
        r#"
[fork]
web_implementation = "fork"

[web]
enabled = true

[[server.listeners]]
ip = "127.0.0.1"
port = 8080
transport = "web"
web_trusted_proxy_cidrs = ["127.0.0.0/8"]
"#,
    ));

    assert!(
        message.contains("fork.web_implementation"),
        "unexpected message: {message}"
    );
}

#[test]
fn web_implementation_telemt_refuses_the_forks_own_transport() {
    let message = load_err(&base_config(
        r#"
[fork]
web_implementation = "telemt"

[fork.web]
enabled = true
hostname = "proxy.example.com"
public_dir = "/var/empty/telemt-web-site"
derive_user_profiles = true
"#,
    ));

    assert!(
        message.contains("fork.web_implementation = \"telemt\""),
        "unexpected message: {message}"
    );
}

#[test]
fn web_implementation_off_keeps_the_forks_transport_down() {
    let message = load_err(&base_config(
        r#"
[fork]
web_implementation = "off"

[fork.web]
enabled = true
hostname = "proxy.example.com"
public_dir = "/var/empty/telemt-web-site"
derive_user_profiles = true
"#,
    ));

    assert!(
        message.contains("fork.web_implementation = \"off\""),
        "unexpected message: {message}"
    );
}

#[test]
fn an_enabled_telegram_bot_needs_a_token_and_an_admin() {
    let message = load_err(&base_config(
        r#"
[fork.telegram]
enabled = true
"#,
    ));

    assert!(
        message.contains("fork.telegram.token"),
        "unexpected message: {message}"
    );

    let message = load_err(&base_config(
        r#"
[fork.telegram]
enabled = true
token = "123456:AAHnotarealtokenatall"
"#,
    ));

    assert!(
        message.contains("fork.telegram.admins"),
        "unexpected message: {message}"
    );
}

#[test]
fn a_disabled_telegram_bot_is_not_validated() {
    // A half-written section an operator has switched off must not stop the
    // proxy from starting.
    let config = load_ok(&base_config(
        r#"
[fork.telegram]
enabled = false
token = "nonsense"
"#,
    ));

    assert!(!config.fork.telegram_enabled());
}

#[test]
fn the_prometheus_panel_refuses_to_shadow_the_metrics_endpoint() {
    let message = load_err(&base_config(
        r#"
[fork.prometheus]
enabled = true
path = "/metrics"
"#,
    ));

    assert!(
        message.contains("must not shadow"),
        "unexpected message: {message}"
    );
}

#[test]
fn a_bulk_timeout_above_the_api_connection_deadline_is_refused() {
    let message = load_err(&base_config(
        r#"
[fork.api]
bulk_enabled = true
bulk_timeout_secs = 30
"#,
    ));

    assert!(
        message.contains("fork.api.bulk_timeout_secs"),
        "unexpected message: {message}"
    );
}
