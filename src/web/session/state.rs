//! Session state, carrier queues, and pending-budget accounting.
//!
//! One `LaneQueue` serves both the single session-wide queue used by the
//! `https`/`websocket` carriers and the per-stream queues used by the lane
//! carriers, so queue semantics cannot drift between the two shapes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::task::Waker;
use std::time::Instant;

use bytes::Bytes;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::config::WebLimits;
use crate::web::frame::{self, FrameType, HEADER_SIZE};

/// Conservative per-item queue charge covering allocation and bookkeeping.
pub(crate) const QUEUE_ITEM_COST: usize = 256;

/// Control-frame reserve floor independent of the stream count.
const CONTROL_RESERVE_EXTRA_ITEMS: usize = 16;

/// Control frames reserved per live stream (WINDOW, CLOSE, and one spare).
const CONTROL_RESERVE_ITEMS_PER_STREAM: usize = 3;

/// Which partition of the pending budget a reservation belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingClass {
    /// Client bytes queued toward a backend.
    Uplink,
    /// Backend bytes queued toward the client.
    Downlink,
    /// WINDOW, CLOSE, and session control frames.
    Control,
}

impl PendingClass {
    /// Classifies one queued frame by the partition it is charged to.
    pub(crate) fn of_frame(kind: FrameType) -> Self {
        if kind == FrameType::DATA {
            PendingClass::Downlink
        } else {
            PendingClass::Control
        }
    }
}

/// One pending-budget charge together with its control-class part.
///
/// A queue holds frames of both classes, so releasing it has to hand each part
/// back to the partition it came from. Carrying the split with the charge is
/// what keeps the control reserve from drifting: every release site is forced
/// to say how much of it was control.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PendingCharge {
    /// Total queued bytes charged.
    pub(crate) cost: usize,
    /// Total queued items charged.
    pub(crate) items: usize,
    /// Part of `cost` charged to the control partition.
    pub(crate) control_cost: usize,
    /// Part of `items` charged to the control partition.
    pub(crate) control_items: usize,
}

impl PendingCharge {
    /// A charge that belongs entirely to an uplink or downlink partition.
    pub(crate) fn data(cost: usize, items: usize) -> Self {
        Self {
            cost,
            items,
            control_cost: 0,
            control_items: 0,
        }
    }

    /// A charge that belongs entirely to the control reserve.
    pub(crate) fn control(cost: usize, items: usize) -> Self {
        Self {
            cost,
            items,
            control_cost: cost,
            control_items: items,
        }
    }

    /// A charge classified by the frame type that produced it.
    pub(crate) fn of_frame(kind: FrameType, cost: usize, items: usize) -> Self {
        match PendingClass::of_frame(kind) {
            PendingClass::Control => Self::control(cost, items),
            _ => Self::data(cost, items),
        }
    }

    /// Accumulates another charge into this one.
    pub(crate) fn add(&mut self, other: Self) {
        self.cost += other.cost;
        self.items += other.items;
        self.control_cost += other.control_cost;
        self.control_items += other.control_items;
    }

    /// True when nothing is charged at all.
    pub(crate) fn is_empty(&self) -> bool {
        self.cost == 0 && self.items == 0
    }
}

/// One encoded frame waiting in a carrier queue.
pub(crate) struct QueuedFrame {
    pub(crate) encoded: Vec<u8>,
    pub(crate) kind: FrameType,
    pub(crate) stream_id: u32,
    pub(crate) cost: usize,
}

/// A downlink batch handed to one carrier response.
pub(crate) struct DownBatch {
    pub(crate) body: Vec<u8>,
    pub(crate) charge: PendingCharge,
}

/// Carrier queue plus its uplink replay and downlink acknowledgement state.
pub(crate) struct LaneQueue {
    pub(crate) last_up_sequence: u64,
    pub(crate) last_up_digest: [u8; 32],
    pub(crate) up_active: bool,
    pub(crate) websocket_active: bool,
    pub(crate) pending_frames: Vec<QueuedFrame>,
    pub(crate) pending_windows: HashMap<u32, usize>,
    pub(crate) unacked: Bytes,
    /// Budget still charged for the unacknowledged batch, split by class.
    pub(crate) unacked_charge: PendingCharge,
    pub(crate) unacked_base: u64,
    pub(crate) down_cursor: u64,
    pub(crate) down_active: bool,
    /// Wakes the currently parked poll when a newer poll takes over.
    pub(crate) superseded: Option<Arc<Notify>>,
    /// Wakes the parked poll when frames arrive or the lane disappears.
    pub(crate) notify: Arc<Notify>,
}

