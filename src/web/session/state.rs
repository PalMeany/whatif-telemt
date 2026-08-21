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
    pub(crate) cost: usize,
    pub(crate) items: usize,
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
    pub(crate) unacked_cost: usize,
    pub(crate) unacked_items: usize,
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
            unacked_cost: 0,
            unacked_items: 0,
            unacked_base: 0,
            down_cursor: 0,
            down_active: false,
            superseded: None,
            notify: Arc::new(Notify::new()),
        }
    }

    /// Total budget still charged to this queue, used when a lane is evicted.
    pub(crate) fn charged(&self) -> (usize, usize) {
        let mut cost = self.unacked_cost;
        let mut items = self.unacked_items;
        for queued in &self.pending_frames {
            cost += queued.cost;
            items += 1;
        }
        (cost, items)
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
        self.unacked_cost = 0;
        self.unacked_items = 0;
        self.notify.notify_waiters();
    }

    /// Moves the head of the queue into one replayable downlink batch.
    pub(crate) fn take_down_batch(&mut self, batch_bytes: usize) -> DownBatch {
        let mut size = 0usize;
        let mut cost = 0usize;
        let mut count = 0usize;
        while count < self.pending_frames.len() && count < frame::MAX_BATCH_FRAMES {
            let next = self.pending_frames[count].encoded.len();
            if count != 0 && size + next > batch_bytes {
                break;
            }
            size += next;
            cost += self.pending_frames[count].cost;
            count += 1;
        }
        let mut body = Vec::with_capacity(size);
        for (index, queued) in self.pending_frames.drain(..count).enumerate() {
            if queued.kind == FrameType::WINDOW
                && self.pending_windows.get(&queued.stream_id) == Some(&index)
            {
                self.pending_windows.remove(&queued.stream_id);
            }
            body.extend_from_slice(&queued.encoded);
        }
        for index in self.pending_windows.values_mut() {
            *index -= count;
        }
        DownBatch {
            body,
            cost,
            items: count,
        }
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
            closed: false,
            last_activity: Instant::now(),
        }
    }

    /// Records a stream id as closed and evicts the oldest tombstone.
    ///
    /// Returns the budget released by an evicted lane so the caller can hand
    /// it back to the global pool outside this borrow.
    pub(crate) fn remember_closed(&mut self, id: u32, max_closed: usize) -> (usize, usize) {
        if !self.closed_streams.insert(id) {
            return (0, 0);
        }
        let mut released = (0usize, 0usize);
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
        assert_eq!(batch.items, 1);
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
        assert_eq!(batch.items, 1);
        assert_eq!(lane.pending_windows.get(&2), Some(&0));
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
}
