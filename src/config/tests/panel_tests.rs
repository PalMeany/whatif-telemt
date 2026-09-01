//! `[panel]` configuration loaded through the production `ProxyConfig::load`
//! path.
//!
//! The struct-level tests next to `PanelConfig` construct the struct directly
//! and therefore never exercise serde's per-field defaults, the strict
//! unknown-key scan, or the TOML spelling of a cluster role — all three of which
//! are what an operator actually types.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{ClusterRole, ProxyConfig};

fn write_temp_config(contents: &str) -> PathBuf {
    // The counter is not decoration: these tests run in parallel and a
    // nanosecond clock is not guaranteed to differ between two of them.
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time must be after unix epoch")
        .as_nanos();
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("telemt-load-panel-{nonce}-{sequence}.toml"));
    fs::write(&path, contents).expect("temp config write must succeed");
    path
}

fn remove_temp_config(path: &PathBuf) {
    let _ = fs::remove_file(path);
}

/// A configuration with a Control API and whatever `[panel]` block is appended.
fn panel_config(panel: &str) -> String {
    format!(
        r#"
[access.users]
alice = "000102030405060708090a0b0c0d0e0f"

[server.api]
enabled = true
listen = "127.0.0.1:9091"

{panel}
"#
    )
}

#[test]
fn a_minimal_panel_block_loads_with_every_default() {
    let path = write_temp_config(&panel_config("[panel]\nenabled = true\n"));
    let config = ProxyConfig::load(path.to_str().expect("path")).expect("load");
    remove_temp_config(&path);

    assert!(config.panel.enabled);
    assert_eq!(config.panel.listen, "127.0.0.1:8443");
    assert_eq!(config.panel.session_ttl_secs, 43_200);
    assert_eq!(config.panel.password_hash_iterations, 600_000);
    assert!(config.panel.audit_enabled);
    assert!(!config.panel.cluster.enabled);
    assert_eq!(config.panel.cluster.role, ClusterRole::Standalone);
    assert!(!config.panel.tls.enabled);
}

#[test]
fn the_panel_is_off_by_default() {
    let path = write_temp_config(&panel_config(""));
    let config = ProxyConfig::load(path.to_str().expect("path")).expect("load");
    remove_temp_config(&path);
    assert!(!config.panel.enabled);
}

#[test]
fn every_cluster_role_spelling_round_trips_through_toml() {
    for (spelling, expected) in [
        ("master", ClusterRole::Master),
        ("agent", ClusterRole::Agent),
        ("master-agent", ClusterRole::MasterAgent),
    ] {
        let path = write_temp_config(&panel_config(&format!(
            "[panel]\nenabled = true\n\n[panel.cluster]\nenabled = true\nrole = \"{spelling}\"\n"
        )));
        let config = ProxyConfig::load(path.to_str().expect("path")).expect(spelling);
        remove_temp_config(&path);
        assert_eq!(config.panel.cluster.role, expected, "{spelling}");
    }
}

#[test]
fn the_panel_requires_the_control_api_it_drives() {
    let contents = r#"
[access.users]
alice = "000102030405060708090a0b0c0d0e0f"

[server.api]
enabled = false

[panel]
enabled = true
"#;
    let path = write_temp_config(contents);
    let error = ProxyConfig::load(path.to_str().expect("path")).expect_err("api disabled");
    remove_temp_config(&path);
    assert!(error.to_string().contains("server.api.enabled"), "{error}");
}

#[test]
fn a_routable_listener_without_tls_or_a_front_proxy_is_refused() {
    let path = write_temp_config(&panel_config(
        "[panel]\nenabled = true\nlisten = \"0.0.0.0:8443\"\n",
    ));
    let error = ProxyConfig::load(path.to_str().expect("path")).expect_err("plaintext off host");
    remove_temp_config(&path);
    assert!(
        error.to_string().contains("panel.trusted_proxies"),
        "{error}"
    );
}

#[test]
fn an_out_of_range_bound_is_refused_by_the_loader() {
    let path = write_temp_config(&panel_config(
        "[panel]\nenabled = true\npassword_hash_iterations = 10\n",
    ));
    let error = ProxyConfig::load(path.to_str().expect("path")).expect_err("weak work factor");
    remove_temp_config(&path);
    assert!(
        error.to_string().contains("password_hash_iterations"),
        "{error}"
    );
}

