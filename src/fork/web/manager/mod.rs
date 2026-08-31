//! Bootstrap and session registry with capacity, rate, and budget control.
//!
//! Submodules:
//! - `limits`: token buckets, the process-wide pending pool, stream permits
//! - `sessions`: bootstrap issuance, session creation, and stream accounting

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use subtle::ConstantTimeEq;
use tracing::{debug, info};

use crate::config::fork::web::{CarrierMode, WebBackend, WebLimits, WebProfileLimits, WebTimeouts};
use crate::crypto::SecureRandom;
use crate::error::{ProxyError, Result};
use crate::fork::web::capability::{TOKEN_BYTES, encode_token, token_hash};
use crate::fork::web::metrics::{WebCapacity, WebMetrics, WebMetricsSnapshot, WebMetricsSource};
use crate::fork::web::runtime::WebRuntime;
use crate::fork::web::session::Session;

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
    ///
    /// The first capability of a profile is the one its configured secret form
    /// derives, and two profiles claiming the same one is a configuration the
    /// operator has to resolve. The rest are aliases this relay adds so a
    /// client handed either secret form still reaches its profile; an alias
    /// that collides is dropped rather than fatal, because the profile it would
    /// have pointed at is reachable through its own primary capability anyway.
    pub(crate) fn new(profiles: Vec<Arc<WebProfile>>) -> Result<Self> {
        let mut index = HashMap::with_capacity(profiles.len() * 2);
        for (position, profile) in profiles.iter().enumerate() {
            let Some(primary) = profile.capabilities.first() else {
                continue;
            };
            if index.insert(*primary, position).is_some() {
                return Err(ProxyError::Config(format!(
                    "duplicate web capability for profile '{}'",
                    profile.name
                )));
            }
        }
        for (position, profile) in profiles.iter().enumerate() {
            for alias in profile.capabilities.iter().skip(1) {
                match index.entry(*alias) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(position);
                    }
                    std::collections::hash_map::Entry::Occupied(_) => {
                        debug!(
                            profile = %profile.name,
                            "WEB secret-form alias already claimed; keeping the owning profile"
                        );
                    }
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
    /// Wakes streams parked on the process-wide pool when budget is released.
    pending_released: Arc<tokio::sync::Notify>,
    metrics: WebMetrics,
    self_ref: Weak<Manager>,
}

/// Rejects a limit set whose control reserve leaves no room for data frames.
///
/// Split out so start-up can check it before binding anything, rather than
/// discovering it when the first stream is refused.
pub(crate) fn validate_pending_split(limits: &WebLimits) -> Result<()> {
    if GlobalPending::new(limits).data_partition_empty() {
        return Err(ProxyError::Config(
            "web control reserve times max_sessions_global exhausts the global pending pool"
                .to_string(),
        ));
    }
    Ok(())
}

impl Manager {
    /// Builds the relay manager for a validated profile set.
    pub(crate) fn new(
        limits: WebLimits,
        timeouts: WebTimeouts,
        profiles: Vec<Arc<WebProfile>>,
        runtime: Arc<WebRuntime>,
    ) -> Result<Arc<Self>> {
        validate_pending_split(&limits)?;
        let pending = GlobalPending::new(&limits);
        let set = Arc::new(ProfileSet::new(profiles)?);
        Ok(Arc::new_cyclic(|self_ref| Self {
            limits,
            timeouts,
            profiles: ArcSwap::new(set),
            runtime,
            rng: Arc::new(SecureRandom::new()),
            state: Mutex::new(ManagerState::default()),
            pending: Mutex::new(pending),
            pending_released: Arc::new(tokio::sync::Notify::new()),
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
    /// Per-profile accounting is keyed by name, so an in-flight session whose
    /// profile survived the reload is unaffected by the swap. A session whose
    /// profile lost the capability it was created from is closed: rotating a
    /// leaked secret has to end the sessions that secret opened, otherwise the
    /// holder keeps relaying forever because every uplink refreshes the idle
    /// timer the reaper watches.
    pub(crate) fn replace_profiles(&self, profiles: Vec<Arc<WebProfile>>) -> Result<()> {
        let set = Arc::new(ProfileSet::new(profiles)?);
        let revoked = {
            let mut guard = self.state.lock();
            let state = &mut *guard;
            // An unredeemed bootstrap outlives the capability it was issued
            // from unless it is dropped here, and it captures its profile at
            // issuance. Leaving it would let the holder mint a fresh session on
            // a revoked secret for the rest of the bootstrap lifetime — and
            // because the session sweep below has already run, that session
            // would never be revoked at all.
            let mut dropped = Vec::new();
            state.bootstraps.retain(|_, entry| {
                if entry.used || profile_survives(&set, &entry.profile) {
                    return true;
                }
                dropped.push(entry.issuance_ip);
                false
            });
            for issuance_ip in dropped {
                decrement_ip(&mut state.bootstraps_per_ip, issuance_ip);
            }
            state
                .sessions
                .values()
                .filter(|session| !profile_survives(&set, &session.profile))
                .cloned()
                .collect::<Vec<_>>()
        };
        self.profiles.store(set);
        if !revoked.is_empty() {
            info!(
                sessions = revoked.len(),
                "WEB proxy closing sessions whose capability was revoked"
            );
        }
        for session in revoked {
            session.close();
        }
        Ok(())
    }
    /// Charges the process-wide pending pool.
    pub(crate) fn reserve_pending_budget(
        &self,
        cost: usize,
        items: usize,
        class: crate::fork::web::session::state::PendingClass,
    ) -> bool {
        let allowed = self.pending.lock().reserve(cost, items, class);
        if !allowed {
            debug!(
                cost,
                items,
                class = ?class,
                "WEB relay refused a queue reservation: process pool exhausted"
            );
            self.count_limit_hit();
        }
        allowed
    }

    /// Returns budget to the process-wide pending pool.
    pub(crate) fn release_pending_budget(&self, cost: usize, items: usize) {
        self.pending.lock().release(cost, items);
        // Wake every stream parked on the process-wide pool. A session's own
        // headroom wakes its writer through `release_pending_locked`, but a
        // pool exhausted by *other* sessions produces no such signal, and
        // polling for it burns wake-ups precisely when the process is
        // saturated.
        self.pending_released.notify_waiters();
    }

    /// Signalled whenever the process-wide pending pool gives budget back.
    pub(crate) fn pending_released(&self) -> Arc<tokio::sync::Notify> {
        self.pending_released.clone()
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

    /// Counts one bridge page rendered for a matching capability.
    pub(crate) fn count_bridge_page(&self) {
        self.metrics
            .bridge_pages_served
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Counts MTProto payload handed to a backend.
    pub(crate) fn count_stream_bytes_up(&self, bytes: usize) {
        self.metrics
            .stream_bytes_up
            .fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// Counts MTProto payload taken from a backend.
    pub(crate) fn count_stream_bytes_down(&self, bytes: usize) {
        self.metrics
            .stream_bytes_down
            .fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// Counts a carrier connection refused by the accept-loop budget.
    pub(crate) fn count_carrier_connection_dropped(&self) {
        self.metrics
            .carrier_connections_dropped
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Counts a request that overran the relay's own deadline.
    pub(crate) fn count_request_timeout(&self) {
        self.metrics
            .request_timeouts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Counts one retryable answer handed back to a carrier.
    pub(crate) fn count_retry_later(&self) {
        self.metrics
            .retry_later_responses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

/// True when a reloaded profile set still grants the session's capability.
///
/// A profile is matched by name *and* by its full capability list, so rotating
/// a secret revokes the sessions it opened even though the profile name and
/// its per-profile accounting survive the reload.
fn profile_survives(set: &ProfileSet, profile: &Arc<WebProfile>) -> bool {
    set.profiles.iter().any(|candidate| {
        candidate.name == profile.name && candidate.capabilities == profile.capabilities
    })
}

/// Collapses a client address into the bucket the per-IP ceilings count.
///
/// An IPv4 address is its own bucket. A single IPv6 client is routinely handed
/// a whole /64, so counting exact addresses there would let one subscriber walk
/// 2^64 keys past every per-IP ceiling.
pub(super) fn ip_bucket(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(value) => {
            let mut octets = value.octets();
            octets[8..].fill(0);
            IpAddr::V6(Ipv6Addr::from(octets))
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
