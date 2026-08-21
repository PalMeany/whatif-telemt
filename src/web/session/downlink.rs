//! Relay-to-client downlink polling with newest-poll-wins parking.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::Notify;

use crate::web::error::WebError;

use super::Session;
use super::state::{LaneQueue, SessionState};

/// Clears the parked-poll marker if this poll still owns it.
///
/// A dropped HTTP request future must not leave `down_active` set, otherwise
/// the next poll on that queue would park behind a caller that no longer runs.
struct PollGuard<'a> {
    session: &'a Session,
    lane: Option<u32>,
    mine: Arc<Notify>,
}

impl Drop for PollGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.session.state.lock();
        if let Some(queue) = queue_mut(&mut state, self.lane)
            && owns(queue, &self.mine)
        {
            queue.down_active = false;
            queue.superseded = None;
        }
    }
}

impl Session {
    /// Waits for the next downlink batch of the shared carrier.
    pub(crate) async fn poll(&self, cursor: u64) -> Result<(Bytes, u64), WebError> {
        if self.uses_lanes() {
            return Err(WebError::Protocol);
        }
        let (body, next, _) = self.poll_queue(None, cursor).await?;
        Ok((body, next))
    }

    /// Waits for the next downlink batch of one carrier lane.
    ///
    /// The third result element reports that the lane finished and the client
    /// should stop polling it.
    pub(crate) async fn poll_lane(
        &self,
        lane_id: u32,
        cursor: u64,
    ) -> Result<(Bytes, u64, bool), WebError> {
        if !self.uses_lanes() || lane_id > crate::web::frame::MAX_STREAM_ID {
            return Err(WebError::Protocol);
        }
        self.poll_queue(Some(lane_id), cursor).await
    }

    async fn poll_queue(
        &self,
        lane: Option<u32>,
        cursor: u64,
    ) -> Result<(Bytes, u64, bool), WebError> {
        let mine = Arc::new(Notify::new());
        let notify = {
            let mut state = self.state.lock();
            if state.closed {
                return Err(WebError::Closed);
            }
            if queue_mut(&mut state, lane).is_none() {
                return Err(WebError::Protocol);
            }
            state.last_activity = Instant::now();
            match self.acknowledge_locked(&mut state, lane, cursor) {
                Acknowledged::Replay(body, next) => return Ok((body, next, false)),
                Acknowledged::Protocol => {
                    drop(state);
                    self.fail(lane);
                    return Err(WebError::Protocol);
                }
                Acknowledged::Continue => {}
            }
            let queue = queue_mut(&mut state, lane).expect("queue presence checked");
            // Newest poll wins: an already parked poll is released with its own
            // cursor instead of refusing the fresh request.
            if queue.down_active
                && let Some(previous) = queue.superseded.take()
            {
                previous.notify_waiters();
            }
            queue.superseded = Some(mine.clone());
            queue.down_active = true;
            queue.notify.clone()
        };
        let _guard = PollGuard {
            session: self,
            lane,
            mine: mine.clone(),
        };

        let deadline = tokio::time::sleep(Duration::from_millis(self.timeouts.long_poll_ms));
        tokio::pin!(deadline);
        loop {
            // Registration happens before the state check so a batch queued
            // between the check and the await cannot be missed.
            let queued = notify.notified();
            let superseded = mine.notified();
            tokio::pin!(queued);
            tokio::pin!(superseded);
            queued.as_mut().enable();
            superseded.as_mut().enable();

            {
                let mut state = self.state.lock();
                match self.collect_locked(&mut state, lane, cursor, &mine) {
                    Collected::Batch(body, next) => {
                        drop(state);
                        self.count_down(body.len());
                        return Ok((body, next, false));
                    }
                    Collected::Superseded => return Ok((Bytes::new(), cursor, false)),
                    Collected::LaneClosed => return Ok((Bytes::new(), cursor, true)),
                    Collected::SessionClosed => return Err(WebError::Closed),
                    Collected::Park => {}
                }
            }

            tokio::select! {
                _ = &mut queued => {}
                _ = &mut superseded => {
                    notify.notify_waiters();
                    return Ok((Bytes::new(), cursor, false));
                }
                _ = &mut deadline => {
                    let mut state = self.state.lock();
                    let collected = self.collect_locked(&mut state, lane, cursor, &mine);
                    match collected {
                        Collected::Batch(body, next) => {
                            drop(state);
                            self.count_down(body.len());
                            return Ok((body, next, false));
                        }
                        Collected::Superseded => {
                            drop(state);
                            notify.notify_waiters();
                            return Ok((Bytes::new(), cursor, false));
                        }
                        Collected::LaneClosed => return Ok((Bytes::new(), cursor, true)),
                        Collected::SessionClosed => return Err(WebError::Closed),
                        Collected::Park => {
                            if let Some(queue) = queue_mut(&mut state, lane) {
                                queue.down_active = false;
                                queue.superseded = None;
                            }
                            state.last_activity = Instant::now();
                            return Ok((Bytes::new(), cursor, false));
                        }
                    }
                }
            }
        }
    }

