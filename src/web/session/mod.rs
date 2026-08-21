//! Relay session: carrier queues, logical streams, and lifecycle.
//!
//! Submodules:
//! - `state`: session state, carrier queues, pending-budget partitions
//! - `queue`: frame queueing, coalescing, and budget reservation
//! - `uplink`: client-to-relay batch validation and application
//! - `downlink`: relay-to-client long polls and batch replay
//! - `bridge`: the in-process stream endpoint handed to a backend
//! - `backend`: backend attachment for internal and loopback streams

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::{CarrierMode, WebLimits, WebTimeouts};
use crate::web::manager::{Manager, WebProfile};
use crate::web::runtime::WebRuntime;

pub(crate) mod backend;
pub(crate) mod bridge;
pub(crate) mod downlink;
pub(crate) mod queue;
pub(crate) mod state;
pub(crate) mod uplink;

use state::{BudgetLimits, SessionState};

/// Everything a session needs that is fixed for its whole lifetime.
pub(crate) struct SessionOptions {
    pub(crate) id: u64,
    pub(crate) token_hash: [u8; 32],
    pub(crate) profile: Arc<WebProfile>,
    pub(crate) client_ip: IpAddr,
    pub(crate) limits: WebLimits,
    pub(crate) timeouts: WebTimeouts,
    pub(crate) manager: Weak<Manager>,
    pub(crate) runtime: Arc<WebRuntime>,
}

/// One authenticated relay session multiplexing logical streams over a carrier.
pub(crate) struct Session {
    /// Process-unique identity used for manager bookkeeping.
    pub(crate) id: u64,
    /// Hashed session bearer, used to unregister without a pointer search.
    pub(crate) token_hash: [u8; 32],
    pub(crate) profile: Arc<WebProfile>,
    pub(crate) client_ip: IpAddr,
    pub(crate) limits: WebLimits,
    pub(crate) budget: BudgetLimits,
    pub(crate) timeouts: WebTimeouts,
    carrier: CarrierMode,
    pub(crate) manager: Weak<Manager>,
    pub(crate) runtime: Arc<WebRuntime>,
    pub(crate) state: Mutex<SessionState>,
    /// Cancelled once the session closes, tearing down every backend.
    pub(crate) done: CancellationToken,
    finished: AtomicBool,
}

impl Session {
    /// Builds a session with the profile's effective limits already applied.
    pub(crate) fn new(options: SessionOptions) -> Arc<Self> {
        let carrier = options.profile.carrier;
        let budget = BudgetLimits::new(&options.limits);
        Arc::new(Self {
            id: options.id,
            token_hash: options.token_hash,
            profile: options.profile,
            client_ip: options.client_ip,
            limits: options.limits,
            budget,
            timeouts: options.timeouts,
            carrier,
            manager: options.manager,
            runtime: options.runtime,
            state: Mutex::new(SessionState::new(
                carrier.uses_lanes(),
                carrier == CarrierMode::HttpsLanes,
            )),
            done: CancellationToken::new(),
            finished: AtomicBool::new(false),
        })
    }

    /// Carrier mode fixed at session creation.
    pub(crate) fn carrier_mode(&self) -> CarrierMode {
        self.carrier
    }

    /// True when this session keeps independent per-stream carrier lanes.
    pub(crate) fn uses_lanes(&self) -> bool {
        self.carrier.uses_lanes()
    }

    /// Timestamp of the last carrier activity, used by the reaper.
    pub(crate) fn last_activity(&self) -> Instant {
        self.state.lock().last_activity
    }

    /// Reports the closed flag to integration tests.
    #[cfg(test)]
    pub(crate) fn is_closed_for_test(&self) -> bool {
        self.state.lock().closed
    }

    /// Closes the session, every stream, and every carrier queue.
    pub(crate) fn close(&self) {
        let mut state = self.state.lock();
        self.close_locked(&mut state);
    }

    /// Acquires the single multiplexed WebSocket of a `websocket` session.
    pub(crate) fn acquire_websocket(&self) -> bool {
        let mut state = self.state.lock();
        if state.closed || self.carrier != CarrierMode::Websocket || state.main.websocket_active {
            return false;
        }
        state.main.websocket_active = true;
        true
    }

    /// Releases the multiplexed WebSocket and closes the session with it.
    pub(crate) fn release_websocket(&self) {
        {
            let mut state = self.state.lock();
            state.main.websocket_active = false;
        }
        self.close();
    }

