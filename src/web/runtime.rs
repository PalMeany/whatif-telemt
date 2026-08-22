//! Attachment of demultiplexed WEB streams to the telemt runtime.
//!
//! Each logical stream becomes a normal telemt client session: the same
//! handshake, masking, routing, statistics, and per-user limits apply. The
//! stream never leaves the process, so the reference deployment's loopback
//! MTProxy hop — two syscalls and two kernel copies per chunk — disappears.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use parking_lot::Mutex;
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

/// Allocator of the synthetic source ports in-process WEB streams are given.
///
/// Middle-End key derivation binds the client `ip:port` pair, so two live
/// streams sharing a port silently break the MTProto handshake of whichever
/// arrives second. A counter modulo the span guarantees uniqueness only until
/// it wraps, and the wrap is remotely reachable by churning streams, so the
/// ports actually in use are tracked and a port is handed out only while free.
struct PortPool {
    /// Ports currently held by a live stream.
    used: Mutex<HashSet<u16>>,
    /// Rotating starting point, so successive streams rarely collide.
    cursor: AtomicU32,
}

impl PortPool {
    fn new() -> Self {
        Self {
            used: Mutex::new(HashSet::new()),
            cursor: AtomicU32::new(0),
        }
    }

    /// Leases one free synthetic port, or nothing when the span is full.
    fn lease(self: &Arc<Self>) -> Option<PortLease> {
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        let mut used = self.used.lock();
        if used.len() >= SYNTHETIC_PORT_SPAN as usize {
            return None;
        }
        for offset in 0..SYNTHETIC_PORT_SPAN {
            let port =
                (SYNTHETIC_PORT_BASE + start.wrapping_add(offset) % SYNTHETIC_PORT_SPAN) as u16;
            if used.insert(port) {
                return Some(PortLease {
                    pool: self.clone(),
                    port,
                });
            }
        }
        None
    }
}

/// A synthetic source port held for the lifetime of one WEB stream.
struct PortLease {
    pool: Arc<PortPool>,
    port: u16,
}

impl Drop for PortLease {
    fn drop(&mut self) {
        self.pool.used.lock().remove(&self.port);
    }
}

/// Handle to the live telemt runtime generation used by WEB streams.
pub(crate) struct WebRuntime {
    active_runtime: Arc<ArcSwap<RuntimeGeneration>>,
    ports: Arc<PortPool>,
}

impl WebRuntime {
    pub(crate) fn new(active_runtime: Arc<ArcSwap<RuntimeGeneration>>) -> Arc<Self> {
        Arc::new(Self {
            active_runtime,
            ports: Arc::new(PortPool::new()),
        })
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
        let Some(port) = self.ports.lease() else {
            debug!("WEB stream refused: no free synthetic source port");
            return false;
        };
        let peer = SocketAddr::new(client_ip, port.port);
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
            // The port stays reserved for as long as the stream lives, so no
            // other stream can derive a Middle-End key from the same pair.
            let _port = port;
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
