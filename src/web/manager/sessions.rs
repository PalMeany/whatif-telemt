//! Bootstrap issuance, session creation, and live-stream accounting.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use subtle::ConstantTimeEq;

use crate::crypto::hash::sha256;
use crate::web::capability::{decode_token, token_hash};
use crate::web::error::WebError;
use crate::web::frame::{self, FrameType};
use crate::web::session::{CreateOutcome, Session, SessionOptions};

use tracing::debug;

use super::limits::{StreamPermit, allow_profile_rate, allow_rate};
use super::{
    Bootstrap, Manager, WebProfile, decrement_ip, decrement_profile, evict_oldest_unused,
    ip_bucket, remove_expired,
};

impl Manager {
    /// Issues a short-lived bootstrap for one bridge page render.
    pub(crate) fn issue_bootstrap(
        &self,
        profile: &Arc<WebProfile>,
        client_ip: IpAddr,
    ) -> std::result::Result<String, WebError> {
        let now = Instant::now();
        let bucket = ip_bucket(client_ip);
        let (token, hash) = self.new_token();
        let mut guard = self.state.lock();
        let state = &mut *guard;
        if state.closed {
            return Err(WebError::Closed);
        }
        remove_expired(state, now);
        let per_ip_full = self.limits.max_bootstraps_per_ip != 0
            && state.bootstraps_per_ip.get(&bucket).copied().unwrap_or(0)
                >= self.limits.max_bootstraps_per_ip;
        if per_ip_full
            || !allow_rate(
                &mut state.bootstrap_rate,
                now,
                self.limits.new_bootstraps_per_minute,
                self.limits.new_bootstraps_burst,
            )
        {
            debug!(
                profile = %profile.name,
                per_ip_full,
                "WEB bootstrap refused by a rate or per-address ceiling"
            );
            self.count_limit_hit();
            return Err(WebError::Limit);
        }
        if state.bootstraps.len() >= self.limits.max_bootstraps_global
            && !evict_oldest_unused(state)
        {
            debug!(
                profile = %profile.name,
                "WEB bootstrap refused: every issued bootstrap is already redeemed"
            );
            self.count_limit_hit();
            return Err(WebError::Limit);
        }
        state.bootstraps.insert(
            hash,
            Bootstrap {
                expires: now + Duration::from_millis(self.timeouts.bootstrap_lifetime_ms),
                profile: profile.clone(),
                issuance_ip: bucket,
                body_digest: [0u8; 32],
                session_token: String::new(),
                session: None,
                used: false,
            },
        );
        *state.bootstraps_per_ip.entry(bucket).or_insert(0) += 1;
        Ok(token)
    }

    /// True while a bootstrap bearer is still redeemable.
    pub(crate) fn has_bootstrap(&self, token: &str) -> bool {
        let Some(raw) = decode_token(token) else {
            return false;
        };
        let hash = token_hash(&raw);
        let now = Instant::now();
        let guard = self.state.lock();
        guard
            .bootstraps
            .get(&hash)
            .is_some_and(|entry| now <= entry.expires)
    }

