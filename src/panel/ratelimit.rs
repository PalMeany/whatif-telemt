//! Login throttling for the panel.
//!
//! Two independent buckets are kept: one per account name and one per source
//! address. The account bucket stops a password from being guessed from a
//! rotating set of addresses; the address bucket stops one address from
//! sweeping a list of account names. Both are bounded, because both are keyed
//! by attacker-chosen input.

use std::collections::HashMap;
use std::net::IpAddr;

use parking_lot::Mutex;

/// Buckets retained per dimension before the coldest entries are evicted.
const MAX_TRACKED_KEYS: usize = 8_192;

/// One throttled key's state.
#[derive(Debug, Clone, Copy)]
struct Attempts {
    failures: u32,
    locked_until: u64,
    last_seen: u64,
}

/// Result of asking whether a login may proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// The attempt may proceed.
    Allow,
    /// The attempt is refused; the value is the remaining lockout in seconds.
    Locked(u64),
}

/// Throttle bounds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ThrottleLimits {
    /// Failures tolerated before a lockout starts.
    pub(crate) max_attempts: u32,
    /// Lockout duration in seconds.
    pub(crate) lockout_secs: u64,
}

/// Two-dimensional login throttle.
pub(crate) struct LoginThrottle {
    limits: ThrottleLimits,
    by_account: Mutex<HashMap<String, Attempts>>,
    by_address: Mutex<HashMap<IpAddr, Attempts>>,
}

impl LoginThrottle {
    /// Builds an empty throttle.
    pub(crate) fn new(limits: ThrottleLimits) -> Self {
        Self {
            limits,
            by_account: Mutex::new(HashMap::new()),
            by_address: Mutex::new(HashMap::new()),
        }
    }

    /// Reports whether a login attempt may proceed.
    pub(crate) fn check(&self, account: &str, address: Option<IpAddr>, now: u64) -> Gate {
        let account_gate = gate(&mut self.by_account.lock(), &account.to_string(), now);
        if let Gate::Locked(remaining) = account_gate {
            return Gate::Locked(remaining);
        }
        match address {
            Some(address) => gate(&mut self.by_address.lock(), &address, now),
            None => Gate::Allow,
        }
    }

    /// Records a failed attempt against both dimensions.
    pub(crate) fn record_failure(&self, account: &str, address: Option<IpAddr>, now: u64) {
        record(
            &mut self.by_account.lock(),
            account.to_string(),
            now,
            &self.limits,
        );
        if let Some(address) = address {
            record(&mut self.by_address.lock(), address, now, &self.limits);
        }
    }

    /// Clears both dimensions after a successful login.
    pub(crate) fn record_success(&self, account: &str, address: Option<IpAddr>) {
        self.by_account.lock().remove(account);
        if let Some(address) = address {
            self.by_address.lock().remove(&address);
        }
    }
}

/// Reads one bucket's gate state, dropping it once the lockout has elapsed.
fn gate<K>(map: &mut HashMap<K, Attempts>, key: &K, now: u64) -> Gate
where
    K: std::hash::Hash + Eq + Clone,
{
    let Some(entry) = map.get(key) else {
        return Gate::Allow;
    };
    if entry.locked_until > now {
        return Gate::Locked(entry.locked_until - now);
    }
    if entry.locked_until != 0 {
        // The lockout elapsed: the counter starts over rather than leaving the
        // account one failure away from being locked again forever.
        map.remove(key);
    }
    Gate::Allow
}

/// Records a failure, starting a lockout once the ceiling is reached.
fn record<K>(map: &mut HashMap<K, Attempts>, key: K, now: u64, limits: &ThrottleLimits)
where
    K: std::hash::Hash + Eq + Clone,
{
    evict_cold(map, now);
    let entry = map.entry(key).or_insert(Attempts {
        failures: 0,
        locked_until: 0,
        last_seen: now,
    });
    entry.failures = entry.failures.saturating_add(1);
    entry.last_seen = now;
    if entry.failures >= limits.max_attempts {
        entry.locked_until = now.saturating_add(limits.lockout_secs);
        entry.failures = 0;
    }
}

/// Keeps the map bounded by dropping the least recently touched entries.
fn evict_cold<K>(map: &mut HashMap<K, Attempts>, now: u64)
where
    K: std::hash::Hash + Eq + Clone,
{
    if map.len() < MAX_TRACKED_KEYS {
        return;
    }
    map.retain(|_, entry| entry.locked_until > now);
    while map.len() >= MAX_TRACKED_KEYS {
        let Some(coldest) = map
            .iter()
            .min_by_key(|(_, entry)| entry.last_seen)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        map.remove(&coldest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn throttle() -> LoginThrottle {
        LoginThrottle::new(ThrottleLimits {
            max_attempts: 3,
            lockout_secs: 60,
        })
    }

    #[test]
    fn the_account_locks_after_the_configured_failures() {
        let throttle = throttle();
        let address: IpAddr = "203.0.113.5".parse().expect("ip");
        for _ in 0..2 {
            throttle.record_failure("root", Some(address), 100);
            assert_eq!(throttle.check("root", Some(address), 100), Gate::Allow);
        }
        throttle.record_failure("root", Some(address), 100);
        assert_eq!(throttle.check("root", Some(address), 100), Gate::Locked(60));
        assert_eq!(throttle.check("root", Some(address), 159), Gate::Locked(1));
        assert_eq!(throttle.check("root", Some(address), 160), Gate::Allow);
    }

    #[test]
    fn an_address_sweeping_account_names_still_locks() {
        let throttle = throttle();
        let address: IpAddr = "203.0.113.5".parse().expect("ip");
        for name in ["a", "b", "c"] {
            throttle.record_failure(name, Some(address), 100);
        }
        // No single account reached the ceiling, but the address did.
        assert_eq!(throttle.check("d", Some(address), 100), Gate::Locked(60));
    }

    #[test]
    fn a_success_clears_both_dimensions() {
        let throttle = throttle();
        let address: IpAddr = "203.0.113.5".parse().expect("ip");
        throttle.record_failure("root", Some(address), 100);
        throttle.record_failure("root", Some(address), 100);
        throttle.record_success("root", Some(address));
        throttle.record_failure("root", Some(address), 100);
        assert_eq!(throttle.check("root", Some(address), 100), Gate::Allow);
    }
}
