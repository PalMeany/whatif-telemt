//! `POST /v1/bulk`: many user operations, one config write.
//!
//! The single-operation routes each take the mutation lock, load the config
//! from disk, write it back and fsync. Provisioning a hundred users that way
//! costs a hundred of each, and every write invalidates the caller's revision
//! for the next request. This route loads once, applies the whole batch in
//! memory, writes once, and then performs the runtime side effects.
//!
//! Off by default; enabled with `[fork.api] bulk_enabled = true`.
//!
//! Submodules:
//! - `apply`: in-memory application of one operation to a candidate config
//! - `model`: request and response wire types

use std::collections::BTreeSet;
use std::time::Duration;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};

use crate::config::ProxyConfig;
use crate::ip_tracker::UserIpTracker;
use crate::proxy::shared_state::ProxySharedState;
use crate::stats::Stats;

use super::config_store::{
    AccessSection, ensure_expected_revision, load_config_from_disk, parse_if_match,
    save_access_sections_to_disk,
};
use super::http_utils::{read_json, success_response};
use super::model::ApiFailure;
use super::users::users_from_config;
use super::{ALLOW_POST, ApiShared};

mod apply;
mod model;
#[cfg(test)]
mod tests;

pub(super) use apply::{RuntimeEffect, apply_operation};
pub(crate) use model::BulkAction;
use model::{BulkRequest, BulkResponse, BulkResult};

/// Path this module owns.
const BULK_PATH: &str = "/v1/bulk";

/// Methods allowed on the bulk route, or nothing when it is not one.
pub(super) fn allowed_methods(path: &str) -> Option<&'static str> {
    (path == BULK_PATH).then_some(ALLOW_POST)
}

/// Reports whether a normalized path belongs to this module.
pub(super) fn is_route(path: &str) -> bool {
    path == BULK_PATH
}

/// Answers `POST /v1/bulk`.
pub(super) async fn handle(
    method: Method,
    request: Request<Incoming>,
    shared: &ApiShared,
    config: &ProxyConfig,
    body_limit: usize,
    read_only: bool,
) -> Result<Response<Full<Bytes>>, ApiFailure> {
    if method != Method::POST {
        return Err(ApiFailure::method_not_allowed(ALLOW_POST));
    }
    if !config.fork.bulk_enabled() {
        return Err(ApiFailure::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "Bulk requests are disabled; enable them with [fork.api] bulk_enabled = true",
        ));
    }
    if read_only {
        return Err(ApiFailure::new(
            StatusCode::FORBIDDEN,
            "read_only",
            "API runs in read-only mode",
        ));
    }

    let expected_revision = parse_if_match(request.headers());
    let body = read_json::<BulkRequest>(request.into_body(), body_limit).await?;
    let max_operations = config.fork.api.bulk_max_operations;
    if body.operations.is_empty() {
        return Err(ApiFailure::bad_request("operations must not be empty"));
    }
    if body.operations.len() > max_operations {
        return Err(ApiFailure::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_many_operations",
            format!("a batch may carry at most {max_operations} operations"),
        ));
    }

    let budget = Duration::from_secs(u64::from(config.fork.api.bulk_timeout_secs));
    run_batch(body, expected_revision, shared, budget).await
}

