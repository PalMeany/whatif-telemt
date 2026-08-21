//! Bootstrap and session registry with capacity, rate, and budget control.
//!
//! Submodules:
//! - `limits`: token buckets, the process-wide pending pool, stream permits
//! - `sessions`: bootstrap issuance, session creation, and stream accounting

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use subtle::ConstantTimeEq;

use crate::config::{CarrierMode, WebBackend, WebLimits, WebProfileLimits, WebTimeouts};
use crate::crypto::SecureRandom;
use crate::error::{ProxyError, Result};
use crate::web::capability::{TOKEN_BYTES, encode_token, token_hash};
use crate::web::metrics::{WebCapacity, WebMetrics, WebMetricsSnapshot, WebMetricsSource};
use crate::web::runtime::WebRuntime;
use crate::web::session::Session;

pub(crate) mod limits;
mod sessions;

pub(crate) use limits::StreamPermit;
use limits::{GlobalPending, RateState};

/// Interval of the bootstrap and idle-session reaper.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(30);

/// The profile set and its capability lookup, replaced atomically on reload.
pub(crate) struct ProfileSet {
    pub(crate) profiles: Vec<Arc<WebProfile>>,
    index: HashMap<[u8; 32], usize>,
}

impl ProfileSet {
    /// Builds the lookup index, rejecting a capability claimed twice.
    pub(crate) fn new(profiles: Vec<Arc<WebProfile>>) -> Result<Self> {
        let mut index = HashMap::with_capacity(profiles.len() * 3);
        for (position, profile) in profiles.iter().enumerate() {
            for capability in &profile.capabilities {
                if index.insert(*capability, position).is_some() {
                    return Err(ProxyError::Config(format!(
                        "duplicate web capability for profile '{}'",
                        profile.name
                    )));
                }
            }
        }
        Ok(Self { profiles, index })
    }
}

/// One resolved capability profile.
pub(crate) struct WebProfile {
    pub(crate) name: String,
    pub(crate) backend: WebBackend,
    pub(crate) carrier: CarrierMode,
    /// Every accepted encoding of the profile secret maps to this profile.
    pub(crate) capabilities: Vec<[u8; 32]>,
    pub(crate) limits: WebProfileLimits,
}

/// An issued, not yet consumed bridge bootstrap.
pub(super) struct Bootstrap {
    expires: Instant,
    profile: Arc<WebProfile>,
    issuance_ip: IpAddr,
    body_digest: [u8; 32],
    session_token: String,
    session: Option<Arc<Session>>,
    used: bool,
}

#[derive(Default)]
pub(super) struct ManagerState {
    bootstraps: HashMap<[u8; 32], Bootstrap>,
    bootstraps_per_ip: HashMap<IpAddr, usize>,
    sessions: HashMap<[u8; 32], Arc<Session>>,
    closed_tokens: HashMap<[u8; 32], Instant>,
    sessions_per_ip: HashMap<IpAddr, usize>,
    sessions_per_profile: HashMap<String, usize>,
    streams_per_profile: HashMap<String, usize>,
    dials_per_profile: HashMap<String, usize>,
    bootstrap_rate: RateState,
    session_rate: RateState,
    stream_rate: RateState,
    profile_session_rates: HashMap<String, RateState>,
    profile_stream_rates: HashMap<String, RateState>,
    streams_live: usize,
    backend_dials_in_flight: usize,
    closed: bool,
    next_session_id: u64,
}

/// Owner of every bootstrap, session, and process-wide relay budget.
pub(crate) struct Manager {
    pub(crate) limits: WebLimits,
    pub(crate) timeouts: WebTimeouts,
    /// Capability profiles and their lookup index.
    ///
    /// The reference relay scans every profile in constant time. Telemt can
    /// derive one profile per configured user, so the scan is replaced by a
    /// randomly keyed hash lookup: the map probe does not depend on secret
    /// bytes in a way a remote peer can observe, and a match still requires
    /// knowing all 256 bits of the capability.
    profiles: ArcSwap<ProfileSet>,
    runtime: Arc<WebRuntime>,
    rng: Arc<SecureRandom>,
    state: Mutex<ManagerState>,
    pending: Mutex<GlobalPending>,
    metrics: WebMetrics,
    self_ref: Weak<Manager>,
}

impl Manager {
    /// Builds the relay manager for a validated profile set.
    pub(crate) fn new(
        limits: WebLimits,
        timeouts: WebTimeouts,
        profiles: Vec<Arc<WebProfile>>,
        runtime: Arc<WebRuntime>,
    ) -> Result<Arc<Self>> {
        let pending = GlobalPending::new(&limits);
        if pending.data_partition_empty() {
            return Err(ProxyError::Config(
                "web control reserve times max_sessions_global exhausts the global pending pool"
                    .to_string(),
            ));
        }
        let set = Arc::new(ProfileSet::new(profiles)?);
        Ok(Arc::new_cyclic(|self_ref| Self {
            limits,
            timeouts,
            profiles: ArcSwap::new(set),
            runtime,
            rng: Arc::new(SecureRandom::new()),
            state: Mutex::new(ManagerState::default()),
            pending: Mutex::new(pending),
            metrics: WebMetrics::default(),
            self_ref: self_ref.clone(),
        }))
    }

    /// Resolves a bridge capability to its profile.
    pub(crate) fn match_capability(&self, candidate: &[u8; 32]) -> Option<Arc<WebProfile>> {
        let set = self.profiles.load();
        let position = *set.index.get(candidate)?;
        let profile = set.profiles.get(position)?;
        let confirmed = profile
            .capabilities
            .iter()
            .any(|value| bool::from(value.ct_eq(candidate)));
        confirmed.then(|| profile.clone())
    }

