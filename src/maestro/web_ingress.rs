//! Process ownership of telemt's own WEB transport.
//!
//! That transport is bound as a `[[server.listeners]]` entry with
//! `transport = "web"`, but everything behind the socket — the session manager,
//! the debug trace store, and the lifecycle publication the API reads — belongs
//! to the process rather than to a runtime generation. This module owns those
//! three pieces so the accept loops, the API task, and the shutdown sequence
//! all reach them the same way.
//!
//! This fork's alternative WEB transport is unrelated and lives in
//! `crate::fork::web`, with its own listener under `[fork.web]`.

use std::net::SocketAddr;
use std::sync::{Arc, Weak};

use arc_swap::ArcSwap;
use tokio::sync::watch;
use tracing::info;

use crate::config::{ListenerTransport, ProxyConfig};
use crate::web::control::{WebRuntimeControl, WebRuntimeLifecycle};
use crate::web::manager::{WebProcessRuntime, WebShutdownOutcome};
use crate::web::trace::WebTraceStore;

use super::generation::{RuntimeGeneration, RuntimeWatchState};
use super::listeners::BoundListener;

/// Process-owned handles for telemt's WEB transport.
pub(crate) struct WebIngress {
    /// Bounded debug recorder shared with the API's WEB status routes.
    trace: Arc<WebTraceStore>,
    /// Lifecycle publication the API reads to answer WEB runtime queries.
    control: WebRuntimeControl,
    /// Session manager, present only once a WEB listener is actually bound.
    runtime: Option<Arc<WebProcessRuntime>>,
    /// Addresses answering WEB, published alongside the lifecycle.
    listeners: Arc<[SocketAddr]>,
}

impl WebIngress {
    /// Builds the process-owned handles before any listener is bound.
    ///
    /// The API task starts before the listeners do and needs both handles, so
    /// they exist from the moment the process has a configuration.
    pub(crate) fn new(config: &ProxyConfig) -> Self {
        Self {
            trace: WebTraceStore::new(config.web.debug.clone(), &config.web.limits),
            control: WebRuntimeControl::new(),
            runtime: None,
            listeners: Arc::from([]),
        }
    }

    /// Shared trace store handed to the API task.
    pub(crate) fn trace(&self) -> Arc<WebTraceStore> {
        Arc::clone(&self.trace)
    }

    /// Lifecycle receiver handed to the API task.
    pub(crate) fn subscribe(&self) -> watch::Receiver<crate::web::control::WebRuntimePublication> {
        self.control.subscribe()
    }

    /// Starts the session manager when the bound inventory contains a WEB listener.
    ///
    /// Returns the manager the accept loops dispatch into, or nothing when no
    /// listener asked for the transport.
    pub(crate) fn start(
        &mut self,
        bound: &[BoundListener],
        active_runtime: Arc<ArcSwap<RuntimeGeneration>>,
    ) -> Option<Arc<WebProcessRuntime>> {
        let requested = bound
            .iter()
            .any(|listener| listener.transport == ListenerTransport::Web);
        let config = active_runtime.load().config();
        let listeners: Arc<[SocketAddr]> = if config.fork.telemt_web_enabled(requested) {
            bound
                .iter()
                .filter(|listener| listener.transport == ListenerTransport::Web)
                .filter_map(|listener| listener.listener.local_addr().ok())
                .collect()
        } else {
            Arc::from([])
        };
        self.listeners = Arc::clone(&listeners);
        if listeners.is_empty() {
            self.control
                .publish(WebRuntimeLifecycle::NoWebListener, listeners, Weak::new());
            return None;
        }
        let runtime = WebProcessRuntime::start_with_trace(active_runtime, self.trace());
        self.control.publish(
            WebRuntimeLifecycle::Running,
            listeners,
            Arc::downgrade(&runtime),
        );
        info!(
            listeners = self.listeners.len(),
            "WEB transport (telemt) started"
        );
        self.runtime = Some(Arc::clone(&runtime));
        Some(runtime)
    }

    /// Follows the active generation so a hot-reloaded debug policy applies.
    ///
    /// The trace store is process-owned while `[web].debug` is a hot field, so
    /// nothing else would carry a policy change into it.
    pub(crate) fn spawn_policy_watcher(
        &self,
        mut runtime_watch_rx: watch::Receiver<Option<RuntimeWatchState>>,
    ) {
        let trace = self.trace();
        tokio::spawn(async move {
            let mut current: Option<RuntimeWatchState> = None;
            loop {
                let published = runtime_watch_rx.borrow_and_update().clone();
                if adopts(
                    current.as_ref().map(|state| state.generation_id),
                    &published,
                ) && let Some(state) = published
                {
                    let config = state.config_rx.borrow().clone();
                    trace.apply_policy(state.generation_id, &config.web.debug);
                    current = Some(state);
                }

                let Some(state) = current.as_mut() else {
                    if runtime_watch_rx.changed().await.is_err() {
                        return;
                    }
                    continue;
                };

                tokio::select! {
                    changed = runtime_watch_rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    changed = state.config_rx.changed() => {
                        if changed.is_err() {
                            // The generation is gone; wait for the next one.
                            if runtime_watch_rx.changed().await.is_err() {
                                return;
                            }
                            continue;
                        }
                        let generation_id = state.generation_id;
                        let config = state.config_rx.borrow_and_update().clone();
                        trace.apply_policy(generation_id, &config.web.debug);
                    }
                }
            }
        });
    }

    /// Drains WEB ingress under its configured shutdown budget.
    pub(crate) async fn shutdown(&self) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        self.control.publish(
            WebRuntimeLifecycle::Draining,
            Arc::clone(&self.listeners),
            Arc::downgrade(runtime),
        );
        let outcome = runtime.shutdown().await;
        self.control.publish(
            if outcome == WebShutdownOutcome::DeadlineExceeded {
                WebRuntimeLifecycle::DeadlineExceeded
            } else {
                WebRuntimeLifecycle::Drained
            },
            Arc::clone(&self.listeners),
            Weak::new(),
        );
    }
}

/// Whether the published generation replaces the one already being followed.
///
/// Re-adopting the generation already in hand would also re-clone its config
/// receiver out of the watch value, and that stored receiver's seen-version is
/// frozen at the moment the generation was built. A rewound receiver reports
/// `changed()` immediately, for ever, so the watcher would spin at full CPU
/// from the first hot reload onwards.
fn adopts(followed: Option<u64>, published: &Option<RuntimeWatchState>) -> bool {
    match (followed, published) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(followed), Some(published)) => followed != published.generation_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProxyConfig;
    use crate::maestro::generation::test_runtime_generation;

    fn watch_state(id: u64) -> RuntimeWatchState {
        test_runtime_generation(id, ProxyConfig::default()).watch_state()
    }

    #[test]
    fn the_first_published_generation_is_adopted() {
        assert!(adopts(None, &Some(watch_state(1))));
    }

    #[test]
    fn a_new_generation_replaces_the_followed_one() {
        assert!(adopts(Some(1), &Some(watch_state(2))));
    }

    #[test]
    fn the_generation_already_followed_is_never_re_adopted() {
        // Re-adopting rewinds the config receiver this task parks on, which is
        // what turned the watcher into a busy loop after one hot reload.
        assert!(!adopts(Some(7), &Some(watch_state(7))));
    }

    #[test]
    fn nothing_is_adopted_before_a_generation_is_published() {
        assert!(!adopts(None, &None));
        assert!(!adopts(Some(3), &None));
    }
}