impl LaneQueue {
    pub(crate) fn new() -> Self {
        Self {
            last_up_sequence: 0,
            last_up_digest: [0u8; 32],
            up_active: false,
            websocket_active: false,
            pending_frames: Vec::new(),
            pending_windows: HashMap::new(),
            unacked: Bytes::new(),
            unacked_charge: PendingCharge::default(),
            unacked_base: 0,
            down_cursor: 0,
            down_active: false,
            superseded: None,
            notify: Arc::new(Notify::new()),
        }
    }

    /// Total budget still charged to this queue, used when a lane is evicted.
    pub(crate) fn charged(&self) -> PendingCharge {
        let mut charge = self.unacked_charge;
        for queued in &self.pending_frames {
            charge.add(PendingCharge::of_frame(queued.kind, queued.cost, 1));
        }
        charge
    }

    /// Drops every queued and unacknowledged byte without releasing budget.
    ///
    /// The caller has already accounted the released budget through
    /// [`LaneQueue::charged`]; parked pollers observe the change through the
    /// lane notification.
    pub(crate) fn clear(&mut self) {
        self.pending_frames = Vec::new();
        self.pending_windows = HashMap::new();
        self.unacked = Bytes::new();
        self.unacked_charge = PendingCharge::default();
        self.notify.notify_waiters();
    }

    /// Moves the head of the queue into one replayable downlink batch.
    pub(crate) fn take_down_batch(&mut self, batch_bytes: usize) -> DownBatch {
        let mut size = 0usize;
        let mut count = 0usize;
        while count < self.pending_frames.len() && count < frame::MAX_BATCH_FRAMES {
            let next = self.pending_frames[count].encoded.len();
            if count != 0 && size + next > batch_bytes {
                break;
            }
            size += next;
            count += 1;
        }
        let mut body = Vec::with_capacity(size);
        let mut charge = PendingCharge::default();
        for (index, queued) in self.pending_frames.drain(..count).enumerate() {
            if queued.kind == FrameType::WINDOW
                && self.pending_windows.get(&queued.stream_id) == Some(&index)
            {
                self.pending_windows.remove(&queued.stream_id);
            }
            charge.add(PendingCharge::of_frame(queued.kind, queued.cost, 1));
            body.extend_from_slice(&queued.encoded);
        }
        for index in self.pending_windows.values_mut() {
            *index -= count;
        }
        DownBatch { body, charge }
    }
}

/// One live logical stream and its backend-facing queues.
pub(crate) struct StreamState {
    /// Credit the relay has granted the client for uplink DATA.
    pub(crate) receive_window: u32,
    /// Credit the client has granted the relay for downlink DATA.
    pub(crate) send_credit: u64,
    pub(crate) pending_write_bytes: usize,
    pub(crate) pending_write_cost: usize,
    pub(crate) pending_write_items: usize,
    /// Uplink payloads queued for the backend, oldest first.
    pub(crate) writes: VecDeque<Vec<u8>>,
    /// Bytes of `writes.front()` already handed to the backend.
    pub(crate) write_offset: usize,
    /// Waker of the backend task waiting for uplink bytes.
    pub(crate) read_waker: Option<Waker>,
    /// Waker of the backend task waiting for downlink credit or budget.
    pub(crate) write_waker: Option<Waker>,
    /// Cancels the backend task when the stream is torn down.
    pub(crate) cancel: CancellationToken,
    /// Set once the client sent CLOSE or the session dropped the stream.
    pub(crate) aborted: bool,
}

impl StreamState {
    pub(crate) fn new(cancel: CancellationToken) -> Self {
        Self {
            receive_window: frame::INITIAL_STREAM_WINDOW,
            send_credit: u64::from(frame::INITIAL_STREAM_WINDOW),
            pending_write_bytes: 0,
            pending_write_cost: 0,
            pending_write_items: 0,
            writes: VecDeque::new(),
            write_offset: 0,
            read_waker: None,
            write_waker: None,
            cancel,
            aborted: false,
        }
    }

    /// Wakes the backend reader after new uplink bytes were queued.
    pub(crate) fn wake_reader(&mut self) {
        if let Some(waker) = self.read_waker.take() {
            waker.wake();
        }
    }