/// Applies one batch and writes it.
///
/// `budget` bounds only the part that runs *before* the write: taking the
/// mutation lock, loading the config, and applying every operation in memory.
/// The write and the runtime effects that follow it are deliberately outside
/// it, because cancelling there would leave the file on disk describing a state
/// the running process was never told about — and `write_atomic` runs in
/// `spawn_blocking`, so dropping the future would not have undone it anyway.
async fn run_batch(
    body: BulkRequest,
    expected_revision: Option<String>,
    shared: &ApiShared,
    budget: Duration,
) -> Result<Response<Full<Bytes>>, ApiFailure> {
    let atomic = body.atomic;
    let deadline = tokio::time::Instant::now() + budget;
    let Ok(guard) = tokio::time::timeout_at(deadline, shared.mutation_lock.lock()).await else {
        return Err(timed_out(budget));
    };
    let Ok(loaded) =
        tokio::time::timeout_at(deadline, load_config_from_disk(&shared.config_path)).await
    else {
        return Err(timed_out(budget));
    };
    let mut cfg = loaded?;
    ensure_expected_revision(&shared.config_path, expected_revision.as_deref()).await?;

    let Some(outcome) = apply_batch(&mut cfg, body.operations, atomic, deadline) else {
        // Cut the batch short before the write rather than after it: a
        // partially applied batch that was never reported is the outcome an
        // operator cannot reconcile.
        drop(guard);
        return Err(timed_out(budget));
    };
    let BatchOutcome {
        mut results,
        sections,
        effects,
        secrets,
        retained,
        succeeded,
        failed,
    } = outcome;

    if failed > 0 && atomic {
        // Nothing was written: the candidate config is dropped with the guard.
        // The results that had already been applied in memory are relabelled,
        // so a caller iterating them cannot read `ok` for a user that does not
        // exist.
        drop(guard);
        roll_back(&mut results);
        return Ok(success_response(
            StatusCode::CONFLICT,
            BulkResponse {
                applied: false,
                succeeded: 0,
                failed,
                results,
            },
            crate::api::config_store::current_revision(&shared.config_path).await?,
        ));
    }

    if succeeded == 0 {
        drop(guard);
        return Ok(success_response(
            StatusCode::OK,
            BulkResponse {
                applied: false,
                succeeded,
                failed,
                results,
            },
            crate::api::config_store::current_revision(&shared.config_path).await?,
        ));
    }

    cfg.validate()
        .map_err(|error| ApiFailure::bad_request(format!("config validation failed: {}", error)))?;
    let revision = save_access_sections_to_disk(&shared.config_path, &cfg, &sections).await?;

    // Effects run while the lock is still held, so the in-memory decision a
    // batch publishes cannot be overtaken by a later writer's: without this the
    // bot and the API can commit in one order and publish in the other.
    run_runtime_effects(
        effects,
        &shared.stats,
        &shared.ip_tracker,
        &shared.proxy_shared,
        shared
            .active_runtime
            .load()
            .config()
            .fork
            .runtime_switches()
            .user_delete_forgets_quota,
    )
    .await;
    drop(guard);

    for (index, secret) in secrets {
        results[index].secret = Some(secret);
    }
    attach_views(&mut results, &retained, &cfg, shared).await;

    Ok(success_response(
        StatusCode::OK,
        BulkResponse {
            applied: failed == 0,
            succeeded,
            failed,
            results,
        },
        revision,
    ))
}

/// Runs the deferred runtime actions once the batch is on disk.
///
/// Shared with `crate::api::control`, so the bot and this route apply the same
/// side effects in the same order.
pub(super) async fn run_runtime_effects(
    effects: Vec<RuntimeEffect>,
    stats: &Stats,
    ip_tracker: &UserIpTracker,
    proxy_shared: &ProxySharedState,
    forget_deleted_user_quota: bool,
) {
    for effect in effects {
        match effect {
            RuntimeEffect::SetEnabled { user, enabled } => {
                proxy_shared.set_user_enabled(&user, enabled);
            }
            RuntimeEffect::CancelSessions { user } => {
                proxy_shared.cancel_user_sessions(&user);
            }
            RuntimeEffect::SetIpLimit { user, limit } => match limit {
                Some(limit) => ip_tracker.set_user_limit(&user, limit).await,
                None => ip_tracker.remove_user_limit(&user).await,
            },
            RuntimeEffect::ClearIps { user } => {
                ip_tracker.clear_user_ips(&user).await;
            }
            RuntimeEffect::ForgetUser { user } => {
                // Quota is process-scoped and outlives both the config edit and
                // the runtime generation, so a re-created name would otherwise
                // start pre-charged.
                if forget_deleted_user_quota {
                    stats.forget_user(&user);
                }
            }
        }
    }
}

