//! WEB relay counters and their Prometheus rendering.
//!
//! The relay's own admin endpoint keeps the reference `tproxy_*` metric names
//! so existing dashboards keep working, while telemt's main `/metrics` surface
//! exposes the same values under the `telemt_web_*` prefix.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

/// Live resource usage sampled under the manager lock.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WebCapacity {
    pub(crate) sessions: usize,
    pub(crate) streams: usize,
    pub(crate) backend_dials_in_flight: usize,
    pub(crate) pending_bytes: u64,
    pub(crate) pending_items: u64,
}

/// Monotonic relay counters.
#[derive(Debug, Default)]
pub(crate) struct WebMetrics {
    pub(crate) sessions_created: AtomicU64,
    pub(crate) sessions_closed: AtomicU64,
    pub(crate) streams_opened: AtomicU64,
    pub(crate) streams_rejected: AtomicU64,
    pub(crate) backend_dial_failures: AtomicU64,
    pub(crate) bytes_up: AtomicU64,
    pub(crate) bytes_down: AtomicU64,
    pub(crate) limit_hits: AtomicU64,
    /// Connections refused because the accept-loop budget was exhausted.
    pub(crate) carrier_connections_dropped: AtomicU64,
    /// Requests answered with 503 because they overran the relay deadline.
    pub(crate) request_timeouts: AtomicU64,
    /// Retryable answers handed to a client that must poll again.
    pub(crate) retry_later_responses: AtomicU64,
}

/// Snapshot of the counters plus current capacity.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WebMetricsSnapshot {
    pub(crate) capacity: WebCapacity,
    pub(crate) sessions_created: u64,
    pub(crate) sessions_closed: u64,
    pub(crate) streams_opened: u64,
    pub(crate) streams_rejected: u64,
    pub(crate) backend_dial_failures: u64,
    pub(crate) bytes_up: u64,
    pub(crate) bytes_down: u64,
    pub(crate) limit_hits: u64,
    pub(crate) carrier_connections_dropped: u64,
    pub(crate) request_timeouts: u64,
    pub(crate) retry_later_responses: u64,
}

impl WebMetrics {
    /// Reads every counter and merges it with a capacity sample.
    pub(crate) fn snapshot(&self, capacity: WebCapacity) -> WebMetricsSnapshot {
        WebMetricsSnapshot {
            capacity,
            sessions_created: self.sessions_created.load(Ordering::Relaxed),
            sessions_closed: self.sessions_closed.load(Ordering::Relaxed),
            streams_opened: self.streams_opened.load(Ordering::Relaxed),
            streams_rejected: self.streams_rejected.load(Ordering::Relaxed),
            backend_dial_failures: self.backend_dial_failures.load(Ordering::Relaxed),
            bytes_up: self.bytes_up.load(Ordering::Relaxed),
            bytes_down: self.bytes_down.load(Ordering::Relaxed),
            limit_hits: self.limit_hits.load(Ordering::Relaxed),
            carrier_connections_dropped: self.carrier_connections_dropped.load(Ordering::Relaxed),
            request_timeouts: self.request_timeouts.load(Ordering::Relaxed),
            retry_later_responses: self.retry_later_responses.load(Ordering::Relaxed),
        }
    }
}

impl WebMetricsSnapshot {
    /// Renders the snapshot with the given metric-name prefix.
    pub(crate) fn render(&self, prefix: &str) -> String {
        let mut out = String::with_capacity(768);
        let values: [(&str, u64); 16] = [
            ("sessions_live", self.capacity.sessions as u64),
            ("streams_live", self.capacity.streams as u64),
            (
                "backend_dials_in_flight",
                self.capacity.backend_dials_in_flight as u64,
            ),
            ("pending_bytes", self.capacity.pending_bytes),
            ("pending_items", self.capacity.pending_items),
            ("sessions_created_total", self.sessions_created),
            ("sessions_closed_total", self.sessions_closed),
            ("streams_opened_total", self.streams_opened),
            ("streams_rejected_total", self.streams_rejected),
            ("backend_dial_failures_total", self.backend_dial_failures),
            ("bytes_up_total", self.bytes_up),
            ("bytes_down_total", self.bytes_down),
            ("limit_hits_total", self.limit_hits),
            (
                "carrier_connections_dropped_total",
                self.carrier_connections_dropped,
            ),
            ("request_timeouts_total", self.request_timeouts),
            ("retry_later_responses_total", self.retry_later_responses),
        ];
        for (name, value) in values {
            out.push_str(prefix);
            out.push_str(name);
            out.push(' ');
            out.push_str(&value.to_string());
            out.push('\n');
        }
        out
    }
}

/// Process-wide handle used by telemt's main metrics endpoint.
///
/// The relay publishes itself once its listener is bound and withdraws on
/// shutdown, so `/metrics` renders the block only while a relay is actually
/// running. It is replaceable rather than write-once: a start-up that fails
/// after publishing, or a shutdown followed by a fresh start, would otherwise
/// leave the process reporting the counters of a relay that no longer exists.
static ACTIVE_RELAY_METRICS: Mutex<Option<Arc<dyn WebMetricsSource>>> = Mutex::new(None);

/// Source of a live metrics snapshot.
pub(crate) trait WebMetricsSource: Send + Sync {
    fn snapshot(&self) -> WebMetricsSnapshot;
}

/// Publishes the running relay so telemt's `/metrics` can sample it.
pub(crate) fn register_metrics_source(source: Arc<dyn WebMetricsSource>) {
    *ACTIVE_RELAY_METRICS.lock() = Some(source);
}

/// Withdraws the published relay when it stops.
pub(crate) fn clear_metrics_source() {
    *ACTIVE_RELAY_METRICS.lock() = None;
}

/// Renders the `telemt_web_*` block, or an empty string when no relay runs.
pub(crate) fn render_active_metrics() -> String {
    let source = ACTIVE_RELAY_METRICS.lock().clone();
    match source {
        Some(source) => source.snapshot().render("telemt_web_"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_emits_every_series() {
        let snapshot = WebMetricsSnapshot {
            capacity: WebCapacity {
                sessions: 2,
                streams: 5,
                backend_dials_in_flight: 1,
                pending_bytes: 4096,
                pending_items: 8,
            },
            bytes_up: 100,
            ..WebMetricsSnapshot::default()
        };
        let text = snapshot.render("tproxy_");
        assert!(text.contains("tproxy_sessions_live 2\n"));
        assert!(text.contains("tproxy_streams_live 5\n"));
        assert!(text.contains("tproxy_bytes_up_total 100\n"));
        assert!(text.contains("tproxy_carrier_connections_dropped_total 0\n"));
        assert!(text.contains("tproxy_request_timeouts_total 0\n"));
        assert!(text.contains("tproxy_retry_later_responses_total 0\n"));
        assert_eq!(text.lines().count(), 16);
    }
}
