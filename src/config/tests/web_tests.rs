//! `[web]` configuration loaded through the production `ProxyConfig::load`
//! path.
//!
//! The struct-level tests next to `WebConfig` construct the struct directly and
//! therefore never exercise serde's per-field defaults, the strict unknown-key
//! scan, or the TOML spelling of a carrier mode — all three of which are what an
//! operator actually types. These tests go through the loader for that reason.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{CarrierMode, ProxyConfig};

fn write_temp_config(contents: &str) -> PathBuf {
    // The counter is not decoration: these tests run in parallel and a
    // nanosecond clock is not guaranteed to differ between two of them, so a
    // timestamp alone lets one test load another's configuration and fail with
    // an error message from a file it never wrote.
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time must be after unix epoch")
        .as_nanos();
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("telemt-load-web-{nonce}-{sequence}.toml"));
    fs::write(&path, contents).expect("temp config write must succeed");
    path
}

fn remove_temp_config(path: &PathBuf) {
    let _ = fs::remove_file(path);
}

/// A `[web]` block that is valid on its own, plus whatever the caller appends.
///
/// `derive_user_profiles` is spelled out rather than left to the default so
/// that every test below fails for the reason it names, not because the profile
/// source default moved underneath it.
fn web_config(extra: &str) -> String {
    format!(
        r#"
[access.users]
alice = "000102030405060708090a0b0c0d0e0f"

[web]
enabled = true
hostname = "proxy.example.com"
public_dir = "/var/empty/telemt-web-site"
derive_user_profiles = true
{extra}
"#
    )
}

/// A single explicit profile whose `[web.profiles.limits]` table is supplied by
/// the caller, so a per-profile ceiling can be pushed past a global one.
fn profile_limits_config(limits: &str) -> String {
    web_config(&format!(
        r#"
[[web.profiles]]
name = "vip"
secret = "000102030405060708090a0b0c0d0e0f"

[web.profiles.limits]
{limits}
"#
    ))
}

fn load_ok(toml: &str) -> ProxyConfig {
    let path = write_temp_config(toml);
    let config = ProxyConfig::load(&path).expect("configuration must load");
    remove_temp_config(&path);
    config
}

/// Loads a configuration that must be refused and returns the validator's own
/// message.
///
/// The `Config error: ` prefix belongs to `ProxyError`'s `Display`, not to the
/// WEB validation being tested, so it is stripped here: these assertions are
/// about the sentence an operator has to act on.
fn load_err(toml: &str) -> String {
    let path = write_temp_config(toml);
    let outcome = ProxyConfig::load(&path);
    remove_temp_config(&path);
    let message = match outcome {
        Ok(config) => panic!(
            "configuration must be refused, but it loaded with web = {:?}",
            config.web
        ),
        Err(error) => error.to_string(),
    };
    message
        .strip_prefix("Config error: ")
        .unwrap_or(&message)
        .to_string()
}

