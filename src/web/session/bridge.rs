//! In-process stream endpoint handed to a backend.
//!
//! `StreamBridge` turns one logical carrier stream into an `AsyncRead +
//! AsyncWrite` handle, so a demultiplexed stream reaches telemt's own client
//! pipeline without a loopback socket: reads take bytes straight out of the
//! session's uplink queue and writes queue DATA frames straight into the
//! downlink queue, with WINDOW credit and pending budget accounted in place.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Sleep;
use tracing::debug;

use crate::web::frame::{self, FrameType};

use super::Session;
use super::state::{PendingCharge, QUEUE_ITEM_COST, SessionState};

/// Retry delay used when the process-wide pool, not this session, is full.
///
/// Session-local headroom wakes the parked writer through
/// `release_pending_locked`, but a pool exhausted by *other* sessions produces
/// no such wake-up, so the writer arms its own timer instead of stalling.
const GLOBAL_BACKPRESSURE_RETRY: Duration = Duration::from_millis(25);

/// One logical stream presented as a byte-oriented duplex endpoint.
pub(crate) struct StreamBridge {
    session: Arc<Session>,
    id: u32,
    finished: bool,
    /// Armed while the process-wide pending pool refuses this stream's DATA.
    backoff: Option<Pin<Box<Sleep>>>,
}

impl StreamBridge {
    pub(crate) fn new(session: Arc<Session>, id: u32) -> Self {
        Self {
            session,
            id,
            finished: false,
            backoff: None,
        }
    }

    /// Detaches the stream, sending CLOSE to the client exactly once.
    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.session.backend_closed(self.id);
    }
}

impl Drop for StreamBridge {
    fn drop(&mut self) {
        self.finish();
    }
}

impl AsyncRead for StreamBridge {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(Ok(()));
        }
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut state = this.session.state.lock();
        if state.closed {
            return Poll::Ready(Ok(()));
        }
        let (copied, completed, grant) = {
            let Some(stream) = state.streams.get_mut(&this.id) else {
                return Poll::Ready(Ok(()));
            };
            if stream.writes.is_empty() {
                if stream.aborted {
                    return Poll::Ready(Ok(()));
                }
                stream.read_waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
            let mut copied = 0usize;
            let mut completed = 0usize;
            while buf.remaining() != 0 {
                let Some(front) = stream.writes.front() else {
                    break;
                };
                let start = stream.write_offset;
                let take = (front.len() - start).min(buf.remaining());
                buf.put_slice(&front[start..start + take]);
                stream.write_offset += take;
                copied += take;
                if stream.write_offset == front.len() {
                    stream.writes.pop_front();
                    stream.write_offset = 0;
                    completed += 1;
                }
            }
            // Credit returns to the client only after the bytes left the queue,
            // which is what bounds a client that never drains its stream.
            let grant = u32::try_from(copied).unwrap_or(u32::MAX);
            stream.pending_write_bytes = stream.pending_write_bytes.saturating_sub(copied);
            stream.pending_write_cost = stream
                .pending_write_cost
                .saturating_sub(copied + completed * QUEUE_ITEM_COST);
            stream.pending_write_items = stream.pending_write_items.saturating_sub(completed);
            stream.receive_window = stream
                .receive_window
                .saturating_add(grant)
                .min(frame::INITIAL_STREAM_WINDOW);
            (copied, completed, grant)
        };
        if copied == 0 {
            return Poll::Ready(Ok(()));
        }
        let release_cost = copied + completed * QUEUE_ITEM_COST;
        this.session
            .release_pending_locked(&mut state, PendingCharge::data(release_cost, completed));
        let queued = this.session.queue_frame_locked(
            &mut state,
            FrameType::WINDOW,
            this.id,
            &frame::window_payload(grant),
        );
        if !queued {
            this.session.control_budget_exhausted(&mut state, this.id);
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for StreamBridge {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // A previous poll found the process-wide pool full; wait out its timer
        // before charging the session lock again.
        if let Some(backoff) = this.backoff.as_mut() {
            if backoff.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            this.backoff = None;
        }
        let mut state = this.session.state.lock();
        if state.closed {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)));
        }
        let Some(stream) = state.streams.get(&this.id) else {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)));
        };
        let credit = stream.send_credit;
        let wanted = buf.len().min(frame::DATA_CHUNK);
        let wanted = wanted.min(usize::try_from(credit).unwrap_or(usize::MAX));
        let allowance = if wanted == 0 {
            0
        } else {
            this.session.data_frame_allowance_locked(&state, wanted)
        };
        if allowance == 0 {
            if let Some(stream) = state.streams.get_mut(&this.id) {
                stream.write_waker = Some(cx.waker().clone());
            }
            return Poll::Pending;
        }
        // Credit is spent only once the frame is actually queued. Deducting
        // first and then failing would bleed the window on every retry, and
        // the client only ever grants credit for bytes it received.
        let queued = this.session.queue_frame_locked(
            &mut state,
            FrameType::DATA,
            this.id,
            &buf[..allowance],
        );
        if !queued {
            if let Some(stream) = state.streams.get_mut(&this.id) {
                stream.write_waker = Some(cx.waker().clone());
            }
            drop(state);
            // `AsyncWrite` has no retryable error, so `WouldBlock` here would
            // kill a live stream over transient backpressure. Park instead.
            let mut backoff = Box::pin(tokio::time::sleep(GLOBAL_BACKPRESSURE_RETRY));
            let armed = backoff.as_mut().poll(cx).is_pending();
            if armed {
                this.backoff = Some(backoff);
            } else {
                cx.waker().wake_by_ref();
            }
            return Poll::Pending;
        }
        if let Some(stream) = state.streams.get_mut(&this.id) {
            stream.send_credit -= allowance as u64;
        }
        Poll::Ready(Ok(allowance))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.finish();
        Poll::Ready(Ok(()))
    }
}

impl Session {
    /// Detaches a finished stream and tells the client it is gone.
    ///
    /// `CLOSE` is an abort: anything still queued for the stream in either
    /// direction is dropped, matching the client's own TCP behaviour.
    pub(crate) fn backend_closed(&self, id: u32) {
        let mut state = self.state.lock();
        if state.closed {
            return;
        }
        let Some(mut stream) = state.streams.remove(&id) else {
            return;
        };
        let mut released =
            PendingCharge::data(stream.pending_write_cost, stream.pending_write_items);
        stream.pending_write_bytes = 0;
        stream.pending_write_cost = 0;
        stream.pending_write_items = 0;
        stream.writes.clear();
        stream.write_offset = 0;
        stream.aborted = true;
        stream.cancel.cancel();
        released.add(state.remember_closed(id, self.limits.max_closed_stream_ids));
        self.release_pending_locked(&mut state, released);
        if !self.queue_frame_locked(&mut state, FrameType::CLOSE, id, &[]) {
            self.control_budget_exhausted(&mut state, id);
        }
    }

    /// Closes the session after a control frame could not be queued.
    ///
    /// The control reserve is sized for one WINDOW and one CLOSE per live
    /// stream. Exceeding it means the peer stopped draining its downlink while
    /// still driving streams, and there is no way to tell it about a stream it
    /// will never hear about again, so the session ends here rather than
    /// silently diverging from the client's view.
    pub(crate) fn control_budget_exhausted(&self, state: &mut SessionState, id: u32) {
        debug!(
            session = self.id,
            stream = id,
            profile = %self.profile.name,
            "WEB session closed: control reserve exhausted"
        );
        self.close_locked(state);
    }
}
