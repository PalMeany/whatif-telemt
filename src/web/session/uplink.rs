//! Client-to-relay batch admission: replay detection, validation, application.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::crypto::hash::sha256;
use crate::web::error::WebError;
use crate::web::frame::{self, Frame, FrameType};
use crate::web::manager::StreamPermit;

use super::Session;
use super::state::{PendingClass, QUEUE_ITEM_COST, SessionState, StreamState};

/// A stream created by this batch, handed to the backend spawner.
pub(crate) struct OpenedStream {
    pub(crate) id: u32,
    pub(crate) permit: StreamPermit,
    pub(crate) cancel: CancellationToken,
}

/// Outcome of applying one validated batch.
struct Applied {
    opened: Vec<OpenedStream>,
    unused_cost: usize,
    unused_items: usize,
    /// False when a control frame could not be queued and the session must die.
    ok: bool,
}

impl Session {
    /// Applies one serialized uplink batch for the shared carrier.
    pub(crate) fn process_up(
        self: &Arc<Self>,
        sequence: u64,
        body: &[u8],
    ) -> Result<u64, WebError> {
        if self.uses_lanes() {
            return Err(WebError::Protocol);
        }
        let digest = sha256(body);
        {
            let mut state = self.state.lock();
            if state.closed {
                return Err(WebError::Closed);
            }
            state.last_activity = Instant::now();
            if sequence == state.main.last_up_sequence && sequence != 0 {
                let matches = state.main.last_up_digest == digest;
                drop(state);
                if !matches {
                    self.protocol_failure();
                    return Err(WebError::Protocol);
                }
                return Ok(sequence);
            }
            if sequence != state.main.last_up_sequence + 1 || sequence == 0 {
                drop(state);
                self.protocol_failure();
                return Err(WebError::Protocol);
            }
            if state.main.up_active {
                return Err(WebError::Concurrent);
            }
            state.main.up_active = true;
        }

        let parsed = parse_client_batch(body, self.limits.max_frame_payload);

        let mut state = self.state.lock();
        state.main.up_active = false;
        if state.closed {
            return Err(WebError::Closed);
        }
        let Some(frames) = parsed
            .as_ref()
            .filter(|frames| self.validate_batch(&state, frames.as_slice()))
        else {
            drop(state);
            self.protocol_failure();
            return Err(WebError::Protocol);
        };
        let (reserved_cost, reserved_items) = backend_write_reservation(&state, frames);
        if (reserved_cost != 0 || reserved_items != 0)
            && !self.reserve_pending_locked(
                &mut state,
                reserved_cost,
                reserved_items,
                PendingClass::Uplink,
            )
        {
            return Err(WebError::Backpressure);
        }
        let applied = self.apply_batch(&mut state, frames, reserved_cost, reserved_items);
        if applied.unused_cost != 0 || applied.unused_items != 0 {
            self.release_pending_locked(&mut state, applied.unused_cost, applied.unused_items);
        }
        if applied.ok {
            state.main.last_up_sequence = sequence;
            state.main.last_up_digest = digest;
        }
        drop(state);

        self.spawn_opened(applied.opened);
        if !applied.ok {
            self.close();
            return Err(WebError::Closed);
        }
        self.count_up(body.len());
        Ok(sequence)
    }

