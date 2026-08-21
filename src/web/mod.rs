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

/// The running relay manager, used by the process shutdown sequence.
static ACTIVE_MANAGER: Mutex<Option<Arc<Manager>>> = Mutex::new(None);

/// Closes every relay session during process shutdown.
pub(crate) fn shutdown() {
    let manager = ACTIVE_MANAGER.lock().take();
    if let Some(manager) = manager {
        manager.shutdown();
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
            Some(UpstreamProxy::new(address))
        }
        None => None,
    };

    let runtime = WebRuntime::new(active_runtime);
    let manager = Manager::new(
        web.limits.clone(),
        web.timeouts.clone(),
        profiles,
        runtime.clone(),
    )?;
    metrics::register_metrics_source(manager.clone());
    *ACTIVE_MANAGER.lock() = Some(manager.clone());

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

    let Some(carrier_listener) = listener::bind(carrier_address, "carrier").await else {
        return Err(ProxyError::Config(format!(
            "web.listen {carrier_address} could not be bound"
        )));
    };
    tokio::spawn(listener::serve_carrier(carrier_listener, relay));

    if !web.admin_listen.is_empty()
        && let Ok(admin_address) = web.admin_listen.parse::<SocketAddr>()
        && let Some(admin_listener) = listener::bind(admin_address, "admin").await
    {
        tokio::spawn(listener::serve_admin(
            admin_listener,
            manager.clone(),
            runtime.clone(),
        ));
    }

    tokio::spawn(manager.clone().run_cleanup());
    tokio::spawn(watch_profiles(
        manager.clone(),
        runtime.clone(),
        web.clone(),
        profile_fingerprint(&web, &config.access.users),
    ));
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
    let mut ticker = tokio::time::interval(PROFILE_REFRESH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let config = runtime.config();
        // A reloaded configuration that disables the relay or changes its
        // hostname cannot be applied in place; keep serving the current set.
        let web = if config.web.enabled && config.web.hostname == initial_web.hostname {
            &config.web
        } else {
            &initial_web
        };
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
    }
    hasher.finish()
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
/// A client derives the capability from the secret it was configured with, so
/// a bare 16-byte secret also has to match its `dd` and `ee` prefixed forms.
fn capabilities_for(hostname: &str, secret: &[u8]) -> Vec<[u8; 32]> {
    let mut result = vec![capability::derive_capability(hostname, secret)];
    if secret.len() == 16 {
        for prefix in [0xddu8, 0xee] {
            let mut prefixed = Vec::with_capacity(17);
            prefixed.push(prefix);
            prefixed.extend_from_slice(secret);
            result.push(capability::derive_capability(hostname, &prefixed));
        }
    }
    result
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
        assert_eq!(profiles[0].capabilities.len(), 3);
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