#[test]
fn minimal_web_block_loads_with_the_documented_defaults() {
    let config = load_ok(&web_config(""));

    // Every one of these is a value an operator inherits by typing nothing, so
    // a silent change here changes a running deployment's resource envelope
    // without any config diff to review.
    assert!(config.web.enabled, "the [web] block sets enabled = true");
    assert_eq!(config.web.listen, "127.0.0.1:8080");
    assert_eq!(config.web.admin_listen, "127.0.0.1:8081");
    assert_eq!(config.web.carrier_mode, CarrierMode::Https);
    assert_eq!(
        config
            .web
            .trusted_proxies
            .iter()
            .map(|network| network.to_string())
            .collect::<Vec<_>>(),
        vec!["127.0.0.0/8".to_string(), "::1/128".to_string()],
    );

    let limits = &config.web.limits;
    assert_eq!(limits.max_header_bytes, 16 * 1024);
    assert_eq!(limits.max_body_bytes, 2 * 1024 * 1024);
    assert_eq!(limits.max_frame_payload, 1024 * 1024);
    assert_eq!(limits.carrier_batch_bytes, 2 * 1024 * 1024);
    assert_eq!(limits.max_streams_per_session, 128);
    assert_eq!(limits.max_closed_stream_ids, 4096);
    assert_eq!(limits.max_pending_per_session, 32 * 1024 * 1024);
    assert_eq!(limits.max_pending_global, 512 * 1024 * 1024);
    assert_eq!(limits.max_pending_items_per_session, 16 * 1024);
    assert_eq!(limits.max_pending_items_global, 256 * 1024);
    assert_eq!(limits.max_sessions_global, 128);
    assert_eq!(limits.max_streams_global, 4096);
    assert_eq!(limits.max_backend_dials_in_flight, 256);
    // Derived from the stream ceiling plus headroom, not a fixed number: a
    // lanes carrier parks one long poll per live stream on its own connection.
    assert_eq!(limits.max_carrier_connections, 4096 + 1024);
    assert_eq!(limits.new_sessions_per_minute, 600);
    assert_eq!(limits.new_sessions_burst, 128);
    assert_eq!(limits.new_streams_per_minute, 6000);
    assert_eq!(limits.new_streams_burst, 512);
    assert_eq!(limits.max_bootstraps_global, 512);
    assert_eq!(limits.new_bootstraps_per_minute, 1200);
    assert_eq!(limits.new_bootstraps_burst, 256);
    assert_eq!(limits.max_profiles, 32);
    // Both per-address ceilings ship disabled: they count live sessions, and
    // behind a carrier-grade NAT one address is thousands of subscribers.
    assert_eq!(limits.max_sessions_per_ip, 0);
    assert_eq!(limits.max_bootstraps_per_ip, 0);

    let timeouts = &config.web.timeouts;
    assert_eq!(timeouts.backend_dial_ms, 5_000);
    assert_eq!(timeouts.long_poll_ms, 25_000);
    assert_eq!(timeouts.reconnect_grace_ms, 120_000);
    assert_eq!(timeouts.bootstrap_lifetime_ms, 120_000);
    assert_eq!(timeouts.read_header_ms, 10_000);
    assert_eq!(timeouts.body_read_ms, 30_000);
    assert_eq!(timeouts.idle_ms, 75_000);
}

#[test]
fn every_profile_ceiling_above_its_global_names_the_field_it_refused() {
    // A per-profile override above the process-wide ceiling is a promise the
    // process cannot keep: the profile would admit work the global pools then
    // refuse, so the operator has to be told which of the nine knobs is the
    // one that overshot rather than that "the web config is invalid".
    for (field, value, expected) in [
        (
            "max_sessions",
            129usize,
            "web profile 'vip': max_sessions must be between 0 and 128",
        ),
        (
            "max_streams",
            4097,
            "web profile 'vip': max_streams must be between 0 and 4096",
        ),
        (
            "max_backend_dials_in_flight",
            257,
            "web profile 'vip': max_backend_dials_in_flight must be between 0 and 256",
        ),
        (
            "new_sessions_per_minute",
            601,
            "web profile 'vip': new_sessions_per_minute must be between 0 and 600",
        ),
        (
            "new_sessions_burst",
            129,
            "web profile 'vip': new_sessions_burst must be between 0 and 128",
        ),
        (
            "new_streams_per_minute",
            6001,
            "web profile 'vip': new_streams_per_minute must be between 0 and 6000",
        ),
        (
            "new_streams_burst",
            513,
            "web profile 'vip': new_streams_burst must be between 0 and 512",
        ),
        (
            "max_streams_per_session",
            129,
            "web profile 'vip': max_streams_per_session must be between 0 and 128",
        ),
        (
            "max_pending_per_session",
            33_554_433,
            "web profile 'vip': max_pending_per_session must be between 0 and 33554432",
        ),
    ] {
        let message = load_err(&profile_limits_config(&format!("{field} = {value}")));
        assert_eq!(
            message, expected,
            "profile ceiling {field} must be refused by name"
        );
    }
}