#[test]
fn strict_config_accepts_every_documented_panel_key() {
    // The strict scan is a hand-maintained key list. A field added to
    // `PanelConfig` without a matching entry there turns a valid configuration
    // into a start-up failure for anyone running `config_strict`.
    let contents = r#"
[general]
config_strict = true

[access.users]
alice = "000102030405060708090a0b0c0d0e0f"

[server.api]
enabled = true
listen = "127.0.0.1:9091"

[panel]
enabled = true
listen = "127.0.0.1:8443"
data_dir = "/var/lib/telemt/panel"
whitelist = ["10.0.0.0/8"]
trusted_proxies = ["127.0.0.0/8"]
control_api_url = "http://127.0.0.1:9091"
control_api_token = "Bearer token"
session_ttl_secs = 3600
session_idle_timeout_secs = 900
max_sessions_per_operator = 4
max_sessions_total = 64
login_max_attempts = 4
login_lockout_secs = 300
password_min_length = 16
password_hash_iterations = 200000
require_totp = true
request_body_limit_bytes = 131072
max_connections = 64
header_read_timeout_ms = 5000
request_timeout_ms = 20000
audit_enabled = true
audit_retention_days = 30
audit_max_bytes = 1048576

[panel.tls]
enabled = false
cert_path = ""
key_path = ""

[panel.cluster]
enabled = true
role = "master-agent"
node_name = "edge-1"
advertise_url = "https://edge-1.example.com:8443"
allow_from = ["203.0.113.10/32"]
request_timeout_ms = 8000
clock_skew_secs = 30
nonce_capacity = 1024
poll_interval_secs = 60
"#;
    let path = write_temp_config(contents);
    let config = ProxyConfig::load(path.to_str().expect("path")).expect("strict load");
    remove_temp_config(&path);

    assert!(config.panel.require_totp);
    assert_eq!(config.panel.cluster.role, ClusterRole::MasterAgent);
    assert_eq!(config.panel.cluster.node_name, "edge-1");
    assert_eq!(config.panel.max_sessions_per_operator, 4);
}

#[test]
fn an_unknown_panel_key_is_refused_under_strict_config() {
    let contents = r#"
[general]
config_strict = true

[access.users]
alice = "000102030405060708090a0b0c0d0e0f"

[server.api]
enabled = true
listen = "127.0.0.1:9091"

[panel]
enabled = true
panel_theme = "neon"
"#;
    let path = write_temp_config(contents);
    let error = ProxyConfig::load(path.to_str().expect("path")).expect_err("unknown key");
    remove_temp_config(&path);
    assert!(error.to_string().contains("panel.panel_theme"), "{error}");
}

#[test]
fn every_shipped_panel_example_loads() {
    // The templates in contrib/panel are what an operator copies. A key
    // renamed here without updating them turns "copy this file" into a process
    // that refuses to start, and nothing else in the tree would catch it.
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contrib/panel");
    let mut checked = 0;
    for entry in std::fs::read_dir(&directory).expect("contrib/panel must exist") {
        let path = entry.expect("directory entry").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
            continue;
        }
        let config = ProxyConfig::load(path.to_str().expect("path"))
            .unwrap_or_else(|error| panic!("{} does not load: {error}", path.display()));
        assert!(
            config.panel.enabled,
            "{} is a panel template and should enable the panel",
            path.display()
        );
        assert!(
            config.server.api.enabled,
            "{} must keep the Control API the panel drives",
            path.display()
        );
        checked += 1;
    }
    assert_eq!(
        checked, 3,
        "expected standalone, master and agent templates"
    );
}

#[test]
fn the_shipped_examples_agree_with_their_documented_roles() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contrib/panel");
    let load = |name: &str| {
        ProxyConfig::load(directory.join(name).to_str().expect("path"))
            .unwrap_or_else(|error| panic!("{name} does not load: {error}"))
    };

    let standalone = load("standalone.toml");
    assert!(!standalone.panel.cluster.enabled);

    let master = load("master.toml");
    assert!(master.panel.cluster.role.is_master());
    assert!(!master.panel.cluster.role.is_agent());
    assert!(
        master.panel.require_totp,
        "a control node reaches every node"
    );

    let agent = load("agent.toml");
    assert!(agent.panel.cluster.role.is_agent());
    // An agent has to advertise a URL or it cannot produce a link token, and
    // it has to terminate TLS or the master would pin nothing.
    assert!(!agent.panel.cluster.advertise_url.is_empty());
    assert!(agent.panel.tls.enabled);
    assert!(
        !agent.panel.cluster.allow_from.is_empty(),
        "the template should show the endpoint being restricted"
    );
}
