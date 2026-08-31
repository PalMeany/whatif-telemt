//! Rate limiting, process-wide pending budget, and stream accounting permits.

use std::sync::{Arc, Weak};
use std::time::Instant;

use crate::config::fork::web::WebLimits;
use crate::fork::web::session::state::{PendingClass, control_reserve};

use super::{Manager, WebProfile};

/// Token-bucket state for one rate-limited operation.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RateState {
    tokens: f64,
    last: Option<Instant>,
}

impl RateState {
    /// Computes the state after one token request without committing it.
    fn take(self, now: Instant, per_minute: usize, burst: usize) -> (RateState, bool) {
        let burst = burst as f64;
        let mut state = self;
        match state.last {
            None => {
                state.tokens = burst;
                state.last = Some(now);
            }
            Some(last) => {
                let per_second = per_minute as f64 / 60.0;
                let elapsed = now.saturating_duration_since(last).as_secs_f64();
                state.tokens = (state.tokens + elapsed * per_second).min(burst);
                state.last = Some(now);
            }
        }
        if state.tokens < 1.0 {
            return (state, false);
        }
        state.tokens -= 1.0;
        (state, true)
    }
}

/// Applies one global bucket, committing only when it allows the operation.
pub(crate) fn allow_rate(
    state: &mut RateState,
    now: Instant,
    per_minute: usize,
    burst: usize,
) -> bool {
    let (updated, allowed) = state.take(now, per_minute, burst);
    *state = updated;
    allowed
}

/// Applies the global and per-profile buckets atomically.
///
/// Neither bucket is charged unless both allow the operation, so a rejected
/// request cannot silently drain the other bucket.
#[allow(clippy::too_many_arguments)]
pub(crate) fn allow_profile_rate(
    global: &mut RateState,
    profile_state: &mut RateState,
    now: Instant,
    global_per_minute: usize,
    global_burst: usize,
    profile_per_minute: usize,
    profile_burst: usize,
) -> bool {
    let (updated_global, global_allowed) = global.take(now, global_per_minute, global_burst);
    let (updated_profile, profile_allowed) =
        profile_state.take(now, profile_per_minute, profile_burst);
    if !global_allowed || !profile_allowed {
        return false;
    }
    *global = updated_global;
    *profile_state = updated_profile;
    true
}

/// Process-wide queued byte and item accounting.
pub(crate) struct GlobalPending {
    cost: usize,
    items: usize,
    control_cost_limit: usize,
    control_item_limit: usize,
    data_cost_limit: usize,
    data_item_limit: usize,
}

impl GlobalPending {
    /// Precomputes the control and data partitions of the global pool.
    pub(crate) fn new(limits: &WebLimits) -> Self {
        let (reserve_cost, reserve_items) = control_reserve(limits);
        let sessions = limits.max_sessions_global.max(1);
        let data_cost_limit = if reserve_cost > limits.max_pending_global / sessions {
            0
        } else {
            limits.max_pending_global - reserve_cost * sessions
        };
        let data_item_limit = if reserve_items > limits.max_pending_items_global / sessions {
            0
        } else {
            limits.max_pending_items_global - reserve_items * sessions
        };
        Self {
            cost: 0,
            items: 0,
            control_cost_limit: limits.max_pending_global,
            control_item_limit: limits.max_pending_items_global,
            data_cost_limit,
            data_item_limit,
        }
    }

    /// True when the control reserve alone would starve every data frame.
    pub(crate) fn data_partition_empty(&self) -> bool {
        self.data_cost_limit == 0 || self.data_item_limit == 0
    }

    /// Charges the pool, refusing the whole reservation if either half fails.
    pub(crate) fn reserve(&mut self, cost: usize, items: usize, class: PendingClass) -> bool {
        let (cost_limit, item_limit) = if class == PendingClass::Control {
            (self.control_cost_limit, self.control_item_limit)
        } else {
            (self.data_cost_limit, self.data_item_limit)
        };
        if cost > cost_limit || self.cost > cost_limit - cost {
            return false;
        }
        if items > item_limit || self.items > item_limit - items {
            return false;
        }
        self.cost += cost;
        self.items += items;
        true
    }

    /// Returns budget to the pool.
    pub(crate) fn release(&mut self, cost: usize, items: usize) {
        debug_assert!(cost <= self.cost && items <= self.items);
        self.cost = self.cost.saturating_sub(cost);
        self.items = self.items.saturating_sub(items);
    }

    /// Current charge, reported by the capacity endpoint.
    pub(crate) fn usage(&self) -> (usize, usize) {
        (self.cost, self.items)
    }
}

/// Live-stream accounting held for a stream's whole lifetime.
///
/// Dropping the permit releases the stream slot and, if the backend never
/// reported a dial outcome, the in-flight dial slot as well.
pub(crate) struct StreamPermit {
    manager: Weak<Manager>,
    profile: Arc<WebProfile>,
    dial_pending: bool,
}

impl StreamPermit {
    pub(crate) fn new(manager: Weak<Manager>, profile: Arc<WebProfile>) -> Self {
        Self {
            manager,
            profile,
            dial_pending: true,
        }
    }

    /// Reports the backend dial outcome exactly once.
    pub(crate) fn dial_finished(&mut self, failed: bool) {
        if !self.dial_pending {
            return;
        }
        self.dial_pending = false;
        if let Some(manager) = self.manager.upgrade() {
            manager.backend_dial_finished(&self.profile, failed);
        }
    }
}

impl Drop for StreamPermit {
    fn drop(&mut self) {
        self.dial_finished(false);
        if let Some(manager) = self.manager.upgrade() {
            manager.stream_finished(&self.profile);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn burst_is_available_immediately_then_refills() {
        let mut state = RateState::default();
        let start = Instant::now();
        assert!(allow_rate(&mut state, start, 60, 2));
        assert!(allow_rate(&mut state, start, 60, 2));
        assert!(!allow_rate(&mut state, start, 60, 2));
        let later = start + Duration::from_secs(1);
        assert!(allow_rate(&mut state, later, 60, 2));
    }

    #[test]
    fn profile_rate_does_not_charge_global_when_profile_denies() {
        let mut global = RateState::default();
        let mut profile = RateState::default();
        let now = Instant::now();
        assert!(allow_profile_rate(
            &mut global,
            &mut profile,
            now,
            60,
            8,
            60,
            1
        ));
        assert!(!allow_profile_rate(
            &mut global,
            &mut profile,
            now,
            60,
            8,
            60,
            1
        ));
        // The global bucket kept its remaining tokens for other profiles.
        let mut other = RateState::default();
        assert!(allow_profile_rate(
            &mut global,
            &mut other,
            now,
            60,
            8,
            60,
            1
        ));
    }

    #[test]
    fn global_pending_refuses_over_limit_reservations() {
        let limits = WebLimits::default();
        let mut pending = GlobalPending::new(&limits);
        assert!(!pending.data_partition_empty());
        assert!(pending.reserve(1024, 1, PendingClass::Downlink));
        assert!(!pending.reserve(usize::MAX, 1, PendingClass::Downlink));
        pending.release(1024, 1);
        assert_eq!(pending.usage(), (0, 0));
    }
}