#[test]
fn profile_streams_per_session_above_its_own_resolved_stream_ceiling_is_refused() {
    // Both values are under their global ceilings, so only the cross-check
    // catches this: a session allowed 16 streams inside a profile allowed 4
    // would have the profile pool refuse streams the session ceiling grants.
    let message = load_err(&profile_limits_config(
        "max_streams = 4\nmax_streams_per_session = 16",
    ));
    assert_eq!(
        message,
        "web profile 'vip': max_streams_per_session must not exceed max_streams"
    );
}

#[test]
fn profile_backend_dials_above_its_own_resolved_stream_ceiling_is_refused() {
    // Same shape, different pair: 8 concurrent dials under a 4-stream profile
    // reserves dial capacity for streams that profile can never hold.
    let message = load_err(&profile_limits_config(
        "max_streams = 4\nmax_backend_dials_in_flight = 8",
    ));
    assert_eq!(
        message,
        "web profile 'vip': max_backend_dials_in_flight must not exceed max_streams"
    );
}

#[test]
fn carrier_connection_ceiling_below_the_global_stream_ceiling_is_refused() {
    // A lanes carrier parks one long poll per live stream on its own
    // connection, so a connection cap under the stream cap refuses streams the
    // stream ceilings explicitly allow — and does it as a 503 the operator
    // reads as overload rather than as misconfiguration.
    let message = load_err(&web_config(
        "\n[web.limits]\nmax_streams_global = 4096\nmax_carrier_connections = 1024",
    ));
    assert_eq!(
        message,
        "global web limits must not be smaller than per-session or per-IP limits"
    );
}

#[test]
fn carrier_connection_ceiling_equal_to_the_global_stream_ceiling_is_accepted() {
    // The boundary is inclusive: exactly one connection per stream is enough to
    // serve every stream the ceilings allow, so this must not be refused.
    let config = load_ok(&web_config(
        "\n[web.limits]\nmax_streams_global = 1024\nmax_carrier_connections = 1024",
    ));
    assert_eq!(config.web.limits.max_carrier_connections, 1024);
}

#[test]
fn non_loopback_listen_without_a_non_loopback_trusted_proxy_is_refused() {
    // A carrier reachable off-host with nothing but loopback trusted to forward
    // for it is a plaintext relay: its bridge capabilities and session bearers
    // are readable by anything that can route to it, and every request is
    // accounted to the front proxy's own address.
    let message = load_err(&web_config("listen = \"0.0.0.0:8080\""));
    assert_eq!(
        message,
        "web.listen is not a loopback address, so web.trusted_proxies must name the front proxy \
         that reaches it. Bind web.listen to 127.0.0.1, or add the front proxy's address or \
         network to web.trusted_proxies."
    );
}

#[test]
fn non_loopback_listen_loads_once_a_front_proxy_is_named() {
    // The refusal above is about the *combination*, not about binding off-host:
    // an operator who names the front proxy has a defensible deployment.
    let config = load_ok(&web_config(
        "listen = \"0.0.0.0:8080\"\ntrusted_proxies = [\"127.0.0.0/8\", \"10.0.0.0/8\"]",
    ));
    assert_eq!(config.web.listen, "0.0.0.0:8080");
    assert_eq!(config.web.trusted_proxies.len(), 2);
}

#[test]
fn admin_listen_must_be_a_loopback_address() {
    // `/metrics` on the admin listener exposes per-profile session and stream
    // counts, and `/healthz` confirms the relay exists at all; neither is
    // authenticated, so binding it off-host publishes both.
    let message = load_err(&web_config("admin_listen = \"0.0.0.0:8081\""));
    assert_eq!(message, "web.admin_listen must be a loopback address");
}

#[test]
fn admin_listen_must_differ_from_the_carrier_listen() {
    // Sharing one address would put the unauthenticated admin routes on the
    // public carrier listener, where the front proxy forwards to them.
    let message = load_err(&web_config("admin_listen = \"127.0.0.1:8080\""));
    assert_eq!(message, "web.listen and web.admin_listen must differ");
}

#[test]
fn an_empty_admin_listen_disables_the_admin_listener() {
    // The empty string is the documented off switch, so it must skip the
    // loopback and distinctness checks instead of failing to parse as ip:port.
    let config = load_ok(&web_config("admin_listen = \"\""));
    assert_eq!(config.web.admin_listen, "");
}