    /// Wakes the backend writer after credit or budget became available.
    pub(crate) fn wake_writer(&mut self) {
        if let Some(waker) = self.write_waker.take() {
            waker.wake();
        }
    }
}

/// Mutable session state guarded by one lock.
pub(crate) struct SessionState {
    pub(crate) streams: HashMap<u32, StreamState>,
    pub(crate) closed_streams: HashSet<u32>,
    pub(crate) closed_order: Vec<u32>,
    pub(crate) closed_start: usize,
    /// Queue used by the non-lane carriers.
    pub(crate) main: LaneQueue,
    /// Per-stream queues used by the lane carriers.
    pub(crate) lanes: HashMap<u32, LaneQueue>,
    pub(crate) pending_cost: usize,
    pub(crate) pending_items: usize,
    /// Part of `pending_cost` charged to the control reserve.
    pub(crate) control_cost: usize,
    /// Part of `pending_items` charged to the control reserve.
    pub(crate) control_items: usize,
    pub(crate) closed: bool,
    pub(crate) last_activity: Instant,
}

impl SessionState {
    pub(crate) fn new(lanes_enabled: bool, https_lanes: bool) -> Self {
        let mut lanes = HashMap::new();
        if lanes_enabled && https_lanes {
            // Lane zero carries session-level PONG traffic in `https-lanes`.
            lanes.insert(0u32, LaneQueue::new());
        }
        Self {
            streams: HashMap::new(),
            closed_streams: HashSet::new(),
            closed_order: Vec::new(),
            closed_start: 0,
            main: LaneQueue::new(),
            lanes,
            pending_cost: 0,
            pending_items: 0,
            control_cost: 0,
            control_items: 0,
            closed: false,
            last_activity: Instant::now(),
        }
    }

    /// Records a stream id as closed and evicts the oldest tombstone.
    ///
    /// Returns the budget released by an evicted lane so the caller can hand
    /// it back to the global pool outside this borrow.
    pub(crate) fn remember_closed(&mut self, id: u32, max_closed: usize) -> PendingCharge {
        if !self.closed_streams.insert(id) {
            return PendingCharge::default();
        }
        let mut released = PendingCharge::default();
        if self.closed_order.len() < max_closed {
            self.closed_order.push(id);
        } else {
            let old = self.closed_order[self.closed_start];
            self.closed_streams.remove(&old);
            if let Some(mut lane) = self.lanes.remove(&old) {
                released = lane.charged();
                lane.clear();
            }
            self.closed_order[self.closed_start] = id;
            self.closed_start = (self.closed_start + 1) % self.closed_order.len();
        }
        if let Some(lane) = self.lanes.get(&id) {
            lane.notify.notify_waiters();
        }
        released
    }

    /// Frees the oldest lane whose stream is already gone.
    ///
    /// Lane carriers keep one queue per stream id, and a closed lane survives
    /// until its tombstone is evicted, which is thousands of closes away. A
    /// client that opens and abandons lanes faster than it drains them would
    /// otherwise hold one queue per id it ever used. Only tombstoned lanes are
    /// eligible, so a live stream never loses its carrier queue.
    pub(crate) fn evict_closed_lane(&mut self) -> Option<PendingCharge> {
        let count = self.closed_order.len();
        for offset in 0..count {
            let id = self.closed_order[(self.closed_start + offset) % count];
            if id == 0 || self.streams.contains_key(&id) || !self.lanes.contains_key(&id) {
                continue;
            }
            // A closed lane still holding frames has not finished delivering:
            // the bridge polls it until the relay answers `X-Lane-Closed`, and
            // dropping the queue underneath it loses bytes the client was told
            // to expect. Skip it and take an older, fully drained one.
            let drainable = self
                .lanes
                .get(&id)
                .is_some_and(|lane| lane.pending_frames.is_empty() && lane.unacked.is_empty());
            if !drainable {
                continue;
            }
            let mut lane = self.lanes.remove(&id).expect("lane presence checked");
            let released = lane.charged();
            lane.clear();
            return Some(released);
        }
        None
    }
}

/// Per-session budget partitions derived from the effective limits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BudgetLimits {
    pub(crate) session_cost: usize,
    pub(crate) session_items: usize,
    pub(crate) uplink_cost: usize,
    pub(crate) uplink_items: usize,
    pub(crate) downlink_cost: usize,
    pub(crate) downlink_items: usize,
}

