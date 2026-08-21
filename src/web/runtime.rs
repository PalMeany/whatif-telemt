//! Attachment of demultiplexed WEB streams to the telemt runtime.
//!
//! Each logical stream becomes a normal telemt client session: the same
//! handshake, masking, routing, statistics, and per-user limits apply. The
//! stream never leaves the process, so the reference deployment's loopback
//! MTProxy hop — two syscalls and two kernel copies per chunk — disappears.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::maestro::generation::RuntimeGeneration;
use crate::web::manager::StreamPermit;
use crate::web::session::bridge::StreamBridge;

/// Copy buffer used when a profile forwards to a loopback MTProxy.
const LOOPBACK_COPY_BUFFER: usize = 64 * 1024;

/// Lowest synthetic source port assigned to a WEB stream.
const SYNTHETIC_PORT_BASE: u32 = 1024;

/// Number of synthetic source ports cycled through.
const SYNTHETIC_PORT_SPAN: u32 = 65535 - SYNTHETIC_PORT_BASE;

/// Handle to the live telemt runtime generation used by WEB streams.
pub(crate) struct WebRuntime {
    active_runtime: Arc<ArcSwap<RuntimeGeneration>>,
    port_counter: AtomicU32,
}

impl WebRuntime {
    pub(crate) fn new(active_runtime: Arc<ArcSwap<RuntimeGeneration>>) -> Arc<Self> {
        Arc::new(Self {
            active_runtime,
            port_counter: AtomicU32::new(0),
        })
    }

    /// Assigns a distinct synthetic source port per stream.
    ///
    /// The Middle-End key derivation binds the client `ip:port` pair, so two
    /// concurrent streams of one WEB session must not share a port.
    fn next_port(&self) -> u16 {
        let ticket = self.port_counter.fetch_add(1, Ordering::Relaxed);
        (SYNTHETIC_PORT_BASE + ticket % SYNTHETIC_PORT_SPAN) as u16
    }

    /// Runs one stream through telemt's own client pipeline.
    ///
    /// Returns `false` when runtime admission is closed or the process-wide
    /// connection budget is exhausted; the caller then aborts that stream.
    pub(crate) fn spawn_internal_stream(
        &self,
        bridge: StreamBridge,
        client_ip: IpAddr,
        cancel: CancellationToken,
        permit: StreamPermit,
    ) -> bool {
        let runtime = self.active_runtime.load_full();
        if !*runtime.admission_rx.borrow() {
            return false;
        }
        let Ok(connection_permit) = runtime.max_connections.clone().try_acquire_owned() else {
            return false;
        };
        let peer = SocketAddr::new(client_ip, self.next_port());
        let config = runtime.config();
        let stats = runtime.stats.clone();
        let upstream_manager = runtime.upstream_manager.clone();
        let replay_checker = runtime.replay_checker.clone();
        let buffer_pool = runtime.buffer_pool.clone();
        let rng = runtime.rng.clone();
        let me_pool = runtime.me_pool.clone();
        let me_pool_runtime = runtime.me_pool_runtime.clone();
        let route_runtime = runtime.route_runtime.clone();
        let tls_cache = runtime.tls_cache.clone();
        let ip_tracker = runtime.ip_tracker.clone();
        let beobachten = runtime.beobachten.clone();
        let shared = runtime.proxy_shared.clone();
        let mut permit = permit;
        permit.dial_finished(false);
        runtime.spawn_session(async move {
            let _connection_permit = connection_permit;
            let _permit = permit;
            let handler = crate::proxy::client::handle_client_stream_with_shared_and_pool_runtime(
                bridge,
                peer,
                config,
                stats,
                upstream_manager,
                replay_checker,
                buffer_pool,
                rng,
                me_pool,
                Some(me_pool_runtime),
                route_runtime,
                tls_cache,
                ip_tracker,
                beobachten,
                shared,
                false,
            );
            tokio::select! {
                _ = cancel.cancelled() => {}
                result = handler => {
                    if let Err(error) = result {
                        debug!(peer = %peer, error = %error, "WEB stream closed");
                    }
                }
            }
        })
    }

    /// Forwards one stream to a loopback MTProxy, matching the reference relay.
    pub(crate) fn spawn_loopback_stream(
        &self,
        bridge: StreamBridge,
        address: SocketAddr,
        dial_timeout: Duration,
        cancel: CancellationToken,
        permit: StreamPermit,
    ) -> bool {
        let runtime = self.active_runtime.load_full();
        if !*runtime.admission_rx.borrow() {
            return false;
        }
        let Ok(connection_permit) = runtime.max_connections.clone().try_acquire_owned() else {
            return false;
        };
        let mut permit = permit;
        runtime.spawn_session(async move {
            let _connection_permit = connection_permit;
            let dialed = tokio::time::timeout(dial_timeout, TcpStream::connect(address)).await;
            let mut backend = match dialed {
                Ok(Ok(stream)) => {
                    permit.dial_finished(false);
                    stream
                }
                Ok(Err(error)) => {
                    permit.dial_finished(true);
                    debug!(backend = %address, error = %error, "WEB backend dial failed");
                    return;
                }
                Err(_) => {
                    permit.dial_finished(true);
                    debug!(backend = %address, "WEB backend dial timed out");
                    return;
                }
            };
            let _permit = permit;
            let mut bridge = bridge;
            tokio::select! {
                _ = cancel.cancelled() => {}
                result = tokio::io::copy_bidirectional_with_sizes(
                    &mut bridge,
                    &mut backend,
                    LOOPBACK_COPY_BUFFER,
                    LOOPBACK_COPY_BUFFER,
                ) => {
                    if let Err(error) = result {
                        debug!(backend = %address, error = %error, "WEB backend relay ended");
                    }
                }
            }
        })
    }

    /// Returns the live configuration of the active runtime generation.
    pub(crate) fn config(&self) -> Arc<crate::config::ProxyConfig> {
        self.active_runtime.load().config()
    }

    /// Reports whether the runtime still accepts new sessions.
    pub(crate) fn admission_open(&self) -> bool {
        *self.active_runtime.load().admission_rx.borrow()
    }
}