    /// Applies the client cursor to the queue's replay window.
    fn acknowledge_locked(
        &self,
        state: &mut SessionState,
        lane: Option<u32>,
        cursor: u64,
    ) -> Acknowledged {
        let charged = {
            let Some(queue) = queue_mut(state, lane) else {
                return Acknowledged::Protocol;
            };
            if queue.unacked.is_empty() {
                if cursor != queue.down_cursor {
                    return Acknowledged::Protocol;
                }
                (0, 0)
            } else if cursor == queue.unacked_base {
                return Acknowledged::Replay(queue.unacked.clone(), queue.down_cursor);
            } else if cursor != queue.down_cursor {
                return Acknowledged::Protocol;
            } else {
                let charged = (queue.unacked_cost, queue.unacked_items);
                queue.unacked = Bytes::new();
                queue.unacked_cost = 0;
                queue.unacked_items = 0;
                charged
            }
        };
        if charged != (0, 0) {
            self.release_pending_locked(state, charged.0, charged.1);
        }
        Acknowledged::Continue
    }

    /// Takes the next batch, or reports why the poll must keep waiting.
    fn collect_locked(
        &self,
        state: &mut SessionState,
        lane: Option<u32>,
        cursor: u64,
        mine: &Arc<Notify>,
    ) -> Collected {
        let closed = state.closed;
        let batch_bytes = self.limits.carrier_batch_bytes;
        let lane_finished = match lane {
            Some(id) if id != 0 => {
                !state.streams.contains_key(&id) && state.closed_streams.contains(&id)
            }
            _ => false,
        };
        let Some(queue) = queue_mut(state, lane) else {
            return Collected::LaneClosed;
        };
        if !owns(queue, mine) {
            return Collected::Superseded;
        }
        if !queue.pending_frames.is_empty() {
            let batch = queue.take_down_batch(batch_bytes);
            let body = Bytes::from(batch.body);
            queue.down_cursor += 1;
            queue.unacked_base = cursor;
            queue.unacked = body.clone();
            queue.unacked_cost = batch.cost;
            queue.unacked_items = batch.items;
            queue.down_active = false;
            queue.superseded = None;
            let next = queue.down_cursor;
            return Collected::Batch(body, next);
        }
        if closed {
            queue.down_active = false;
            queue.superseded = None;
            return Collected::SessionClosed;
        }
        if lane_finished {
            queue.down_active = false;
            queue.superseded = None;
            return Collected::LaneClosed;
        }
        Collected::Park
    }

    /// Routes a downlink protocol violation to the right blast radius.
    fn fail(&self, lane: Option<u32>) {
        match lane {
            Some(lane_id) => self.lane_protocol_failure(lane_id),
            None => self.protocol_failure(),
        }
    }
}

/// Result of applying the client cursor.
enum Acknowledged {
    /// The client repeated the previous cursor; replay the same batch.
    Replay(Bytes, u64),
    /// The cursor is neither the previous nor the current one.
    Protocol,
    /// The cursor advanced; the poll may park.
    Continue,
}

/// Result of one parked-poll wake-up.
enum Collected {
    Batch(Bytes, u64),
    Superseded,
    LaneClosed,
    SessionClosed,
    Park,
}

fn owns(queue: &LaneQueue, mine: &Arc<Notify>) -> bool {
    queue
        .superseded
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, mine))
}

fn queue_mut(state: &mut SessionState, lane: Option<u32>) -> Option<&mut LaneQueue> {
    match lane {
        Some(lane_id) => state.lanes.get_mut(&lane_id),
        None => Some(&mut state.main),
    }
}