impl BudgetLimits {
    /// Precomputes the three partitions once per session.
    pub(crate) fn new(limits: &WebLimits) -> Self {
        let (reserve_cost, reserve_items) = control_reserve(limits);
        let (uplink_cost, uplink_items) = subtract_reserve(
            limits.max_pending_per_session,
            limits.max_pending_items_per_session,
            reserve_cost,
            reserve_items,
        );
        let (body_cost, body_items) = uplink_reserve(limits);
        let (downlink_cost, downlink_items) =
            subtract_reserve(uplink_cost, uplink_items, body_cost, body_items);
        Self {
            session_cost: limits.max_pending_per_session,
            session_items: limits.max_pending_items_per_session,
            uplink_cost,
            uplink_items,
            downlink_cost,
            downlink_items,
        }
    }

    /// Returns the cost and item ceiling for one reservation class.
    ///
    /// The control reserve is a *floor*, not a cap: it is subtracted from the
    /// two data partitions so a saturated stream can never make a CLOSE or a
    /// WINDOW unqueueable, but a control frame itself is bounded by the whole
    /// session pool, exactly as the reference bounds it. Capping control at the
    /// reserve instead would turn an ordinary burst — one legal uplink batch of
    /// OPENs whose stream-limit CLOSEs exceed the reserve — into a session
    /// kill, which `PROTOCOL.md` forbids: an over-limit stream receives CLOSE
    /// and the authenticated session and its other streams survive.
    ///
    /// The process-wide split stays sound without a per-session control cap:
    /// [`GlobalPending`] bounds the data classes by
    /// `max_pending_global - control_reserve * max_sessions_global` while
    /// leaving the control class the full pool, so the reserved headroom is
    /// reachable only by control frames no matter which session queues them.
    ///
    /// [`GlobalPending`]: crate::web::manager::limits::GlobalPending
    pub(crate) fn for_class(&self, class: PendingClass) -> (usize, usize) {
        match class {
            PendingClass::Control => (self.session_cost, self.session_items),
            PendingClass::Uplink => (self.uplink_cost, self.uplink_items),
            PendingClass::Downlink => (self.downlink_cost, self.downlink_items),
        }
    }
}

/// Control-frame reserve that keeps WINDOW, CLOSE, and session frames sendable
/// even when the data partitions are saturated.
pub(crate) fn control_reserve(limits: &WebLimits) -> (usize, usize) {
    let per_stream = limits
        .max_streams_per_session
        .saturating_mul(CONTROL_RESERVE_ITEMS_PER_STREAM);
    let items = per_stream.saturating_add(CONTROL_RESERVE_EXTRA_ITEMS);
    let cost_per_item = QUEUE_ITEM_COST + HEADER_SIZE + 4;
    match items.checked_mul(cost_per_item) {
        Some(cost) if per_stream != usize::MAX => (cost, items),
        // Degenerate configuration: reserve the whole per-session budget so no
        // data frame is admitted rather than silently overflowing.
        _ => (
            limits.max_pending_per_session,
            limits.max_pending_items_per_session,
        ),
    }
}

/// Headroom kept for one maximum uplink batch so a downlink burst can never
/// starve the uplink direction.
fn uplink_reserve(limits: &WebLimits) -> (usize, usize) {
    let mut items = limits.max_body_bytes / HEADER_SIZE;
    if items > frame::MAX_BATCH_FRAMES {
        items = frame::MAX_BATCH_FRAMES;
    }
    match items
        .checked_mul(QUEUE_ITEM_COST)
        .and_then(|extra| limits.max_body_bytes.checked_add(extra))
    {
        Some(cost) => (cost, items),
        _ => (
            limits.max_pending_per_session,
            limits.max_pending_items_per_session,
        ),
    }
}

