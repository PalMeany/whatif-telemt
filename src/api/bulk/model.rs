//! Wire types for `POST /v1/bulk`.

use serde::{Deserialize, Serialize};

use crate::api::model::UserInfo;

/// One batch of user operations applied under a single config write.
#[derive(Debug, Deserialize)]
pub(super) struct BulkRequest {
    /// Refuses the whole batch when any operation is invalid.
    ///
    /// On by default: a partially applied batch is the outcome an operator
    /// cannot undo without reading every result.
    #[serde(default = "default_atomic")]
    pub(super) atomic: bool,

    /// Operations, applied in order.
    pub(super) operations: Vec<BulkOperation>,
}

fn default_atomic() -> bool {
    true
}

/// One operation in a batch.
#[derive(Debug, Deserialize)]
pub(super) struct BulkOperation {
    /// Caller-supplied correlation id, echoed back on the result.
    #[serde(default)]
    pub(super) id: Option<String>,

    /// What to do.
    pub(super) action: BulkAction,

    /// Target username. Required by every action except `user.create`, which
    /// takes it from the body.
    #[serde(default)]
    pub(super) user: Option<String>,

    /// Action payload, in the same shape the single-operation route accepts.
    #[serde(default)]
    pub(super) body: Option<serde_json::Value>,
}

/// Operations a batch may carry.
///
/// Deliberately limited to the `access.*` tables: those are the ones an
/// operator provisions in bulk, and they are all owned by one config source,
/// so the whole batch is one atomic write. Every variant is prefixed with the
/// object it acts on, so the wire names stay readable once more than users are
/// batchable.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum BulkAction {
    /// Adds a user; body is a `POST /v1/users` request.
    #[serde(rename = "user.create")]
    UserCreate,
    /// Updates a user; body is a `PATCH /v1/users/{user}` request.
    #[serde(rename = "user.patch")]
    UserPatch,
    /// Removes a user and everything keyed by their name.
    #[serde(rename = "user.delete")]
    UserDelete,
    /// Clears `access.user_enabled` for a user.
    #[serde(rename = "user.enable")]
    UserEnable,
    /// Sets `access.user_enabled` to false and cancels live sessions.
    #[serde(rename = "user.disable")]
    UserDisable,
    /// Replaces a user's secret; body may pin one.
    #[serde(rename = "user.rotate_secret")]
    UserRotateSecret,
}

impl BulkAction {
    /// Wire name, so a result reads the same as the request that produced it.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BulkAction::UserCreate => "user.create",
            BulkAction::UserPatch => "user.patch",
            BulkAction::UserDelete => "user.delete",
            BulkAction::UserEnable => "user.enable",
            BulkAction::UserDisable => "user.disable",
            BulkAction::UserRotateSecret => "user.rotate_secret",
        }
    }
}

/// Batch outcome returned in the ordinary success envelope.
#[derive(Serialize)]
pub(super) struct BulkResponse {
    /// Whether every operation was applied.
    pub(super) applied: bool,
    /// Operations that changed the configuration.
    pub(super) succeeded: usize,
    /// Operations that were refused.
    pub(super) failed: usize,
    /// One entry per submitted operation, in submission order.
    pub(super) results: Vec<BulkResult>,
}

/// Outcome of one operation.
#[derive(Serialize)]
pub(super) struct BulkResult {
    /// Correlation id the caller supplied, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) id: Option<String>,
    /// Wire name of the action.
    pub(super) action: &'static str,
    /// Target username, once it is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) user: Option<String>,
    /// `ok`, `failed`, `rolled_back`, or `skipped`.
    pub(super) status: &'static str,
    /// Stable failure code, matching the single-operation route's codes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) code: Option<&'static str>,
    /// Human-readable failure reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) message: Option<String>,
    /// Secret this operation issued, for `user.create` and `user.rotate_secret`.
    ///
    /// Returned here because it is the only place the caller can read a
    /// generated secret; the config file is the other one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) secret: Option<String>,
    /// The user as it stands after the batch, for operations that kept one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) view: Option<UserInfo>,
}

impl BulkResult {
    /// Builds a successful result for one operation.
    pub(super) fn ok(id: Option<String>, action: BulkAction, user: String) -> Self {
        Self {
            id,
            action: action.as_str(),
            user: Some(user),
            status: "ok",
            code: None,
            message: None,
            secret: None,
            view: None,
        }
    }

    /// Builds a failed result for one operation.
    pub(super) fn failed(
        id: Option<String>,
        action: BulkAction,
        user: Option<String>,
        code: &'static str,
        message: String,
    ) -> Self {
        Self {
            id,
            action: action.as_str(),
            user,
            status: "failed",
            code: Some(code),
            message: Some(message),
            secret: None,
            view: None,
        }
    }

    /// Relabels an applied result after the batch was rolled back.
    ///
    /// The operation validated and mutated the candidate, but the candidate was
    /// discarded, so leaving it as `ok` would tell a caller iterating results
    /// that a user exists when nothing was written.
    pub(super) fn rolled_back(&mut self) {
        if self.status != "ok" {
            return;
        }
        self.status = "rolled_back";
        self.code = Some("batch_aborted");
        self.message =
            Some("applied in memory, then discarded because the batch is atomic".to_string());
        self.secret = None;
        self.view = None;
    }

    /// Builds a result for an operation the batch never reached.
    pub(super) fn skipped(id: Option<String>, action: BulkAction, user: Option<String>) -> Self {
        Self {
            id,
            action: action.as_str(),
            user,
            status: "skipped",
            code: Some("batch_aborted"),
            message: Some("an earlier operation failed and the batch is atomic".to_string()),
            secret: None,
            view: None,
        }
    }
}
