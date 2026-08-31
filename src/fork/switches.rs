//! Process-wide mirror of the `[fork.runtime]` switches that cannot reach a
//! configuration handle.
//!
//! Two fork behaviours run where no `ProxyConfig` is in scope: `Drop` for the
//! TLS-front cache, which returns a retired generation's reservations to a
//! process-wide gauge, and the reload status store, which is owned by the
//! process rather than by a generation. Both are start-up decisions, so the
//! boot configuration publishes them here once and they are read as plain
//! atomics afterwards.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::ProxyConfig;

/// Whether a retired TLS-front cache returns its full-certificate reservations.
static TLS_FRONT_CACHE_BUDGET_RELEASE: AtomicBool = AtomicBool::new(true);

/// Whether reload status carries the stable `error_kind` slug.
static RELOAD_ERROR_KIND: AtomicBool = AtomicBool::new(true);

/// Publishes the switches this module mirrors, once, from the boot config.
pub(crate) fn publish(config: &ProxyConfig) {
    let switches = config.fork.runtime_switches();
    TLS_FRONT_CACHE_BUDGET_RELEASE
        .store(switches.tls_front_cache_budget_release, Ordering::Relaxed);
    RELOAD_ERROR_KIND.store(switches.reload_error_kind, Ordering::Relaxed);
}

/// Reports whether a dropped TLS-front cache releases its budget reservations.
pub(crate) fn tls_front_cache_budget_release() -> bool {
    TLS_FRONT_CACHE_BUDGET_RELEASE.load(Ordering::Relaxed)
}

/// Reports whether reload status serialises `error_kind`.
pub(crate) fn reload_error_kind() -> bool {
    RELOAD_ERROR_KIND.load(Ordering::Relaxed)
}
