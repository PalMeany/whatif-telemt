//! In-process stream endpoint handed to a backend.
//!
//! `StreamBridge` turns one logical carrier stream into an `AsyncRead +
//! AsyncWrite` handle, so a demultiplexed stream reaches telemt's own client
//! pipeline without a loopback socket: reads take bytes straight out of the
//! session's uplink queue and writes queue DATA frames straight into the
//! downlink queue, with WINDOW credit and pending budget accounted in place.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::web::frame::{self, FrameType};

use super::Session;
use super::state::QUEUE_ITEM_COST;

/// One logical stream presented as a byte-oriented duplex endpoint.
pub(crate) struct StreamBridge {
    session: Arc<Session>,
    id: u32,
    finished: bool,
}

impl StreamBridge {
    pub(crate) fn new(session: Arc<Session>, id: u32) -> Self {
        Self {
            session,
            id,
            finished: false,
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
            .release_pending_locked(&mut state, release_cost, completed);
        let queued = this.session.queue_frame_locked(
            &mut state,
            FrameType::WINDOW,
            this.id,
            &frame::window_payload(grant),
        );
        if !queued {
            this.session.close_locked(&mut state);
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
        if let Some(stream) = state.streams.get_mut(&this.id) {
            stream.send_credit -= allowance as u64;
        }
        let queued = this.session.queue_frame_locked(
            &mut state,
            FrameType::DATA,
            this.id,
            &buf[..allowance],
        );
        if !queued {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::WouldBlock)));
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
        let released = (stream.pending_write_cost, stream.pending_write_items);
        stream.pending_write_bytes = 0;
        stream.pending_write_cost = 0;
        stream.pending_write_items = 0;
        stream.writes.clear();
        stream.write_offset = 0;
        stream.aborted = true;
        stream.cancel.cancel();
        let evicted = state.remember_closed(id, self.limits.max_closed_stream_ids);
        self.release_pending_locked(&mut state, released.0 + evicted.0, released.1 + evicted.1);
        if !self.queue_frame_locked(&mut state, FrameType::CLOSE, id, &[]) {
            self.close_locked(&mut state);
        }
    }
}
