//! WEB proxy transport: an MTProxy carrier that looks like an ordinary HTTPS site.
//!
//! A WEB-capable Telegram app keeps its normal MTProxy framing and encryption
//! but sends every proxy connection through one app-owned WebView transport.
//! The WebView runs a same-origin HTTPS or WebSocket carrier, and this relay
//! separates the multiplexed logical streams again. Unlike the reference
//! implementation, which forwards each stream to a stock MTProxy over
//! loopback TCP, telemt terminates the stream in-process: the demultiplexed
//! bytes enter the same client pipeline a direct TCP client would use.
//!
//! Submodules:
//! - `capability`: bridge capability derivation and token primitives
//! - `frame`: shared frame codec
//! - `bridge`: one-shot bridge page rendering
//! - `session`: relay sessions, carrier queues, and logical streams
//! - `manager`: bootstrap and session registry with capacity control
//! - `http`: the public HTTP surface
//! - `websocket`: RFC 6455 framing for the WebSocket carriers
//! - `site`: the operator's in-memory static site
//! - `upstream`: reverse proxy to the operator's private site application
//! - `admin`: loopback health and metrics endpoints
//! - `listener`: listener binding and accept loops
//! - `runtime`: attachment of demultiplexed streams to the telemt runtime

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::{ProxyConfig, WebBackend, WebConfig, WebProfileConfig, WebProfileLimits};
use crate::crypto::SecureRandom;
use crate::error::{ProxyError, Result};
use crate::maestro::generation::RuntimeGeneration;

pub(crate) mod admin;
pub(crate) mod bridge;
pub(crate) mod capability;
pub(crate) mod error;
pub(crate) mod frame;
pub(crate) mod http;
pub(crate) mod listener;
pub(crate) mod manager;
pub(crate) mod metrics;
pub(crate) mod runtime;
pub(crate) mod session;
pub(crate) mod site;
#[cfg(test)]
mod tests;
pub(crate) mod upstream;
pub(crate) mod websocket;

use http::Relay;
use manager::{Manager, WebProfile};
use runtime::WebRuntime;
use site::StaticSite;
use upstream::UpstreamProxy;

/// Everything the process shutdown sequence needs to stop the relay.
struct ActiveRelay {
    manager: Arc<Manager>,
    /// Stops both accept loops before the sessions are closed.
    shutdown: CancellationToken,
}

/// The running relay, published only once its listener is actually bound.
static ACTIVE_RELAY: Mutex<Option<ActiveRelay>> = Mutex::new(None);

/// Stops the relay listeners and closes every relay session.
///
/// The listeners are cancelled first, for the same reason the proxy unbinds its
/// own sockets first: an accept loop that keeps running through the drain window
/// creates sessions the drain has already passed.
pub(crate) fn shutdown() {
    let active = ACTIVE_RELAY.lock().take();
    if let Some(active) = active {
        active.shutdown.cancel();
        active.manager.shutdown();
        metrics::clear_metrics_source();
    }
}

/// Starts the WEB relay when it is enabled, leaving the proxy running if the
/// relay cannot bind.
pub(crate) async fn start(
    config: &ProxyConfig,
    config_dir: Option<&PathBuf>,
    active_runtime: Arc<ArcSwap<RuntimeGeneration>>,
) {
    let web = &config.web;
    if !web.enabled {
        return;
    }
    match build(config, config_dir, active_runtime).await {
        Ok(()) => {}
        Err(error) => warn!(error = %error, "WEB proxy disabled: startup failed"),
    }
}