fn subtract_reserve(
    cost: usize,
    items: usize,
    reserve_cost: usize,
    reserve_items: usize,
) -> (usize, usize) {
    (
        cost.saturating_sub(reserve_cost),
        items.saturating_sub(reserve_items),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::frame::encode;

    fn queued(kind: FrameType, id: u32, payload: &[u8]) -> QueuedFrame {
        let encoded = encode(kind, id, payload);
        let cost = encoded.len() + QUEUE_ITEM_COST;
        QueuedFrame {
            encoded,
            kind,
            stream_id: id,
            cost,
        }
    }

    #[test]
    fn down_batch_respects_byte_ceiling_but_always_takes_one() {
        let mut lane = LaneQueue::new();
        lane.pending_frames
            .push(queued(FrameType::DATA, 1, &vec![0u8; 4096]));
        lane.pending_frames
            .push(queued(FrameType::DATA, 1, &vec![0u8; 4096]));
        let batch = lane.take_down_batch(1024);
        assert_eq!(batch.charge.items, 1);
        assert_eq!(batch.charge.control_items, 0);
        assert_eq!(batch.body.len(), HEADER_SIZE + 4096);
        assert_eq!(lane.pending_frames.len(), 1);
    }

    #[test]
    fn down_batch_reindexes_pending_windows() {
        let mut lane = LaneQueue::new();
        lane.pending_frames.push(queued(FrameType::DATA, 1, b"a"));
        lane.pending_frames
            .push(queued(FrameType::WINDOW, 2, &[0, 0, 0, 1]));
        lane.pending_windows.insert(2, 1);
        let batch = lane.take_down_batch(HEADER_SIZE + 1);
        assert_eq!(batch.charge.items, 1);
        assert_eq!(lane.pending_windows.get(&2), Some(&0));
    }

    #[test]
    fn down_batch_splits_the_control_part_of_its_charge() {
        let mut lane = LaneQueue::new();
        lane.pending_frames.push(queued(FrameType::DATA, 1, b"ab"));
        lane.pending_frames.push(queued(FrameType::CLOSE, 1, &[]));
        let batch = lane.take_down_batch(usize::MAX);
        assert_eq!(batch.charge.items, 2);
        assert_eq!(batch.charge.control_items, 1);
        assert_eq!(batch.charge.control_cost, HEADER_SIZE + QUEUE_ITEM_COST);
        assert_eq!(
            batch.charge.cost,
            batch.charge.control_cost + HEADER_SIZE + 2 + QUEUE_ITEM_COST
        );
    }

    #[test]
    fn closed_lanes_are_evicted_before_live_ones() {
        let mut state = SessionState::new(true, false);
        state.lanes.insert(1, LaneQueue::new());
        state.lanes.insert(2, LaneQueue::new());
        state
            .streams
            .insert(1, StreamState::new(CancellationToken::new()));
        state.remember_closed(2, 16);
        assert!(state.evict_closed_lane().is_some());
        assert!(!state.lanes.contains_key(&2));
        assert!(state.lanes.contains_key(&1));
        // Nothing else is tombstoned, so a live lane is never taken.
        assert!(state.evict_closed_lane().is_none());
    }

    #[test]
    fn tombstone_ring_evicts_oldest_id() {
        let mut state = SessionState::new(false, false);
        state.remember_closed(1, 2);
        state.remember_closed(2, 2);
        state.remember_closed(3, 2);
        assert!(!state.closed_streams.contains(&1));
        assert!(state.closed_streams.contains(&2));
        assert!(state.closed_streams.contains(&3));
    }

    #[test]
    fn budget_partitions_are_ordered() {
        let limits = WebLimits::default();
        let budget = BudgetLimits::new(&limits);
        assert!(budget.downlink_cost < budget.uplink_cost);
        assert!(budget.uplink_cost < budget.session_cost);
        assert!(budget.downlink_items < budget.uplink_items);
    }

    #[test]
    fn the_control_reserve_is_a_floor_not_a_cap() {
        let limits = WebLimits::default();
        let budget = BudgetLimits::new(&limits);
        let (reserve_cost, reserve_items) = control_reserve(&limits);

        // A control frame may use the whole session pool, exactly as the
        // reference relay allows. Capping it at the reserve would make one
        // legal burst of OPENs kill the session on its own stream-limit
        // CLOSEs, which `PROTOCOL.md` forbids.
        assert_eq!(
            budget.for_class(PendingClass::Control),
            (budget.session_cost, budget.session_items)
        );

        // The reserve does its work by being subtracted from the data
        // partitions, so a saturated stream can never leave a CLOSE or a
        // WINDOW unqueueable.
        assert!(budget.uplink_cost + reserve_cost <= budget.session_cost);
        assert!(budget.uplink_items + reserve_items <= budget.session_items);
        assert!(reserve_cost > 0 && reserve_items > 0);
    }
}
