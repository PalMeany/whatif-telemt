//! Process-scoped runtime state shared by every [`RuntimeGeneration`].
//!
//! In-process reload introduced a second lifetime scope. This module is the
//! single place that answers "what does the *process* own, versus what does a
//! generation own": everything reachable from [`ProcessScope`] outlives every
//! cutover and must be retuned in place rather than rebuilt, because rebuilding
//! it per generation would either double a committed resource envelope (the
//! admission semaphore, the Direct buffer budget, the shared buffer pool) or
//! silently reset a process-level observation (quota accounting, uptime).
//!
//! [`RuntimeGeneration`]: super::generation::RuntimeGeneration

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Semaphore, watch};
use tracing::debug;

use crate::config::ProxyConfig;
use crate::proxy::direct_buffer_budget::{
    DirectBufferBudget, DirectBufferBudgetInputs, resolve_direct_buffer_hard_limit,
};
use crate::proxy::shared_state::ProxySharedState;
use crate::stats::{QuotaStore, Stats};
use crate::stream::BufferPool;

/// Retry cadence while a shrink is blocked on permits that are still in flight.
const LIMIT_RECONCILE_RETRY: Duration = Duration::from_millis(250);

/// Buffer geometry of the process-wide relay pool.
const BUFFER_POOL_BUFFER_BYTES: usize = 64 * 1024;
const BUFFER_POOL_MAX_BUFFERS: usize = 4096;

/// Process-wide connection admission budget.
///
/// One semaphore backs every generation. Minting a fresh `Semaphore(max_connections)`
/// per generation let a drain reload run at 2x the configured cap: the new
/// generation issues a full set of permits from the accept loop while the
/// retired generation's sessions still hold theirs for the whole drain window,
/// so a 20k cap admitted ~40k sessions and ~80k fds and the listeners spun on
/// `EMFILE`. Reload retunes this one budget instead.
pub(crate) struct ConnectionLimiter {
    semaphore: Arc<Semaphore>,
    /// Permits currently backing `semaphore`.
    issued: AtomicUsize,
    /// Permit count the limiter is converging on.
    target: AtomicUsize,
    /// Serializes reconciliation so concurrent retunes cannot interleave.
    reconcile: Mutex<()>,
}

impl ConnectionLimiter {
    /// Creates the process-wide limiter for a configured `max_connections`.
    pub(crate) fn new(max_connections: u32) -> Arc<Self> {
        let limit = Self::permits_for(max_connections);
        Arc::new(Self {
            semaphore: Arc::new(Semaphore::new(limit)),
            issued: AtomicUsize::new(limit),
            target: AtomicUsize::new(limit),
            reconcile: Mutex::new(()),
        })
    }

    /// Translates `server.max_connections` (0 = unlimited) into a permit count.
    pub(crate) fn permits_for(max_connections: u32) -> usize {
        if max_connections == 0 {
            Semaphore::MAX_PERMITS
        } else {
            max_connections as usize
        }
    }

    /// Returns the shared semaphore handed to a generation's accept path.
    pub(crate) fn semaphore(&self) -> Arc<Semaphore> {
        self.semaphore.clone()
    }

    /// Permits currently backing the limiter (test/observability hook).
    #[cfg(test)]
    pub(crate) fn issued_permits(&self) -> usize {
        self.issued.load(Ordering::Acquire)
    }

    /// Retunes the budget to a new configured limit.
    ///
    /// Growing is immediate. Shrinking forgets whatever is free right now and
    /// keeps converging in the background as in-flight sessions release their
    /// permits, so the process never temporarily admits above the new cap and
    /// never blocks the cutover waiting for the old one to drain.
    pub(crate) fn retune(self: &Arc<Self>, max_connections: u32) {
        let target = Self::permits_for(max_connections);
        if self.target.swap(target, Ordering::AcqRel) == target
            && self.issued.load(Ordering::Acquire) == target
        {
            return;
        }
        let limiter = self.clone();
        tokio::spawn(async move { limiter.reconcile_to_target().await });
    }

