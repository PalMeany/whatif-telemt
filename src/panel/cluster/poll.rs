//! Background health polling of linked nodes.
//!
//! The master's node list has to say something truthful before an operator
//! clicks anything, so reachability is refreshed on a timer rather than only on
//! demand. The poll is strictly bounded: one pass at a time, one request per
//! node, and it stops with the panel.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::panel::state::{NodeHealth, PanelState, unix_now};

use super::client;

/// Runs the poll loop until the panel is cancelled.
pub(crate) async fn run(state: Arc<PanelState>, shutdown: CancellationToken) {
    let interval = Duration::from_secs(state.config.cluster.poll_interval_secs);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(interval) => {}
        }
        let nodes = {
            let store = state.store.read().await;
            store.nodes.clone()
        };
        for node in nodes {
            if shutdown.is_cancelled() {
                return;
            }
            let started = Instant::now();
            let now = unix_now();
            match client::hello(&state, &node).await {
                Ok((hello, _)) => state.record_node_health(
                    &node.id,
                    NodeHealth {
                        reachable: true,
                        checked_at: now,
                        latency_ms: Some(started.elapsed().as_millis() as u64),
                        error: None,
                        version: Some(hello.version),
                    },
                ),
                Err(error) => {
                    debug!(node = %node.id, %error, "Linked node probe failed");
                    state.record_node_health(
                        &node.id,
                        NodeHealth {
                            reachable: false,
                            checked_at: now,
                            latency_ms: None,
                            error: Some(error.to_string()),
                            version: None,
                        },
                    );
                }
            }
        }
    }
}