    /// Applies one uplink batch on an independent carrier lane.
    pub(crate) fn process_up_lane(
        self: &Arc<Self>,
        lane_id: u32,
        sequence: u64,
        body: &[u8],
    ) -> Result<u64, WebError> {
        if !self.uses_lanes() || lane_id > frame::MAX_STREAM_ID {
            return Err(WebError::Protocol);
        }
        let digest = sha256(body);
        let parsed = parse_client_batch(body, self.limits.max_frame_payload)
            .filter(|frames| frames.iter().all(|value| value.stream_id == lane_id));
        let Some(frames) = parsed.as_ref() else {
            self.lane_protocol_failure(lane_id);
            return Err(WebError::Protocol);
        };

        let mut state = self.state.lock();
        if state.closed {
            return Err(WebError::Closed);
        }
        if !state.lanes.contains_key(&lane_id) {
            let starts_with_open = frames.first().is_some_and(|f| f.kind == FrameType::OPEN);
            if lane_id != 0 && !starts_with_open && only_late_frames(frames) {
                // The lane tombstone was already evicted: acknowledge and drop
                // well-formed late frames instead of failing the session.
                state.last_activity = Instant::now();
                return Ok(sequence);
            }
            if lane_id == 0 || !starts_with_open {
                drop(state);
                self.lane_protocol_failure(lane_id);
                return Err(WebError::Protocol);
            }
            state.lanes.insert(lane_id, super::state::LaneQueue::new());
        }
        state.last_activity = Instant::now();
        let (last_sequence, last_digest, up_active) = {
            let lane = state.lanes.get(&lane_id).expect("lane present");
            (lane.last_up_sequence, lane.last_up_digest, lane.up_active)
        };
        if sequence == last_sequence && sequence != 0 {
            let matches = last_digest == digest;
            drop(state);
            if !matches {
                self.lane_protocol_failure(lane_id);
                return Err(WebError::Protocol);
            }
            return Ok(sequence);
        }
        if sequence != last_sequence + 1 || sequence == 0 {
            drop(state);
            self.lane_protocol_failure(lane_id);
            return Err(WebError::Protocol);
        }
        if up_active {
            return Err(WebError::Concurrent);
        }
        state
            .lanes
            .get_mut(&lane_id)
            .expect("lane present")
            .up_active = true;
        if !self.validate_batch(&state, frames) {
            state
                .lanes
                .get_mut(&lane_id)
                .expect("lane present")
                .up_active = false;
            drop(state);
            self.lane_protocol_failure(lane_id);
            return Err(WebError::Protocol);
        }
        let (reserved_cost, reserved_items) = backend_write_reservation(&state, frames);
        if (reserved_cost != 0 || reserved_items != 0)
            && !self.reserve_pending_locked(
                &mut state,
                reserved_cost,
                reserved_items,
                PendingClass::Uplink,
            )
        {
            state
                .lanes
                .get_mut(&lane_id)
                .expect("lane present")
                .up_active = false;
            return Err(WebError::Backpressure);
        }
        let applied = self.apply_batch(&mut state, frames, reserved_cost, reserved_items);
        if applied.unused_cost != 0 || applied.unused_items != 0 {
            self.release_pending_locked(&mut state, applied.unused_cost, applied.unused_items);
        }
        if let Some(lane) = state.lanes.get_mut(&lane_id) {
            lane.up_active = false;
            if applied.ok {
                lane.last_up_sequence = sequence;
                lane.last_up_digest = digest;
            }
        }
        drop(state);

        self.spawn_opened(applied.opened);
        if !applied.ok {
            self.close();
            return Err(WebError::Closed);
        }
        self.count_up(body.len());
        Ok(sequence)
    }