    /// Exchanges a bootstrap for a session, idempotently and atomically.
    pub(crate) fn create(
        &self,
        token: &str,
        client_ip: IpAddr,
        body: &[u8],
    ) -> std::result::Result<CreateOutcome, WebError> {
        // The client's protocol version is read but not yet acted on: v1 is the
        // only version this relay speaks, and answering a newer HELLO with the
        // v1 WELCOME is the downgrade signal a later client needs. Refusing it
        // outright would be a 404 that a client cannot tell apart from "this
        // host is not a relay at all".
        let client_version = frame::parse_hello(body).map_err(|_| WebError::Protocol)?;
        let raw = decode_token(token).ok_or(WebError::Authentication)?;
        let hash = token_hash(&raw);
        let body_digest = sha256(body);
        let now = Instant::now();

        let mut guard = self.state.lock();
        let state = &mut *guard;
        let Some(entry) = state.bootstraps.get(&hash) else {
            return Err(WebError::Authentication);
        };
        if now > entry.expires {
            return Err(WebError::Authentication);
        }
        if entry.used {
            let digest_matches = bool::from(entry.body_digest.ct_eq(&body_digest));
            let Some(session) = entry.session.clone() else {
                return Err(WebError::Authentication);
            };
            if !digest_matches {
                return Err(WebError::Authentication);
            }
            return Ok(CreateOutcome {
                token: entry.session_token.clone(),
                welcome: frame::encode(FrameType::WELCOME, 0, &[]),
                session,
            });
        }
        let profile = entry.profile.clone();
        let issuance_ip = entry.issuance_ip;
        let bucket = ip_bucket(client_ip);
        let profile_limits = profile.limits.clone();
        let sessions_for_profile = state
            .sessions_per_profile
            .get(&profile.name)
            .copied()
            .unwrap_or(0);
        let sessions_for_ip = state.sessions_per_ip.get(&bucket).copied().unwrap_or(0);
        let per_ip_full = self.limits.max_sessions_per_ip != 0
            && sessions_for_ip >= self.limits.max_sessions_per_ip;
        if state.closed
            || state.sessions.len() >= self.limits.max_sessions_global
            || sessions_for_profile >= profile_limits.max_sessions
            || per_ip_full
        {
            debug!(
                profile = %profile.name,
                per_ip_full,
                sessions_for_profile,
                "WEB session refused by a capacity ceiling"
            );
            self.count_limit_hit();
            return Err(WebError::Limit);
        }
        let profile_rate = state
            .profile_session_rates
            .entry(profile.name.clone())
            .or_default();
        let mut global_rate = state.session_rate;
        let allowed = allow_profile_rate(
            &mut global_rate,
            profile_rate,
            now,
            self.limits.new_sessions_per_minute,
            self.limits.new_sessions_burst,
            profile_limits.new_sessions_per_minute,
            profile_limits.new_sessions_burst,
        );
        if !allowed {
            debug!(profile = %profile.name, "WEB session refused by a creation rate limit");
            self.count_limit_hit();
            return Err(WebError::Limit);
        }
        state.session_rate = global_rate;

        let (session_token, session_hash) = self.new_token();
        let mut session_limits = self.limits.clone();
        session_limits.max_streams_per_session = profile_limits.max_streams_per_session;
        session_limits.max_pending_per_session = profile_limits.max_pending_per_session;
        state.next_session_id += 1;
        let created = Session::new(SessionOptions {
            id: state.next_session_id,
            token_hash: session_hash,
            profile: profile.clone(),
            client_ip,
            limits: session_limits,
            timeouts: self.timeouts.clone(),
            manager: self.self_ref.clone(),
            runtime: self.runtime.clone(),
            rng: self.rng.clone(),
        });
        state.sessions.insert(session_hash, created.clone());
        *state.sessions_per_ip.entry(bucket).or_insert(0) += 1;
        *state
            .sessions_per_profile
            .entry(profile.name.clone())
            .or_insert(0) += 1;
        decrement_ip(&mut state.bootstraps_per_ip, issuance_ip);
        if let Some(entry) = state.bootstraps.get_mut(&hash) {
            entry.used = true;
            entry.body_digest = body_digest;
            entry.session_token = session_token.clone();
            entry.session = Some(created.clone());
        }
        self.metrics
            .sessions_created
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        debug!(
            session = created.id,
            profile = %profile.name,
            carrier = created.carrier_mode().as_str(),
            client_version,
            negotiated = client_version.min(frame::PROTOCOL_VERSION),
            "WEB session created"
        );
        Ok(CreateOutcome {
            token: session_token,
            welcome: frame::encode(FrameType::WELCOME, 0, &[]),
            session: created,
        })
    }

    /// Looks up a live session by its bearer.
    pub(crate) fn get(&self, token: &str) -> Option<Arc<Session>> {
        let raw = decode_token(token)?;
        let hash = token_hash(&raw);
        self.state.lock().sessions.get(&hash).cloned()
    }

    /// Closes a session by bearer; idempotent for a recently closed token.
    pub(crate) fn close_token(&self, token: &str) -> std::result::Result<(), WebError> {
        let raw = decode_token(token).ok_or(WebError::Authentication)?;
        let hash = token_hash(&raw);
        let (session, recently_closed) = {
            let guard = self.state.lock();
            (
                guard.sessions.get(&hash).cloned(),
                guard.closed_tokens.contains_key(&hash),
            )
        };
        match session {
            Some(session) => {
                session.close();
                Ok(())
            }
            None if recently_closed => Ok(()),
            None => Err(WebError::Authentication),
        }
    }

