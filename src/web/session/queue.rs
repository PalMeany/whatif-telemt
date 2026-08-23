//! Downlink frame queueing, coalescing, and pending-budget reservation.

use crate::web::frame::{self, FrameType, HEADER_SIZE};

use super::Session;
use super::state::{PendingCharge, PendingClass, QUEUE_ITEM_COST, QueuedFrame, SessionState};

impl Session {
    /// Reserves session and process budget for one queued item.
    ///
    /// Mirrors the reference relay: a reservation must fit its class partition
    /// *and* the process-wide pool, and no partial charge is ever left behind.
    ///
    /// Every class is measured against the session-wide running total. The
    /// control reserve does its work on the other side of the split — it is
    /// subtracted from the two data partitions in [`BudgetLimits::new`], so a
    /// saturated data partition still leaves room for a CLOSE or a WINDOW —
    /// while a control frame itself may use the whole session pool. Bounding
    /// control by the reserve instead would make one legal burst of OPENs kill
    /// the session on its own stream-limit CLOSEs.
    ///
    /// [`BudgetLimits::new`]: super::state::BudgetLimits::new
    pub(crate) fn reserve_pending_locked(
        &self,
        state: &mut SessionState,
        cost: usize,
        items: usize,
        class: PendingClass,
    ) -> bool {
        let (cost_limit, item_limit) = self.budget.for_class(class);
        let (used_cost, used_items) = (state.pending_cost, state.pending_items);
        if cost == 0
            || cost > cost_limit
            || items > item_limit
            || used_cost > cost_limit - cost
            || used_items > item_limit - items
        {
            return false;
        }
        // A data partition already fits inside the session pool, so this can
        // only fire if the partitions are ever resized apart; for the control
        // class it is the same check twice. It is checked rather than assumed,
        // because the process-wide split into `control_reserve *
        // max_sessions_global` plus the rest is built on the session pool being
        // an actual ceiling.
        if state.pending_cost > self.budget.session_cost.saturating_sub(cost)
            || state.pending_items > self.budget.session_items.saturating_sub(items)
        {
            return false;
        }
        let Some(manager) = self.manager.upgrade() else {
            return false;
        };
        if !manager.reserve_pending_budget(cost, items, class) {
            return false;
        }
        state.pending_cost += cost;
        state.pending_items += items;
        if class == PendingClass::Control {
            state.control_cost += cost;
            state.control_items += items;
        }
        true
    }

    /// Returns budget to the session and process pools and wakes backends that
    /// were parked waiting for downlink headroom.
    pub(crate) fn release_pending_locked(&self, state: &mut SessionState, charge: PendingCharge) {
        if charge.is_empty() {
            return;
        }
        debug_assert!(charge.cost <= state.pending_cost && charge.items <= state.pending_items);
        debug_assert!(
            charge.control_cost <= state.control_cost
                && charge.control_items <= state.control_items
        );
        state.pending_cost = state.pending_cost.saturating_sub(charge.cost);
        state.pending_items = state.pending_items.saturating_sub(charge.items);
        state.control_cost = state.control_cost.saturating_sub(charge.control_cost);
        state.control_items = state.control_items.saturating_sub(charge.control_items);
        if let Some(manager) = self.manager.upgrade() {
            manager.release_pending_budget(charge.cost, charge.items);
        }
        for stream in state.streams.values_mut() {
            stream.wake_writer();
        }
    }

    /// Largest DATA payload that currently fits the downlink partition.
    pub(crate) fn data_frame_allowance_locked(&self, state: &SessionState, limit: usize) -> usize {
        let (cost_limit, item_limit) = self.budget.for_class(PendingClass::Downlink);
        if state.pending_items >= item_limit {
            return 0;
        }
        let available = cost_limit
            .saturating_sub(state.pending_cost)
            .saturating_sub(QUEUE_ITEM_COST)
            .saturating_sub(HEADER_SIZE);
        limit.min(available)
    }

