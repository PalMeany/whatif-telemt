//! Backend attachment for streams accepted by a batch.

use std::sync::Arc;
use std::time::Duration;

use crate::config::WebBackend;

use super::Session;
use super::bridge::StreamBridge;
use super::uplink::OpenedStream;

impl Session {
    /// Connects every stream opened by a batch to the profile's backend.
    ///
    /// Runs outside the session lock so a spawn refusal can abort exactly that
    /// stream without touching the session or its siblings.
    pub(crate) fn spawn_opened(self: &Arc<Self>, opened: Vec<OpenedStream>) {
        for stream in opened {
            let bridge = StreamBridge::new(self.clone(), stream.id);
            let started = match self.profile.backend {
                WebBackend::Internal => self.runtime.spawn_internal_stream(
                    bridge,
                    self.client_ip,
                    stream.cancel,
                    stream.permit,
                ),
                WebBackend::Loopback(address) => self.runtime.spawn_loopback_stream(
                    bridge,
                    address,
                    Duration::from_millis(self.timeouts.backend_dial_ms),
                    stream.cancel,
                    stream.permit,
                ),
            };
            if !started {
                self.backend_closed(stream.id);
            }
        }
    }
}