async fn build(
    config: &ProxyConfig,
    config_dir: Option<&PathBuf>,
    active_runtime: Arc<ArcSwap<RuntimeGeneration>>,
) -> Result<()> {
    let web = config.web.clone();
    web.validate()?;
    let profiles = build_profiles(&web, &config.access.users)?;
    let carrier_address: SocketAddr = web
        .listen
        .parse()
        .map_err(|_| ProxyError::Config("web.listen must be ip:port".to_string()))?;

    let site = match web.public_dir.as_deref().filter(|value| !value.is_empty()) {
        Some(directory) => {
            let path = resolve_path(directory, config_dir);
            Some(StaticSite::load(&path)?)
        }
        None => None,
    };
    let relay_timeouts = web.timeouts.clone();
    let upstream = match web
        .public_upstream
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        Some(value) => {
            let address: SocketAddr = value
                .trim_start_matches("http://")
                .parse()
                .map_err(|_| ProxyError::Config("web.public_upstream must be ip:port".into()))?;
            Some(UpstreamProxy::new(
                address,
                Duration::from_millis(relay_timeouts.body_read_ms),
            ))
        }
        None => None,
    };

    warn_on_public_listener(carrier_address);

    let runtime = WebRuntime::new(active_runtime);
    let manager = Manager::new(
        web.limits.clone(),
        web.timeouts.clone(),
        profiles,
        runtime.clone(),
    )?;

    let relay = Arc::new(Relay {
        hostname: web.hostname.clone(),
        manager: manager.clone(),
        limits: web.limits.clone(),
        timeouts: web.timeouts.clone(),
        site,
        upstream,
        trusted_proxies: web.trusted_proxies.clone(),
        rng: Arc::new(SecureRandom::new()),
    });

    // Nothing is published before the carrier listener is bound: a manager and
    // a metrics source registered by a start-up that then fails would leave the
    // process reporting a relay it does not run.
    let Some(carrier_listener) = listener::bind(carrier_address, "carrier").await else {
        return Err(ProxyError::Config(format!(
            "web.listen {carrier_address} could not be bound"
        )));
    };

    let shutdown = CancellationToken::new();
    metrics::register_metrics_source(manager.clone());
    *ACTIVE_RELAY.lock() = Some(ActiveRelay {
        manager: manager.clone(),
        shutdown: shutdown.clone(),
    });

    tokio::spawn(listener::serve_carrier(
        carrier_listener,
        relay,
        shutdown.clone(),
    ));

    if !web.admin_listen.is_empty()
        && let Ok(admin_address) = web.admin_listen.parse::<SocketAddr>()
        && let Some(admin_listener) = listener::bind(admin_address, "admin").await
    {
        tokio::spawn(listener::serve_admin(
            admin_listener,
            manager.clone(),
            runtime.clone(),
            shutdown.clone(),
        ));
    }

    tokio::spawn(manager.clone().run_cleanup());
    tokio::spawn(watch_profiles(
        manager.clone(),
        runtime.clone(),
        web.clone(),
        profile_fingerprint(&web, &config.access.users),
    ));
    log_client_secret_forms(config);
    info!(
        hostname = %web.hostname,
        carrier = %web.listen,
        profiles = manager.profiles().profiles.len(),
        default_mode = web.carrier_mode.as_str(),
        "WEB proxy transport enabled"
    );
    Ok(())
}

/// Interval at which the relay re-derives capabilities after a config reload.
const PROFILE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Rebuilds the capability set whenever the reloaded configuration changes it.
///
/// Telemt reloads `[access.users]` at runtime, so a user added after start-up
/// must gain WEB access without restarting the process. The listener address
/// and the public site are fixed for the process lifetime.
async fn watch_profiles(
    manager: Arc<Manager>,
    runtime: Arc<WebRuntime>,
    initial_web: WebConfig,
    initial_fingerprint: u64,
) {
    let mut fingerprint = initial_fingerprint;
    let mut warned_about_limits = false;
    let mut ticker = tokio::time::interval(PROFILE_REFRESH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let config = runtime.config();
        // A reloaded configuration that disables the relay or changes its
        // hostname cannot be applied in place; keep serving the current set.
        let reloaded = if config.web.enabled && config.web.hostname == initial_web.hostname {
            &config.web
        } else {
            &initial_web
        };
        // Per-profile ceilings are always resolved against the ceilings that
        // are actually in force. The process-wide pools, the session budget
        // partitions, and every timeout are built once at start-up, so taking
        // per-profile values from reloaded ceilings would apply half of an
        // operator's change and silently discard the other half.
        let mut effective = reloaded.clone();
        effective.limits = initial_web.limits.clone();
        effective.timeouts = initial_web.timeouts.clone();
        if !warned_about_limits
            && (reloaded.limits != initial_web.limits || reloaded.timeouts != initial_web.timeouts)
        {
            warned_about_limits = true;
            warn!(
                "WEB proxy kept the running web.limits and web.timeouts: they are fixed for the \
                 process lifetime. Restart telemt to apply the reloaded values."
            );
        }
        let web = &effective;
        let next = profile_fingerprint(web, &config.access.users);
        if next == fingerprint {
            continue;
        }
        match build_profiles(web, &config.access.users) {
            Ok(profiles) => {
                let count = profiles.len();
                match manager.replace_profiles(profiles) {
                    Ok(()) => {
                        fingerprint = next;
                        info!(profiles = count, "WEB proxy capability profiles reloaded");
                    }
                    Err(error) => {
                        warn!(error = %error, "WEB proxy kept the previous capability profiles")
                    }
                }
            }
            Err(error) => {
                warn!(error = %error, "WEB proxy kept the previous capability profiles")
            }
        }
    }
}