    /// Attaches one lane socket of a `websocket-lanes` session.
    pub(crate) fn acquire_websocket_lane(&self, lane_id: u32) -> bool {
        let mut state = self.state.lock();
        if state.closed
            || self.carrier != CarrierMode::WebsocketLanes
            || lane_id == 0
            || lane_id > crate::web::frame::MAX_STREAM_ID
            || state.closed_streams.contains(&lane_id)
        {
            return false;
        }
        match state.lanes.get_mut(&lane_id) {
            Some(lane) => {
                if lane.websocket_active {
                    return false;
                }
                lane.websocket_active = true;
            }
            None => {
                if state.lanes.len() >= self.limits.max_streams_per_session {
                    return false;
                }
                let mut lane = state::LaneQueue::new();
                lane.websocket_active = true;
                state.lanes.insert(lane_id, lane);
            }
        }
        state.last_activity = Instant::now();
        true
    }

    /// Detaches a lane socket and aborts only that lane's logical stream.
    pub(crate) fn release_websocket_lane(&self, lane_id: u32) {
        let mut released = (0usize, 0usize);
        {
            let mut state = self.state.lock();
            if self.carrier != CarrierMode::WebsocketLanes {
                return;
            }
            let Some(lane) = state.lanes.get_mut(&lane_id) else {
                return;
            };
            lane.websocket_active = false;
            if let Some(mut stream) = state.streams.remove(&lane_id) {
                let (cost, items) = (stream.pending_write_cost, stream.pending_write_items);
                stream.pending_write_bytes = 0;
                stream.pending_write_cost = 0;
                stream.pending_write_items = 0;
                stream.writes.clear();
                stream.write_offset = 0;
                stream.aborted = true;
                stream.cancel.cancel();
                stream.wake_reader();
                stream.wake_writer();
                released = (cost, items);
                let evicted = state.remember_closed(lane_id, self.limits.max_closed_stream_ids);
                released.0 += evicted.0;
                released.1 += evicted.1;
            }
            if let Some(mut lane) = state.lanes.remove(&lane_id) {
                let charged = lane.charged();
                released.0 += charged.0;
                released.1 += charged.1;
                lane.clear();
            }
            state.last_activity = Instant::now();
            if released != (0, 0) {
                self.release_pending_locked(&mut state, released.0, released.1);
            }
        }
    }

    /// Tears down all session state; the caller holds the session lock.
    pub(crate) fn close_locked(&self, state: &mut SessionState) {
        if state.closed {
            return;
        }
        state.closed = true;
        self.done.cancel();
        // Everything the session still holds goes back to the process pool in
        // one step; the per-queue charges are already summed here.
        let released_cost = state.pending_cost;
        let released_items = state.pending_items;
        state.main.clear();
        for lane in state.lanes.values_mut() {
            lane.clear();
        }
        for (_, mut stream) in state.streams.drain() {
            stream.aborted = true;
            stream.cancel.cancel();
            stream.wake_reader();
            stream.wake_writer();
        }
        state.pending_cost = 0;
        state.pending_items = 0;
        if let Some(manager) = self.manager.upgrade() {
            if released_cost != 0 || released_items != 0 {
                manager.release_pending_budget(released_cost, released_items);
            }
            if !self.finished.swap(true, Ordering::AcqRel) {
                manager.session_finished(self);
            }
        }
    }

    /// Reports uplink carrier bytes to the manager metrics.
    pub(crate) fn count_up(&self, bytes: usize) {
        if let Some(manager) = self.manager.upgrade() {
            manager.count_bytes_up(bytes);
        }
    }

    /// Reports downlink carrier bytes to the manager metrics.
    pub(crate) fn count_down(&self, bytes: usize) {
        if let Some(manager) = self.manager.upgrade() {
            manager.count_bytes_down(bytes);
        }
    }

    /// Closes the session after a protocol violation on the shared carrier.
    pub(crate) fn protocol_failure(&self) {
        self.close();
    }

    /// Closes only the offending lane when the carrier isolates lanes.
    pub(crate) fn lane_protocol_failure(&self, lane_id: u32) {
        if self.carrier == CarrierMode::WebsocketLanes {
            self.release_websocket_lane(lane_id);
        } else {
            self.protocol_failure();
        }
    }
}

/// Result of a session-creation request.
pub(crate) struct CreateOutcome {
    pub(crate) token: String,
    pub(crate) welcome: Vec<u8>,
    pub(crate) session: Arc<Session>,
}