    async fn reconcile_to_target(self: Arc<Self>) {
        let _guard = self.reconcile.lock().await;
        loop {
            let target = self.target.load(Ordering::Acquire);
            let issued = self.issued.load(Ordering::Acquire);
            if issued == target {
                return;
            }
            if issued < target {
                let granted = target - issued;
                self.semaphore.add_permits(granted);
                self.issued.store(target, Ordering::Release);
                debug!(granted, target, "Connection limiter grew");
                continue;
            }
            let surplus = issued - target;
            let forgotten = self.semaphore.forget_permits(surplus);
            if forgotten > 0 {
                self.issued.fetch_sub(forgotten, Ordering::AcqRel);
                debug!(forgotten, target, "Connection limiter shrank");
            }
            if forgotten < surplus {
                // The rest is held by live sessions; converge as they finish.
                tokio::time::sleep(LIMIT_RECONCILE_RETRY).await;
            }
        }
    }
}

/// Everything the process owns for its whole lifetime, across every generation.
pub(crate) struct ProcessScope {
    /// Process start instant. Seeds every generation's `Stats` so
    /// `telemt_uptime_seconds` measures process uptime, not generation uptime.
    started_at: Instant,
    quota_store: Arc<QuotaStore>,
    connection_limiter: Arc<ConnectionLimiter>,
    buffer_pool: Arc<BufferPool>,
    direct_buffer_budget: Arc<DirectBufferBudget>,
    budget_inputs_tx: watch::Sender<Arc<DirectBufferBudgetInputs>>,
    /// Fork switches resolved once, at start-up.
    ///
    /// Which resources a generation is handed is a structural decision taken
    /// while the process boots, so it is captured here rather than re-read per
    /// generation: a reload that flipped it mid-flight would leave the two
    /// generations sharing half a budget.
    switches: ProcessSwitches,
}

/// The `[fork.runtime]` switches this scope acts on.
#[derive(Debug, Clone, Copy)]
struct ProcessSwitches {
    /// Share one admission budget across generations.
    admission_budget: bool,
    /// Share one relay buffer pool across generations.
    buffer_pool: bool,
    /// Seed generation stats with the process start instant.
    uptime_clock: bool,
}

impl ProcessScope {
    /// Builds the process scope from the configuration the process booted with.
    pub(crate) async fn new(config: &ProxyConfig) -> Arc<Self> {
        let hard_limit =
            resolve_direct_buffer_hard_limit(config.general.direct_relay_buffer_budget_max_bytes)
                .await;
        let direct_buffer_budget = DirectBufferBudget::new(hard_limit);
        let buffer_pool = Arc::new(BufferPool::with_config(
            BUFFER_POOL_BUFFER_BYTES,
            BUFFER_POOL_MAX_BUFFERS,
        ));
        // Seeded with a throwaway generation view; `publish_generation` replaces
        // it before the controller's first tick and on every cutover after that.
        let (budget_inputs_tx, _) = watch::channel(Arc::new(DirectBufferBudgetInputs {
            stats: Arc::new(Stats::new()),
            shared: ProxySharedState::new_with_direct_buffer_budget(direct_buffer_budget.clone()),
            max_connections: config.server.max_connections,
            buffer_pool: buffer_pool.clone(),
        }));
        let fork = config.fork.runtime_switches();
        Arc::new(Self {
            started_at: Instant::now(),
            quota_store: Arc::new(QuotaStore::default()),
            connection_limiter: ConnectionLimiter::new(config.server.max_connections),
            buffer_pool,
            direct_buffer_budget,
            budget_inputs_tx,
            switches: ProcessSwitches {
                admission_budget: fork.process_admission_budget,
                buffer_pool: fork.process_buffer_pool,
                uptime_clock: fork.process_uptime_clock,
            },
        })
    }

    /// Start instant a new generation's `Stats` is seeded with.
    ///
    /// With `fork.runtime.process_uptime_clock` off this is the moment the
    /// generation is built, so uptime restarts on every reload as telemt's does.
    pub(crate) fn started_at(&self) -> Instant {
        if self.switches.uptime_clock {
            self.started_at
        } else {
            Instant::now()
        }
    }

    /// Per-user quota accounting that must survive reloads.
    pub(crate) fn quota_store(&self) -> Arc<QuotaStore> {
        self.quota_store.clone()
    }