/// Fingerprints every input that changes the derived capability set.
fn profile_fingerprint(web: &WebConfig, users: &HashMap<String, String>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    web.hostname.hash(&mut hasher);
    web.carrier_mode.as_str().hash(&mut hasher);
    web.derive_user_profiles.hash(&mut hasher);
    let mut entries: Vec<(&String, &String)> = users.iter().collect();
    entries.sort();
    for (name, secret) in entries {
        name.hash(&mut hasher);
        secret.hash(&mut hasher);
    }
    for profile in &web.profiles {
        profile.name.hash(&mut hasher);
        profile.secret.hash(&mut hasher);
        profile.backend.hash(&mut hasher);
        web.profile_carrier_mode(profile).as_str().hash(&mut hasher);
        hash_profile_limits(&profile.limits, &mut hasher);
    }
    hasher.finish()
}

/// Folds the per-profile ceilings into the capability fingerprint.
///
/// They are rebuilt with the profile set, so a reload that only changes a
/// profile's ceilings has to be noticed here or it is never applied.
fn hash_profile_limits(limits: &WebProfileLimits, hasher: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    for value in [
        limits.max_sessions,
        limits.max_streams,
        limits.max_backend_dials_in_flight,
        limits.new_sessions_per_minute,
        limits.new_sessions_burst,
        limits.new_streams_per_minute,
        limits.new_streams_burst,
        limits.max_streams_per_session,
        limits.max_pending_per_session,
    ] {
        value.hash(hasher);
    }
}

/// Warns when the carrier listener is reachable from outside the host.
///
/// The carrier speaks plaintext HTTP/1.1 on purpose: TLS, ACME, and the
/// publicly trusted certificate belong to the front proxy. Binding it to a
/// public interface therefore puts the bridge capability and the session bearer
/// on the wire in the clear. It is not refused outright, because a container
/// deployment reaches the relay from a sibling container and must bind
/// `0.0.0.0` to do so — but that is a decision an operator has to make on
/// purpose, not one they discover from a cheerful "listener bound" line.
fn warn_on_public_listener(address: SocketAddr) {
    if address.ip().is_loopback() {
        return;
    }
    warn!(
        %address,
        "web.listen is not a loopback address. The carrier is plaintext HTTP: anything that can \
         reach this address reads the bridge capability and the session bearer. Bind it to \
         127.0.0.1 unless the front proxy is on another host inside a private network."
    );
}

/// Builds the profile set from explicit entries and, optionally, from users.
fn build_profiles(
    web: &WebConfig,
    users: &HashMap<String, String>,
) -> Result<Vec<Arc<WebProfile>>> {
    let mut profiles = Vec::with_capacity(web.profiles.len() + users.len());
    let mut names = std::collections::HashSet::new();
    for entry in &web.profiles {
        let profile = explicit_profile(web, entry)?;
        names.insert(profile.name.clone());
        profiles.push(Arc::new(profile));
    }
    if web.derive_user_profiles {
        for (user, secret) in users {
            if names.contains(user) {
                // An explicit profile with the same name wins.
                continue;
            }
            let decoded = hex::decode(secret).map_err(|_| ProxyError::InvalidSecret {
                user: user.clone(),
                reason: "Must be 32 hex characters".to_string(),
            })?;
            if decoded.len() != 16 {
                return Err(ProxyError::InvalidSecret {
                    user: user.clone(),
                    reason: "Must be 32 hex characters".to_string(),
                });
            }
            profiles.push(Arc::new(WebProfile {
                name: user.clone(),
                backend: WebBackend::Internal,
                carrier: web.carrier_mode,
                capabilities: capabilities_for(&web.hostname, &decoded),
                limits: WebProfileLimits::default().with_defaults(&web.limits),
            }));
        }
    }
    if profiles.is_empty() {
        return Err(ProxyError::Config(
            "web is enabled but no profile could be built".to_string(),
        ));
    }
    Ok(profiles)
}

fn explicit_profile(web: &WebConfig, entry: &WebProfileConfig) -> Result<WebProfile> {
    let secret = capability::decode_secret(&entry.secret)
        .map_err(|reason| ProxyError::Config(format!("web profile '{}': {reason}", entry.name)))?;
    Ok(WebProfile {
        name: entry.name.clone(),
        backend: WebBackend::parse(&entry.backend)?,
        carrier: web.profile_carrier_mode(entry),
        capabilities: capabilities_for(&web.hostname, &secret),
        limits: entry.limits.with_defaults(&web.limits),
    })
}