    /// Reserves one live-stream slot and its backend dial slot.
    pub(crate) fn acquire_stream(&self, profile: &Arc<WebProfile>) -> Option<StreamPermit> {
        let now = Instant::now();
        let mut guard = self.state.lock();
        let state = &mut *guard;
        let streams_for_profile = state
            .streams_per_profile
            .get(&profile.name)
            .copied()
            .unwrap_or(0);
        let dials_for_profile = state
            .dials_per_profile
            .get(&profile.name)
            .copied()
            .unwrap_or(0);
        if state.closed
            || state.streams_live >= self.limits.max_streams_global
            || state.backend_dials_in_flight >= self.limits.max_backend_dials_in_flight
            || streams_for_profile >= profile.limits.max_streams
            || dials_for_profile >= profile.limits.max_backend_dials_in_flight
        {
            debug!(
                profile = %profile.name,
                streams_live = state.streams_live,
                dials_in_flight = state.backend_dials_in_flight,
                "WEB stream refused by a capacity ceiling"
            );
            return None;
        }
        let profile_rate = state
            .profile_stream_rates
            .entry(profile.name.clone())
            .or_default();
        let mut global_rate = state.stream_rate;
        let allowed = allow_profile_rate(
            &mut global_rate,
            profile_rate,
            now,
            self.limits.new_streams_per_minute,
            self.limits.new_streams_burst,
            profile.limits.new_streams_per_minute,
            profile.limits.new_streams_burst,
        );
        if !allowed {
            debug!(profile = %profile.name, "WEB stream refused by a creation rate limit");
            return None;
        }
        state.stream_rate = global_rate;
        state.streams_live += 1;
        state.backend_dials_in_flight += 1;
        *state
            .streams_per_profile
            .entry(profile.name.clone())
            .or_insert(0) += 1;
        *state
            .dials_per_profile
            .entry(profile.name.clone())
            .or_insert(0) += 1;
        self.metrics
            .streams_opened
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(StreamPermit::new(self.self_ref.clone(), profile.clone()))
    }

    /// Releases one in-flight backend dial slot.
    pub(crate) fn backend_dial_finished(&self, profile: &Arc<WebProfile>, failed: bool) {
        {
            let mut guard = self.state.lock();
            let state = &mut *guard;
            state.backend_dials_in_flight = state.backend_dials_in_flight.saturating_sub(1);
            decrement_profile(&mut state.dials_per_profile, &profile.name);
        }
        if failed {
            self.metrics
                .backend_dial_failures
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Releases one live-stream slot.
    pub(crate) fn stream_finished(&self, profile: &Arc<WebProfile>) {
        let mut guard = self.state.lock();
        let state = &mut *guard;
        state.streams_live = state.streams_live.saturating_sub(1);
        decrement_profile(&mut state.streams_per_profile, &profile.name);
    }

    /// Unregisters a closed session and remembers its token briefly.
    pub(crate) fn session_finished(&self, session: &Session) {
        {
            let mut guard = self.state.lock();
            let state = &mut *guard;
            let owned = state
                .sessions
                .get(&session.token_hash)
                .is_some_and(|current| current.id == session.id);
            if owned {
                state.sessions.remove(&session.token_hash);
                if state.closed_tokens.len() >= self.limits.max_sessions_global * 16
                    && let Some(oldest) = state
                        .closed_tokens
                        .iter()
                        .min_by_key(|(_, expires)| **expires)
                        .map(|(hash, _)| *hash)
                {
                    state.closed_tokens.remove(&oldest);
                }
                state.closed_tokens.insert(
                    session.token_hash,
                    Instant::now() + Duration::from_millis(self.timeouts.bootstrap_lifetime_ms),
                );
                decrement_ip(&mut state.sessions_per_ip, ip_bucket(session.client_ip));
                decrement_profile(&mut state.sessions_per_profile, &session.profile.name);
            }
            let session_id = session.id;
            state.bootstraps.retain(|_, entry| {
                entry.session.as_ref().map(|value| value.id) != Some(session_id)
            });
        }
        self.metrics
            .sessions_closed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}