    /// Connection admission budget a generation is handed.
    ///
    /// With `fork.runtime.process_admission_budget` off each generation gets
    /// its own limiter, so a drain admits up to twice `server.max_connections`
    /// while both generations are live, exactly as telemt does.
    pub(crate) fn connection_limiter(&self, config: &ProxyConfig) -> Arc<ConnectionLimiter> {
        if self.switches.admission_budget {
            self.connection_limiter.clone()
        } else {
            ConnectionLimiter::new(config.server.max_connections)
        }
    }

    /// Relay buffer pool a generation is handed.
    ///
    /// With `fork.runtime.process_buffer_pool` off each generation allocates
    /// its own pool, as telemt does.
    pub(crate) fn buffer_pool(&self) -> Arc<BufferPool> {
        if self.switches.buffer_pool {
            self.buffer_pool.clone()
        } else {
            Arc::new(BufferPool::with_config(
                BUFFER_POOL_BUFFER_BYTES,
                BUFFER_POOL_MAX_BUFFERS,
            ))
        }
    }

    /// Process-wide Direct copy-buffer envelope.
    pub(crate) fn direct_buffer_budget(&self) -> Arc<DirectBufferBudget> {
        self.direct_buffer_budget.clone()
    }

    /// Spawns the single process-wide Direct buffer budget controller.
    pub(crate) fn spawn_budget_controller(&self) -> tokio::task::JoinHandle<()> {
        let budget = self.direct_buffer_budget.clone();
        let inputs_rx = self.budget_inputs_tx.subscribe();
        tokio::spawn(async move {
            crate::proxy::direct_buffer_budget::run_direct_buffer_budget_controller(
                budget, inputs_rx,
            )
            .await;
        })
    }

    /// Retargets process-scoped budgets at a newly activated generation.
    pub(crate) fn publish_generation(
        self: &Arc<Self>,
        config: &ProxyConfig,
        stats: Arc<Stats>,
        shared: Arc<ProxySharedState>,
        buffer_pool: Arc<BufferPool>,
    ) {
        if self.switches.admission_budget {
            self.connection_limiter
                .retune(config.server.max_connections);
        }
        self.budget_inputs_tx
            .send_replace(Arc::new(DirectBufferBudgetInputs {
                stats,
                shared,
                max_connections: config.server.max_connections,
                buffer_pool,
            }));
    }

    /// Re-points the Direct envelope after a reload changed its configured cap.
    ///
    /// Kept separate from [`Self::publish_generation`] because resolving the
    /// ceiling reads system memory and must not run on the cutover path.
    pub(crate) async fn retune_direct_buffer_budget(&self, config: &ProxyConfig) {
        let hard_limit =
            resolve_direct_buffer_hard_limit(config.general.direct_relay_buffer_budget_max_bytes)
                .await;
        self.direct_buffer_budget.set_hard_limit(hard_limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retune_grows_and_shrinks_a_single_process_budget() {
        let limiter = ConnectionLimiter::new(4);
        assert_eq!(limiter.issued_permits(), 4);
        assert_eq!(limiter.semaphore().available_permits(), 4);

        limiter.retune(16);
        for _ in 0..64 {
            if limiter.issued_permits() == 16 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(limiter.issued_permits(), 16);
        assert_eq!(limiter.semaphore().available_permits(), 16);

        limiter.retune(2);
        for _ in 0..64 {
            if limiter.issued_permits() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(limiter.issued_permits(), 2);
        assert_eq!(limiter.semaphore().available_permits(), 2);
    }

    #[tokio::test]
    async fn shrink_never_exceeds_the_new_cap_while_sessions_hold_permits() {
        let limiter = ConnectionLimiter::new(4);
        let semaphore = limiter.semaphore();
        let held = semaphore.clone().acquire_many_owned(3).await.unwrap();

        limiter.retune(1);
        for _ in 0..64 {
            if semaphore.available_permits() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        // Only one free permit existed, so the cutover cannot hand out more.
        assert_eq!(semaphore.available_permits(), 0);

        drop(held);
        // The rest converges as the retired sessions release their permits.
        for _ in 0..200 {
            if limiter.issued_permits() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(limiter.issued_permits(), 1);
        assert_eq!(semaphore.available_permits(), 1);
    }

    #[tokio::test]
    async fn zero_max_connections_means_unlimited() {
        assert_eq!(ConnectionLimiter::permits_for(0), Semaphore::MAX_PERMITS);
        assert_eq!(ConnectionLimiter::permits_for(20_000), 20_000);
    }
}