/// Derives every capability a client may present for one secret.
///
/// A WEB client accepts a plain 16-byte or `dd` random-padding secret and
/// rejects `ee` fake-TLS secrets outright, so those two forms are the only
/// ones whose capability can ever be presented.
fn capabilities_for(hostname: &str, secret: &[u8]) -> Vec<[u8; 32]> {
    let mut result = vec![capability::derive_capability(hostname, secret)];
    if secret.len() == capability::SECRET_BYTES {
        let mut padded = Vec::with_capacity(1 + secret.len());
        padded.push(0xdd);
        padded.extend_from_slice(secret);
        let derived = capability::derive_capability(hostname, &padded);
        if !result.contains(&derived) {
            result.push(derived);
        }
    }
    result
}

/// Reports which secret form WEB clients must be given, or that none works.
///
/// A capability derived from a plain or `dd` secret reaches the bridge, but the
/// stream it opens then speaks the matching MTProto transform. If that mode is
/// disabled the handshake is refused and masked, which looks exactly like a
/// working carrier that passes no data — so it is called out loudly.
///
/// `general.modes.tls` is irrelevant here: a WEB client rejects `ee` secrets,
/// so it never offers the fake-TLS transform over the carrier.
fn log_client_secret_forms(config: &ProxyConfig) {
    let modes = &config.general.modes;
    let mut accepted: Vec<&str> = Vec::with_capacity(2);
    if modes.classic {
        accepted.push("plain 32-hex");
    }
    if modes.secure {
        accepted.push("dd-prefixed");
    }
    if accepted.is_empty() {
        warn!(
            "WEB proxy: no client can complete a handshake. A WEB client may only use a plain \
             or dd-prefixed secret, and both general.modes.classic and general.modes.secure are \
             disabled. Enable one of them — secure, paired with the dd… secret, keeps the padded \
             transform."
        );
        return;
    }
    info!(
        forms = accepted.join(" or "),
        "WEB proxy client secret forms accepted by the proxy handshake"
    );
}

fn resolve_path(value: &str, config_dir: Option<&PathBuf>) -> PathBuf {
    let path = PathBuf::from(value);
    match config_dir {
        Some(directory) if path.is_relative() => directory.join(path),
        _ => path,
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    use crate::config::CarrierMode;

    fn base_config() -> WebConfig {
        WebConfig {
            enabled: true,
            hostname: "proxy.example.com".to_string(),
            public_dir: Some("site".to_string()),
            ..WebConfig::default()
        }
    }

    #[test]
    fn derives_one_profile_per_user_with_all_secret_forms() {
        let web = base_config();
        let mut users = HashMap::new();
        users.insert(
            "alice".to_string(),
            "000102030405060708090a0b0c0d0e0f".to_string(),
        );
        let profiles = build_profiles(&web, &users).expect("profiles");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "alice");
        // Plain and dd only: an `ee` capability could never be presented.
        assert_eq!(profiles[0].capabilities.len(), 2);
        assert_eq!(profiles[0].backend, WebBackend::Internal);
        let expected = capability::encode_token(&profiles[0].capabilities[0]);
        assert_eq!(expected, "MHLEY5PmW1GWqJkSrlmJpvJUiLhBH_QKy6yKg8a0JPk");
    }

    #[test]
    fn explicit_profile_overrides_a_user_of_the_same_name() {
        let mut web = base_config();
        web.profiles.push(WebProfileConfig {
            name: "alice".to_string(),
            secret: "0f0e0d0c0b0a09080706050403020100".to_string(),
            backend: "127.0.0.1:2398".to_string(),
            carrier_mode: Some(CarrierMode::WebsocketLanes),
            limits: WebProfileLimits::default(),
        });
        let mut users = HashMap::new();
        users.insert(
            "alice".to_string(),
            "000102030405060708090a0b0c0d0e0f".to_string(),
        );
        let profiles = build_profiles(&web, &users).expect("profiles");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].carrier, CarrierMode::WebsocketLanes);
        assert!(matches!(profiles[0].backend, WebBackend::Loopback(_)));
    }

    #[test]
    fn user_profiles_can_be_disabled() {
        let mut web = base_config();
        web.derive_user_profiles = false;
        web.profiles.push(WebProfileConfig {
            name: "only".to_string(),
            secret: "0f0e0d0c0b0a09080706050403020100".to_string(),
            backend: "internal".to_string(),
            carrier_mode: None,
            limits: WebProfileLimits::default(),
        });
        let mut users = HashMap::new();
        users.insert(
            "bob".to_string(),
            "00000000000000000000000000000000".to_string(),
        );
        let profiles = build_profiles(&web, &users).expect("profiles");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "only");
    }
}
