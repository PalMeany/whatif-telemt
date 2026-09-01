//! Operator session registry.
//!
//! Sessions live in memory only: a restart logs everyone out, which is the
//! behaviour an operator can reason about after a crash and which keeps session
//! bearers off disk entirely. The map is keyed by the SHA-256 of the cookie
//! value, so the registry never holds a token that could be replayed if it
//! leaked.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::crypto::{SecureRandom, sha256};

use super::crypto::{encode, random_token, secure_eq};

/// One live operator session.
#[derive(Debug, Clone)]
pub(crate) struct Session {
    /// Operator identifier this session authenticates.
    pub(crate) operator_id: String,
    /// Double-submit token required on every state-changing request.
    pub(crate) csrf_token: String,
    /// Unix seconds the session was created at.
    pub(crate) created_at: u64,
    /// Unix seconds of the most recent request served on this session.
    pub(crate) last_seen: u64,
    /// Address the session was created from.
    pub(crate) address: Option<IpAddr>,
    /// User agent recorded at login, truncated for display.
    pub(crate) user_agent: String,
}

/// Outcome of resolving a cookie against the registry.
///
/// Expiry and revocation are deliberately indistinguishable from an unknown
/// cookie: the registry prunes expired sessions before every lookup, and the
/// difference is not something the browser should be told anyway.
pub(crate) enum Lookup {
    /// The cookie names a live session.
    Live(Session),
    /// The cookie names nothing the registry still holds.
    Unknown,
}

/// Bounds applied to the registry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionLimits {
    /// Absolute lifetime, in seconds.
    pub(crate) ttl_secs: u64,
    /// Idle timeout, in seconds.
    pub(crate) idle_timeout_secs: u64,
    /// Concurrent sessions one operator may hold.
    pub(crate) max_per_operator: usize,
    /// Concurrent sessions across all operators.
    pub(crate) max_total: usize,
}

/// In-memory session registry.
pub(crate) struct SessionRegistry {
    limits: SessionLimits,
    random: Arc<SecureRandom>,
    sessions: Mutex<HashMap<String, Session>>,
}