#[test]
fn exactly_one_public_site_source_is_required() {
    // Two sources means the relay silently picks one and the operator's other
    // site never serves; zero means `GET /` has nothing to answer with, and the
    // decoy site is the entire reason an observer sees an ordinary web server.
    let both = load_err(&web_config("public_upstream = \"http://127.0.0.1:3000\""));
    assert_eq!(
        both,
        "exactly one of web.public_dir or web.public_upstream is required"
    );

    let neither = load_err(
        r#"
[access.users]
alice = "000102030405060708090a0b0c0d0e0f"

[web]
enabled = true
hostname = "proxy.example.com"
derive_user_profiles = true
"#,
    );
    assert_eq!(
        neither,
        "exactly one of web.public_dir or web.public_upstream is required"
    );
}

#[test]
fn a_web_block_naming_no_profile_source_is_refused() {
    // Deriving a bridge capability for every `[access.users]` secret is opt-in:
    // enabling the WEB relay must not silently publish a carrier for every user
    // of an existing MTProto deployment. So a `[web]` block that names neither
    // an explicit profile nor `derive_user_profiles` has to be refused, and the
    // error has to name the key that would have turned derivation on.
    //
    // KNOWN FAILING against the code as it stands: `WebConfig::default()` sets
    // `derive_user_profiles: false`, but the field still carries
    // `#[serde(default = "default_true")]` (src/config/web/mod.rs), and serde
    // uses the *field* default for a key missing from a `[web]` table that is
    // present. So the opt-in above is only opt-in for a config with no `[web]`
    // block at all — exactly the case that cannot publish anything anyway.
    // Deleting `default = "default_true"` makes this pass.
    let message = load_err(
        r#"
[access.users]
alice = "000102030405060708090a0b0c0d0e0f"

[web]
enabled = true
hostname = "proxy.example.com"
public_dir = "/var/empty/telemt-web-site"
"#,
    );
    assert_eq!(
        message,
        "web requires at least one profile or web.derive_user_profiles=true"
    );
}

#[test]
fn every_documented_carrier_mode_parses_from_its_wire_name() {
    // The TOML spelling is the same string the bridge page and the
    // `X-Carrier-Mode` header carry, so a renamed variant silently repoints
    // every client at a carrier the server is not running.
    for (spelling, expected) in [
        ("https", CarrierMode::Https),
        ("https-lanes", CarrierMode::HttpsLanes),
        ("websocket", CarrierMode::Websocket),
        ("websocket-lanes", CarrierMode::WebsocketLanes),
    ] {
        let config = load_ok(&web_config(&format!("carrier_mode = \"{spelling}\"")));
        assert_eq!(
            config.web.carrier_mode, expected,
            "carrier_mode = \"{spelling}\" must select {expected:?}"
        );
        assert_eq!(config.web.carrier_mode.as_str(), spelling);
    }
}

#[test]
fn an_unknown_carrier_mode_is_refused_and_lists_the_supported_ones() {
    // Falling back to the default carrier on a typo would hand clients a bridge
    // page for a transport the operator did not choose.
    let message = load_err(&web_config("carrier_mode = \"quic\""));
    assert!(
        message.contains("unknown variant `quic`"),
        "error must name the rejected carrier mode, got: {message}"
    );
    for supported in ["https", "https-lanes", "websocket", "websocket-lanes"] {
        assert!(
            message.contains(supported),
            "error must list the supported mode `{supported}`, got: {message}"
        );
    }
}

#[test]
fn an_unknown_key_in_web_limits_is_refused_under_strict_config() {
    // A misspelled ceiling is worse than a rejected one: the relay would run
    // with the default the operator meant to raise, and nothing in the logs of
    // a healthy process would ever say so.
    let message = load_err(&web_config(
        "\n[general]\nconfig_strict = true\n\n[web.limits]\nmax_carrier_connection = 8192",
    ));
    assert_eq!(
        message,
        "unknown config keys are not allowed when general.config_strict=true: \
         web.limits.max_carrier_connection (did you mean `max_carrier_connections`?)"
    );
}