/// Attaches the post-batch view of every user the batch kept.
async fn attach_views(
    results: &mut [BulkResult],
    retained: &BTreeSet<String>,
    cfg: &ProxyConfig,
    shared: &ApiShared,
) {
    if retained.is_empty() {
        return;
    }
    let (detected_ip_v4, detected_ip_v6) = shared.detected_link_ips();
    let mut views = users_from_config(
        cfg,
        &shared.stats,
        &shared.ip_tracker,
        detected_ip_v4,
        detected_ip_v6,
        None,
    )
    .await
    .into_iter()
    .filter(|view| retained.contains(&view.username))
    .map(|view| (view.username.clone(), view))
    .collect::<std::collections::HashMap<_, _>>();

    for result in results.iter_mut() {
        if result.status != "ok" {
            continue;
        }
        let Some(user) = result.user.as_deref() else {
            continue;
        };
        // One user may appear in several operations; the last one wins the
        // single view, which is also the state that is now on disk.
        if let Some(view) = views.remove(user) {
            result.view = Some(view);
        }
    }
}

/// What applying a batch to a candidate configuration produced.
///
/// Split out of [`run_batch`] so the decisions -- which results are `ok`, what
/// a refusal aborts, what a rollback relabels -- are testable without a live
/// API, a config file, or a runtime generation.
struct BatchOutcome {
    /// One entry per submitted operation, in submission order.
    results: Vec<BulkResult>,
    /// Every `access.*` table the batch dirtied, with repeats.
    sections: Vec<AccessSection>,
    /// Runtime actions queued for after the write.
    effects: Vec<RuntimeEffect>,
    /// Secrets issued, keyed by their index in `results`.
    secrets: Vec<(usize, String)>,
    /// Users the batch leaves in the configuration.
    retained: BTreeSet<String>,
    /// Operations that changed the candidate.
    succeeded: usize,
    /// Operations that were refused.
    failed: usize,
}

/// Applies every operation to the candidate, in order.
///
/// Returns nothing when `deadline` passes mid-batch, which the caller turns
/// into a refusal: stopping here means nothing has been written yet.
fn apply_batch(
    cfg: &mut ProxyConfig,
    operations: Vec<model::BulkOperation>,
    atomic: bool,
    deadline: tokio::time::Instant,
) -> Option<BatchOutcome> {
    let mut outcome = BatchOutcome {
        results: Vec::with_capacity(operations.len()),
        sections: Vec::new(),
        effects: Vec::new(),
        secrets: Vec::new(),
        retained: BTreeSet::new(),
        succeeded: 0,
        failed: 0,
    };
    let mut aborted = false;

    for operation in operations {
        let action = operation.action;
        let id = operation.id;
        if aborted {
            outcome
                .results
                .push(BulkResult::skipped(id, action, operation.user));
            continue;
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        match apply_operation(cfg, action, operation.user, operation.body) {
            Ok(applied) => {
                outcome.sections.extend(applied.sections);
                outcome.effects.extend(applied.effects);
                if let Some(secret) = applied.secret {
                    outcome.secrets.push((outcome.results.len(), secret));
                }
                if applied.retained {
                    outcome.retained.insert(applied.user.clone());
                } else {
                    outcome.retained.remove(&applied.user);
                }
                outcome.succeeded += 1;
                outcome
                    .results
                    .push(BulkResult::ok(id, action, applied.user));
            }
            Err(rejected) => {
                outcome.failed += 1;
                outcome.results.push(BulkResult::failed(
                    id,
                    action,
                    rejected.user,
                    rejected.code,
                    rejected.message,
                ));
                if atomic {
                    aborted = true;
                }
            }
        }
    }
    Some(outcome)
}

/// Relabels the results of a batch whose candidate was discarded.
fn roll_back(results: &mut [BulkResult]) {
    for result in results.iter_mut() {
        result.rolled_back();
    }
}

/// Builds the refusal used when a batch outruns its configured budget.
fn timed_out(budget: Duration) -> ApiFailure {
    ApiFailure::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "bulk_timeout",
        format!(
            "the batch exceeded fork.api.bulk_timeout_secs = {}",
            budget.as_secs()
        ),
    )
}