    /// Queues one relay-to-client frame on the carrier queue that owns `id`.
    ///
    /// Returns `false` when the queue budget refuses the frame; the caller
    /// treats that as a fatal condition for the stream or session, exactly
    /// like the reference relay.
    pub(crate) fn queue_frame_locked(
        &self,
        state: &mut SessionState,
        kind: FrameType,
        id: u32,
        payload: &[u8],
    ) -> bool {
        let lane_key = if self.uses_lanes() { Some(id) } else { None };
        if let Some(lane_id) = lane_key
            && !state.lanes.contains_key(&lane_id)
        {
            return false;
        }

        // WINDOW grants for one stream coalesce in place and cost nothing new.
        if kind == FrameType::WINDOW {
            let coalesced = {
                let queue = queue_mut(state, lane_key);
                match queue.pending_windows.get(&id).copied() {
                    Some(index) => {
                        let queued = &mut queue.pending_frames[index];
                        let previous =
                            frame::window_amount(&queued.encoded[HEADER_SIZE..]).unwrap_or(0);
                        let amount = frame::window_amount(payload).unwrap_or(0);
                        let total = u64::from(previous) + u64::from(amount);
                        if total <= u64::from(u32::MAX) {
                            queued.encoded[HEADER_SIZE..HEADER_SIZE + 4]
                                .copy_from_slice(&(total as u32).to_be_bytes());
                            queue.notify.notify_waiters();
                            true
                        } else {
                            false
                        }
                    }
                    None => false,
                }
            };
            if coalesced {
                return true;
            }
        }

        // Adjacent DATA for one stream coalesces into the tail frame, charging
        // only the extra bytes rather than a second queue item.
        let coalesce_data = {
            let queue = queue_ref(state, lane_key);
            match queue.pending_frames.last() {
                Some(last) => {
                    last.kind == FrameType::DATA
                        && kind == FrameType::DATA
                        && last.stream_id == id
                        && last.encoded.len() - HEADER_SIZE + payload.len()
                            <= self.limits.max_frame_payload
                }
                None => false,
            }
        };
        if coalesce_data {
            if !self.reserve_pending_locked(state, payload.len(), 0, PendingClass::Downlink) {
                return false;
            }
            let queue = queue_mut(state, lane_key);
            let last = queue
                .pending_frames
                .last_mut()
                .expect("tail frame checked above");
            last.encoded.extend_from_slice(payload);
            last.cost += payload.len();
            frame::patch_length(&mut last.encoded);
            queue.notify.notify_waiters();
            return true;
        }

        let encoded = frame::encode(kind, id, payload);
        let cost = encoded.len() + QUEUE_ITEM_COST;
        let class = PendingClass::of_frame(kind);
        if !self.reserve_pending_locked(state, cost, 1, class) {
            return false;
        }
        let queue = queue_mut(state, lane_key);
        queue.pending_frames.push(QueuedFrame {
            encoded,
            kind,
            stream_id: id,
            cost,
        });
        if kind == FrameType::WINDOW {
            let index = queue.pending_frames.len() - 1;
            queue.pending_windows.insert(id, index);
        }
        queue.notify.notify_waiters();
        true
    }
}

/// Immutable access to the carrier queue that owns a stream id.
fn queue_ref(state: &SessionState, lane: Option<u32>) -> &super::state::LaneQueue {
    match lane {
        Some(lane_id) => state.lanes.get(&lane_id).expect("lane presence checked"),
        None => &state.main,
    }
}

/// Mutable access to the carrier queue that owns a stream id.
fn queue_mut(state: &mut SessionState, lane: Option<u32>) -> &mut super::state::LaneQueue {
    match lane {
        Some(lane_id) => state
            .lanes
            .get_mut(&lane_id)
            .expect("lane presence checked"),
        None => &mut state.main,
    }
}