impl SessionRegistry {
    /// Builds an empty registry.
    pub(crate) fn new(limits: SessionLimits, random: Arc<SecureRandom>) -> Self {
        Self {
            limits,
            random,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Creates a session and returns the cookie value handed to the client.
    ///
    /// The caller never learns the storage key: only the client's cookie can
    /// produce it, which is what makes a registry dump useless on its own.
    pub(crate) fn create(
        &self,
        operator_id: &str,
        now: u64,
        address: Option<IpAddr>,
        user_agent: &str,
    ) -> (String, Session) {
        let token = random_token(&self.random);
        let session = Session {
            operator_id: operator_id.to_string(),
            csrf_token: random_token(&self.random),
            created_at: now,
            last_seen: now,
            address,
            user_agent: truncate_user_agent(user_agent),
        };
        let mut sessions = self.sessions.lock();
        prune_expired(&mut sessions, &self.limits, now);
        enforce_per_operator(&mut sessions, operator_id, self.limits.max_per_operator);
        enforce_total(&mut sessions, self.limits.max_total);
        sessions.insert(storage_key(&token), session.clone());
        (token, session)
    }

    /// Resolves a cookie value, refreshing the idle timer on success.
    pub(crate) fn touch(&self, token: &str, now: u64) -> Lookup {
        let key = storage_key(token);
        let mut sessions = self.sessions.lock();
        prune_expired(&mut sessions, &self.limits, now);
        match sessions.get_mut(&key) {
            Some(session) => {
                session.last_seen = now;
                Lookup::Live(session.clone())
            }
            None => Lookup::Unknown,
        }
    }

    /// Re-inserts a session under a cookie value the client already holds.
    ///
    /// Used by the "revoke every other session" paths: the whole operator is
    /// cleared and the caller's own session is put straight back, so the
    /// browser that issued the request is not logged out by its own action.
    pub(crate) fn reinstate(&self, token: &str, mut session: Session, now: u64) {
        session.last_seen = now;
        let mut sessions = self.sessions.lock();
        prune_expired(&mut sessions, &self.limits, now);
        enforce_total(&mut sessions, self.limits.max_total);
        sessions.insert(storage_key(token), session);
    }

    /// Drops one session by its cookie value.
    pub(crate) fn revoke(&self, token: &str) -> bool {
        self.sessions.lock().remove(&storage_key(token)).is_some()
    }

    /// Drops every session belonging to one operator.
    ///
    /// Used after a password change, a role change, and an account being
    /// disabled: a credential that no longer proves anything must not keep
    /// authenticating requests through an already-issued cookie.
    pub(crate) fn revoke_operator(&self, operator_id: &str) -> usize {
        let mut sessions = self.sessions.lock();
        let before = sessions.len();
        sessions.retain(|_, session| session.operator_id != operator_id);
        before - sessions.len()
    }

    /// Lists the live sessions of one operator, newest first.
    pub(crate) fn list_for_operator(&self, operator_id: &str, now: u64) -> Vec<Session> {
        let mut sessions = self.sessions.lock();
        prune_expired(&mut sessions, &self.limits, now);
        let mut listed: Vec<Session> = sessions
            .values()
            .filter(|session| session.operator_id == operator_id)
            .cloned()
            .collect();
        listed.sort_by_key(|session| std::cmp::Reverse(session.created_at));
        listed
    }
}

/// Constant-time comparison of a submitted CSRF token with the session's.
pub(crate) fn csrf_matches(session: &Session, submitted: &str) -> bool {
    secure_eq(session.csrf_token.as_bytes(), submitted.as_bytes())
}

/// Maps a cookie value onto its storage key.
fn storage_key(token: &str) -> String {
    encode(&sha256(token.as_bytes()))
}

/// Removes sessions past either bound.
fn prune_expired(sessions: &mut HashMap<String, Session>, limits: &SessionLimits, now: u64) {
    sessions.retain(|_, session| {
        let absolute_ok = now.saturating_sub(session.created_at) < limits.ttl_secs;
        let idle_ok = now.saturating_sub(session.last_seen) < limits.idle_timeout_secs;
        absolute_ok && idle_ok
    });
}

/// Drops the operator's oldest sessions until one slot is free.
fn enforce_per_operator(
    sessions: &mut HashMap<String, Session>,
    operator_id: &str,
    max_per_operator: usize,
) {
    loop {
        let owned: Vec<(String, u64)> = sessions
            .iter()
            .filter(|(_, session)| session.operator_id == operator_id)
            .map(|(key, session)| (key.clone(), session.created_at))
            .collect();
        if owned.len() < max_per_operator {
            return;
        }
        let Some((oldest, _)) = owned.into_iter().min_by_key(|(_, created)| *created) else {
            return;
        };
        sessions.remove(&oldest);
    }
}

/// Drops the globally oldest sessions until one slot is free.
fn enforce_total(sessions: &mut HashMap<String, Session>, max_total: usize) {
    while sessions.len() >= max_total {
        let Some(oldest) = sessions
            .iter()
            .min_by_key(|(_, session)| session.created_at)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        sessions.remove(&oldest);
    }
}

/// Trims a user agent to a length worth storing and displaying.
fn truncate_user_agent(value: &str) -> String {
    const MAX: usize = 160;
    let cleaned: String = value
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX)
        .collect();
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(max_per_operator: usize, max_total: usize) -> SessionRegistry {
        SessionRegistry::new(
            SessionLimits {
                ttl_secs: 3_600,
                idle_timeout_secs: 600,
                max_per_operator,
                max_total,
            },
            Arc::new(SecureRandom::new()),
        )
    }

    #[test]
    fn a_created_session_resolves_from_its_cookie_only() {
        let registry = registry(4, 16);
        let (token, session) = registry.create("op", 1_000, None, "agent");
        assert!(matches!(registry.touch(&token, 1_010), Lookup::Live(_)));
        assert!(matches!(registry.touch("other", 1_010), Lookup::Unknown));
        assert!(!session.csrf_token.is_empty());
    }

    #[test]
    fn the_idle_timeout_and_the_absolute_lifetime_both_apply() {
        let registry = registry(4, 16);
        let (token, _) = registry.create("op", 1_000, None, "agent");
        assert!(matches!(registry.touch(&token, 1_500), Lookup::Live(_)));
        // Idle past the timeout even though the absolute lifetime is intact.
        assert!(matches!(registry.touch(&token, 2_200), Lookup::Unknown));

        let (token, _) = registry.create("op", 1_000, None, "agent");
        // Kept warm by traffic, but the absolute lifetime still ends it: the
        // session was created at 1_000 and the ttl is 3_600.
        for now in (1_000..4_600).step_by(300) {
            assert!(matches!(registry.touch(&token, now), Lookup::Live(_)));
        }
        assert!(matches!(registry.touch(&token, 4_600), Lookup::Unknown));
    }

    #[test]
    fn the_per_operator_ceiling_evicts_the_oldest_session() {
        let registry = registry(2, 16);
        let (first, _) = registry.create("op", 1_000, None, "agent");
        let (second, _) = registry.create("op", 1_001, None, "agent");
        let (third, _) = registry.create("op", 1_002, None, "agent");
        assert!(matches!(registry.touch(&first, 1_003), Lookup::Unknown));
        assert!(matches!(registry.touch(&second, 1_003), Lookup::Live(_)));
        assert!(matches!(registry.touch(&third, 1_003), Lookup::Live(_)));
    }

    #[test]
    fn revoking_an_operator_drops_every_one_of_its_sessions() {
        let registry = registry(4, 16);
        let (first, _) = registry.create("op", 1_000, None, "agent");
        let (second, _) = registry.create("op", 1_001, None, "agent");
        let (other, _) = registry.create("other", 1_002, None, "agent");
        assert_eq!(registry.revoke_operator("op"), 2);
        assert!(matches!(registry.touch(&first, 1_003), Lookup::Unknown));
        assert!(matches!(registry.touch(&second, 1_003), Lookup::Unknown));
        assert!(matches!(registry.touch(&other, 1_003), Lookup::Live(_)));
    }

    #[test]
    fn reinstating_keeps_the_caller_signed_in_after_a_mass_revoke() {
        let registry = registry(4, 16);
        let (mine, session) = registry.create("op", 1_000, None, "agent");
        let (other, _) = registry.create("op", 1_001, None, "agent");
        assert_eq!(registry.revoke_operator("op"), 2);
        registry.reinstate(&mine, session, 1_002);
        assert!(matches!(registry.touch(&mine, 1_003), Lookup::Live(_)));
        assert!(matches!(registry.touch(&other, 1_003), Lookup::Unknown));
    }

    #[test]
    fn csrf_comparison_rejects_a_foreign_token() {
        let registry = registry(4, 16);
        let (_, session) = registry.create("op", 1_000, None, "agent");
        assert!(csrf_matches(&session, &session.csrf_token));
        assert!(!csrf_matches(&session, "wrong"));
    }
}