    /// Rejects a batch that would violate stream lifecycle or flow control.
    ///
    /// The simulation runs before any state is mutated so a batch is applied
    /// atomically or not at all.
    fn validate_batch(&self, state: &SessionState, values: &[Frame<'_>]) -> bool {
        let mut live: HashMap<u32, (u32, u64)> = HashMap::with_capacity(state.streams.len());
        for (id, stream) in &state.streams {
            live.insert(*id, (stream.receive_window, stream.send_credit));
        }
        let mut closed_in_batch: HashSet<u32> = HashSet::new();
        for value in values {
            if value.stream_id == 0 {
                if value.kind != FrameType::PONG {
                    return false;
                }
                continue;
            }
            let was_closed = state.closed_streams.contains(&value.stream_id)
                || closed_in_batch.contains(&value.stream_id);
            match value.kind {
                FrameType::OPEN => {
                    if live.contains_key(&value.stream_id) || was_closed {
                        return false;
                    }
                    live.insert(
                        value.stream_id,
                        (
                            frame::INITIAL_STREAM_WINDOW,
                            u64::from(frame::INITIAL_STREAM_WINDOW),
                        ),
                    );
                }
                FrameType::DATA => {
                    if was_closed {
                        continue;
                    }
                    let Some(entry) = live.get_mut(&value.stream_id) else {
                        return false;
                    };
                    let length = value.payload.len() as u64;
                    if length > u64::from(entry.0) {
                        return false;
                    }
                    entry.0 -= length as u32;
                }
                FrameType::WINDOW => {
                    if was_closed {
                        continue;
                    }
                    let Some(entry) = live.get_mut(&value.stream_id) else {
                        return false;
                    };
                    let amount = frame::window_amount(value.payload).unwrap_or(0);
                    entry.1 = (entry.1 + u64::from(amount)).min(u64::from(u32::MAX));
                }
                FrameType::CLOSE => {
                    if was_closed {
                        continue;
                    }
                    if live.remove(&value.stream_id).is_none() {
                        return false;
                    }
                    closed_in_batch.insert(value.stream_id);
                }
                _ => return false,
            }
        }
        true
    }

    /// Applies a validated batch to live session state.
    fn apply_batch(
        &self,
        state: &mut SessionState,
        values: &[Frame<'_>],
        mut reserved_cost: usize,
        mut reserved_items: usize,
    ) -> Applied {
        let mut opened = Vec::new();
        for value in values {
            if value.stream_id == 0 {
                continue;
            }
            let id = value.stream_id;
            let was_closed = state.closed_streams.contains(&id);
            match value.kind {
                FrameType::OPEN => {
                    let permit = if state.streams.len() >= self.limits.max_streams_per_session {
                        None
                    } else {
                        self.manager
                            .upgrade()
                            .and_then(|manager| manager.acquire_stream(&self.profile))
                    };
                    let Some(permit) = permit else {
                        let evicted = state.remember_closed(id, self.limits.max_closed_stream_ids);
                        self.release_pending_locked(state, evicted.0, evicted.1);
                        if let Some(manager) = self.manager.upgrade() {
                            manager.count_stream_rejected();
                        }
                        if !self.queue_frame_locked(state, FrameType::CLOSE, id, &[]) {
                            return Applied {
                                opened,
                                unused_cost: reserved_cost,
                                unused_items: reserved_items,
                                ok: false,
                            };
                        }
                        continue;
                    };
                    let cancel = CancellationToken::new();
                    state.streams.insert(id, StreamState::new(cancel.clone()));
                    opened.push(OpenedStream { id, permit, cancel });
                }
                FrameType::DATA => {
                    if was_closed {
                        continue;
                    }
                    let Some(stream) = state.streams.get_mut(&id) else {
                        continue;
                    };
                    let (cost, items) = append_backend_write(stream, value.payload);
                    reserved_cost = reserved_cost.saturating_sub(cost);
                    reserved_items = reserved_items.saturating_sub(items);
                    stream.receive_window -= value.payload.len() as u32;
                    stream.wake_reader();
                }
                FrameType::WINDOW => {
                    if was_closed {
                        continue;
                    }
                    let Some(stream) = state.streams.get_mut(&id) else {
                        continue;
                    };
                    let amount = frame::window_amount(value.payload).unwrap_or(0);
                    stream.send_credit =
                        (stream.send_credit + u64::from(amount)).min(u64::from(u32::MAX));
                    stream.wake_writer();
                }
                FrameType::CLOSE => {
                    if was_closed {
                        continue;
                    }
                    let Some(mut stream) = state.streams.remove(&id) else {
                        continue;
                    };
                    let released = (stream.pending_write_cost, stream.pending_write_items);
                    stream.pending_write_bytes = 0;
                    stream.pending_write_cost = 0;
                    stream.pending_write_items = 0;
                    stream.writes.clear();
                    stream.write_offset = 0;
                    stream.aborted = true;
                    stream.cancel.cancel();
                    stream.wake_reader();
                    stream.wake_writer();
                    let evicted = state.remember_closed(id, self.limits.max_closed_stream_ids);
                    self.release_pending_locked(
                        state,
                        released.0 + evicted.0,
                        released.1 + evicted.1,
                    );
                }
                _ => continue,
            }
        }
        Applied {
            opened,
            unused_cost: reserved_cost,
            unused_items: reserved_items,
            ok: true,
        }
    }
}

/// Parses and shape-validates one client batch outside the session lock.
fn parse_client_batch(body: &[u8], max_payload: usize) -> Option<Vec<Frame<'_>>> {
    let frames = frame::parse_all(body, max_payload).ok()?;
    for value in &frames {
        frame::validate_client_shape(value).ok()?;
    }
    Some(frames)
}

/// True when every frame is a late DATA, WINDOW, or CLOSE for a dead lane.
fn only_late_frames(values: &[Frame<'_>]) -> bool {
    !values.is_empty()
        && values.iter().all(|value| {
            matches!(
                value.kind,
                FrameType::DATA | FrameType::WINDOW | FrameType::CLOSE
            )
        })
}

/// Computes the uplink budget one batch will charge to backend write queues.
fn backend_write_reservation(state: &SessionState, values: &[Frame<'_>]) -> (usize, usize) {
    let mut live: HashSet<u32> = state.streams.keys().copied().collect();
    let mut cost = 0usize;
    let mut items = 0usize;
    for value in values {
        if value.stream_id == 0 {
            continue;
        }
        match value.kind {
            FrameType::OPEN => {
                live.insert(value.stream_id);
            }
            FrameType::DATA => {
                if live.contains(&value.stream_id) {
                    cost += value.payload.len() + QUEUE_ITEM_COST;
                    items += 1;
                }
            }
            FrameType::CLOSE => {
                live.remove(&value.stream_id);
            }
            _ => {}
        }
    }
    (cost, items)
}

/// Appends uplink bytes to a stream's backend queue, coalescing small writes.
///
/// Returns the budget actually consumed so the batch reservation can be
/// reconciled: a coalesced append charges bytes only, never a second item.
fn append_backend_write(stream: &mut StreamState, payload: &[u8]) -> (usize, usize) {
    let coalesce = stream
        .writes
        .back()
        .is_some_and(|last| last.len() + payload.len() <= frame::DATA_CHUNK);
    let (cost, items) = if coalesce {
        (payload.len(), 0)
    } else {
        (payload.len() + QUEUE_ITEM_COST, 1)
    };
    if coalesce {
        stream
            .writes
            .back_mut()
            .expect("tail checked above")
            .extend_from_slice(payload);
    } else {
        stream.writes.push_back(payload.to_vec());
    }
    stream.pending_write_bytes += payload.len();
    stream.pending_write_cost += cost;
    stream.pending_write_items += items;
    (cost, items)
}