    /// Installs a rebuilt profile set after a configuration reload.
    ///
    /// Live sessions keep the profile object they were created with, and all
    /// per-profile accounting is keyed by name, so an in-flight session is
    /// unaffected by the swap.
    pub(crate) fn replace_profiles(&self, profiles: Vec<Arc<WebProfile>>) -> Result<()> {
        let set = Arc::new(ProfileSet::new(profiles)?);
        self.profiles.store(set);
        Ok(())
    }
    /// Charges the process-wide pending pool.
    pub(crate) fn reserve_pending_budget(
        &self,
        cost: usize,
        items: usize,
        class: crate::web::session::state::PendingClass,
    ) -> bool {
        let allowed = self.pending.lock().reserve(cost, items, class);
        if !allowed {
            self.count_limit_hit();
        }
        allowed
    }

    /// Returns budget to the process-wide pending pool.
    pub(crate) fn release_pending_budget(&self, cost: usize, items: usize) {
        self.pending.lock().release(cost, items);
    }
    /// Counts a stream rejected by a capacity or rate limit.
    pub(crate) fn count_stream_rejected(&self) {
        self.metrics
            .streams_rejected
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.count_limit_hit();
    }

    /// Counts uplink carrier bytes.
    pub(crate) fn count_bytes_up(&self, bytes: usize) {
        self.metrics
            .bytes_up
            .fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// Counts downlink carrier bytes.
    pub(crate) fn count_bytes_down(&self, bytes: usize) {
        self.metrics
            .bytes_down
            .fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn count_limit_hit(&self) {
        self.metrics
            .limit_hits
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Samples live capacity for the metrics endpoints.
    pub(crate) fn capacity(&self) -> WebCapacity {
        let (pending_bytes, pending_items) = self.pending.lock().usage();
        let guard = self.state.lock();
        WebCapacity {
            sessions: guard.sessions.len(),
            streams: guard.streams_live,
            backend_dials_in_flight: guard.backend_dials_in_flight,
            pending_bytes: pending_bytes as u64,
            pending_items: pending_items as u64,
        }
    }

    /// Profiles served by this relay, used by the readiness probe.
    pub(crate) fn profiles(&self) -> Arc<ProfileSet> {
        self.profiles.load_full()
    }

    /// Closes every session and stops accepting new ones.
    pub(crate) fn shutdown(&self) {
        let sessions = {
            let mut guard = self.state.lock();
            guard.closed = true;
            guard.sessions.values().cloned().collect::<Vec<_>>()
        };
        for session in sessions {
            session.close();
        }
    }

    /// Drops expired bootstraps and sessions idle past the reconnect grace.
    pub(crate) fn reap(&self) {
        let now = Instant::now();
        let grace = Duration::from_millis(self.timeouts.reconnect_grace_ms);
        // Sessions are collected under the manager lock but inspected without
        // it: `last_activity` takes the session lock, and the lock order is
        // always session before manager.
        let sessions = {
            let mut guard = self.state.lock();
            let state = &mut *guard;
            remove_expired(state, now);
            state.sessions.values().cloned().collect::<Vec<_>>()
        };
        for session in sessions {
            if now.saturating_duration_since(session.last_activity()) > grace {
                session.close();
            }
        }
    }

    /// Runs the periodic reaper until the relay shuts down.
    pub(crate) async fn run_cleanup(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(CLEANUP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if self.state.lock().closed {
                return;
            }
            self.reap();
        }
    }

    pub(super) fn new_token(&self) -> (String, [u8; 32]) {
        let mut raw = [0u8; TOKEN_BYTES];
        self.rng.fill(&mut raw);
        (encode_token(&raw), token_hash(&raw))
    }
}

impl WebMetricsSource for Manager {
    fn snapshot(&self) -> WebMetricsSnapshot {
        self.metrics.snapshot(self.capacity())
    }
}

pub(super) fn remove_expired(state: &mut ManagerState, now: Instant) {
    let expired: Vec<[u8; 32]> = state
        .bootstraps
        .iter()
        .filter(|(_, entry)| now > entry.expires)
        .map(|(hash, _)| *hash)
        .collect();
    for hash in expired {
        if let Some(entry) = state.bootstraps.remove(&hash)
            && !entry.used
        {
            decrement_ip(&mut state.bootstraps_per_ip, entry.issuance_ip);
        }
    }
    state.closed_tokens.retain(|_, expires| now <= *expires);
}

pub(super) fn evict_oldest_unused(state: &mut ManagerState) -> bool {
    let oldest = state
        .bootstraps
        .iter()
        .filter(|(_, entry)| !entry.used)
        .min_by_key(|(_, entry)| entry.expires)
        .map(|(hash, _)| *hash);
    let Some(hash) = oldest else {
        return false;
    };
    if let Some(entry) = state.bootstraps.remove(&hash) {
        decrement_ip(&mut state.bootstraps_per_ip, entry.issuance_ip);
    }
    true
}

pub(super) fn decrement_ip(counters: &mut HashMap<IpAddr, usize>, ip: IpAddr) {
    if let Some(count) = counters.get_mut(&ip) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            counters.remove(&ip);
        }
    }
}

pub(super) fn decrement_profile(counters: &mut HashMap<String, usize>, name: &str) {
    if let Some(count) = counters.get_mut(name) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            counters.remove(name);
        }
    }
}
