//! In-memory application of one batch to a loaded configuration.
//!
//! Nothing here touches disk or the runtime. One operation mutates the
//! candidate `ProxyConfig`, records which `access.*` tables it dirtied, and
//! queues the runtime side effects the route layer performs after the write.
//! That separation is what lets a batch of any size cost one config write and
//! one set of side effects instead of one per operation.

use crate::api::config_store::AccessSection;
use crate::api::model::{
    CreateUserRequest, PatchUserRequest, RotateSecretRequest, is_valid_ad_tag,
    is_valid_user_secret, is_valid_username, parse_optional_expiration, random_user_secret,
};
use crate::api::patch::Patch;
use crate::config::{ProxyConfig, RateLimitBps};

use super::model::BulkAction;

/// A runtime action deferred until after the batch is written.
///
/// The route layer performs these; they are not configuration, and running
/// them mid-batch would leave live sessions inconsistent with a batch that is
/// later refused as a whole.
#[derive(Debug)]
pub(in crate::api) enum RuntimeEffect {
    /// Publishes an enable/disable decision to the live generation.
    SetEnabled { user: String, enabled: bool },
    /// Cancels a user's live sessions.
    CancelSessions { user: String },
    /// Applies a per-user unique-IP ceiling.
    SetIpLimit { user: String, limit: Option<usize> },
    /// Drops a deleted user's tracked IPs.
    ClearIps { user: String },
    /// Drops a deleted user's process-scoped quota and stats.
    ForgetUser { user: String },
}

/// What one applied operation produced.
#[derive(Debug)]
pub(in crate::api) struct AppliedOperation {
    /// Username the operation acted on.
    pub(in crate::api) user: String,
    /// Secret this operation issued, when it issued one.
    pub(in crate::api) secret: Option<String>,
    /// Tables the operation dirtied.
    pub(in crate::api) sections: Vec<AccessSection>,
    /// Runtime actions to run after the write.
    pub(in crate::api) effects: Vec<RuntimeEffect>,
    /// Whether the user still exists after the operation.
    pub(in crate::api) retained: bool,
}

/// Why one operation was refused.
#[derive(Debug)]
pub(in crate::api) struct RejectedOperation {
    /// Stable failure code, matching the single-operation route's codes.
    pub(in crate::api) code: &'static str,
    /// Human-readable reason.
    pub(in crate::api) message: String,
    /// Username the operation named, when it named a usable one.
    pub(in crate::api) user: Option<String>,
}

impl RejectedOperation {
    fn bad_request(message: impl Into<String>, user: Option<String>) -> Self {
        Self {
            code: "bad_request",
            message: message.into(),
            user,
        }
    }
}

type OperationResult = Result<AppliedOperation, RejectedOperation>;

/// Applies one operation to the candidate configuration.
pub(in crate::api) fn apply_operation(
    cfg: &mut ProxyConfig,
    action: BulkAction,
    user: Option<String>,
    body: Option<serde_json::Value>,
) -> OperationResult {
    match action {
        BulkAction::UserCreate => create_user(cfg, body),
        BulkAction::UserPatch => patch_user(cfg, user, body),
        BulkAction::UserDelete => delete_user(cfg, user),
        BulkAction::UserEnable => set_enabled(cfg, user, true),
        BulkAction::UserDisable => set_enabled(cfg, user, false),
        BulkAction::UserRotateSecret => rotate_secret(cfg, user, body),
    }
}

/// Reads the operation's `user` field, rejecting a missing or malformed name.
fn required_user(user: Option<String>) -> Result<String, RejectedOperation> {
    let Some(user) = user else {
        return Err(RejectedOperation::bad_request(
            "this action requires a `user`",
            None,
        ));
    };
    if !is_valid_username(&user) {
        return Err(RejectedOperation::bad_request(
            "username must match [A-Za-z0-9_.-] and be 1..64 chars",
            None,
        ));
    }
    Ok(user)
}

/// Decodes an operation body into the DTO the single-operation route uses.
fn decode_body<T: serde::de::DeserializeOwned + Default>(
    body: Option<serde_json::Value>,
    user: Option<String>,
) -> Result<T, RejectedOperation> {
    match body {
        None => Ok(T::default()),
        Some(serde_json::Value::Null) => Ok(T::default()),
        Some(value) => serde_json::from_value(value)
            .map_err(|error| RejectedOperation::bad_request(error.to_string(), user)),
    }
}

fn create_user(cfg: &mut ProxyConfig, body: Option<serde_json::Value>) -> OperationResult {
    let Some(body) = body else {
        return Err(RejectedOperation::bad_request(
            "user.create requires a `body`",
            None,
        ));
    };
    let body: CreateUserRequest = serde_json::from_value(body)
        .map_err(|error| RejectedOperation::bad_request(error.to_string(), None))?;

    if !is_valid_username(&body.username) {
        return Err(RejectedOperation::bad_request(
            "username must match [A-Za-z0-9_.-] and be 1..64 chars",
            None,
        ));
    }
    let user = body.username.clone();
    let secret = match body.secret {
        Some(secret) => {
            if !is_valid_user_secret(&secret) {
                return Err(RejectedOperation::bad_request(
                    "secret must be exactly 32 hex characters",
                    Some(user),
                ));
            }
            secret
        }
        None => random_user_secret(),
    };
    if let Some(ad_tag) = body.user_ad_tag.as_ref()
        && !is_valid_ad_tag(ad_tag)
    {
        return Err(RejectedOperation::bad_request(
            "user_ad_tag must be exactly 32 hex characters",
            Some(user),
        ));
    }
    let expiration =
        parse_optional_expiration(body.expiration_rfc3339.as_deref()).map_err(|failure| {
            RejectedOperation {
                code: failure.code,
                message: failure.message,
                user: Some(user.clone()),
            }
        })?;
    if cfg.access.users.contains_key(&user) {
        return Err(RejectedOperation {
            code: "user_exists",
            message: "User already exists".to_string(),
            user: Some(user),
        });
    }

    let mut sections = vec![AccessSection::Users];
    cfg.access.users.insert(user.clone(), secret.clone());
    if let Some(ad_tag) = body.user_ad_tag {
        cfg.access.user_ad_tags.insert(user.clone(), ad_tag);
        sections.push(AccessSection::UserAdTags);
    }
    if let Some(limit) = body.max_tcp_conns {
        cfg.access.user_max_tcp_conns.insert(user.clone(), limit);
        sections.push(AccessSection::UserMaxTcpConns);
    }
    if let Some(expiration) = expiration {
        cfg.access.user_expirations.insert(user.clone(), expiration);
        sections.push(AccessSection::UserExpirations);
    }
    if let Some(quota) = body.data_quota_bytes {
        cfg.access.user_data_quota.insert(user.clone(), quota);
        sections.push(AccessSection::UserDataQuota);
    }
    let mut effects = Vec::new();
    if body.rate_limit_up_bps.is_some() || body.rate_limit_down_bps.is_some() {
        cfg.access.user_rate_limits.insert(
            user.clone(),
            RateLimitBps {
                up_bps: body.rate_limit_up_bps.unwrap_or(0),
                down_bps: body.rate_limit_down_bps.unwrap_or(0),
            },
        );
        sections.push(AccessSection::UserRateLimits);
    }
    if let Some(limit) = body.max_unique_ips {
        cfg.access.user_max_unique_ips.insert(user.clone(), limit);
        sections.push(AccessSection::UserMaxUniqueIps);
        effects.push(RuntimeEffect::SetIpLimit {
            user: user.clone(),
            limit: Some(limit),
        });
    }
    if matches!(body.enabled, Some(false)) {
        cfg.access.user_enabled.insert(user.clone(), false);
        sections.push(AccessSection::UserEnabled);
        effects.push(RuntimeEffect::SetEnabled {
            user: user.clone(),
            enabled: false,
        });
        effects.push(RuntimeEffect::CancelSessions { user: user.clone() });
    }

    Ok(AppliedOperation {
        user,
        secret: Some(secret),
        sections,
        effects,
        retained: true,
    })
}

fn patch_user(
    cfg: &mut ProxyConfig,
    user: Option<String>,
    body: Option<serde_json::Value>,
) -> OperationResult {
    let user = required_user(user)?;
    let Some(body) = body else {
        return Err(RejectedOperation::bad_request(
            "user.patch requires a `body`",
            Some(user),
        ));
    };
    let body: PatchUserRequest = serde_json::from_value(body)
        .map_err(|error| RejectedOperation::bad_request(error.to_string(), Some(user.clone())))?;

    if let Some(secret) = body.secret.as_ref()
        && !is_valid_user_secret(secret)
    {
        return Err(RejectedOperation::bad_request(
            "secret must be exactly 32 hex characters",
            Some(user),
        ));
    }
    if let Patch::Set(ad_tag) = &body.user_ad_tag
        && !is_valid_ad_tag(ad_tag)
    {
        return Err(RejectedOperation::bad_request(
            "user_ad_tag must be exactly 32 hex characters",
            Some(user),
        ));
    }
    let expiration = match &body.expiration_rfc3339 {
        Patch::Unchanged => Patch::Unchanged,
        Patch::Remove => Patch::Remove,
        Patch::Set(value) => {
            let parsed = parse_optional_expiration(Some(value.as_str())).map_err(|failure| {
                RejectedOperation {
                    code: failure.code,
                    message: failure.message,
                    user: Some(user.clone()),
                }
            })?;
            match parsed {
                Some(parsed) => Patch::Set(parsed),
                None => Patch::Remove,
            }
        }
    };
    if !cfg.access.users.contains_key(&user) {
        return Err(RejectedOperation {
            code: "not_found",
            message: "User not found".to_string(),
            user: Some(user),
        });
    }

    let mut sections = Vec::new();
    let mut effects = Vec::new();
    if let Some(secret) = body.secret {
        cfg.access.users.insert(user.clone(), secret);
        sections.push(AccessSection::Users);
    }
    apply_patch_entry(
        &mut cfg.access.user_ad_tags,
        &user,
        body.user_ad_tag,
        &mut sections,
        AccessSection::UserAdTags,
    );
    apply_patch_entry(
        &mut cfg.access.user_max_tcp_conns,
        &user,
        body.max_tcp_conns,
        &mut sections,
        AccessSection::UserMaxTcpConns,
    );
    apply_patch_entry(
        &mut cfg.access.user_expirations,
        &user,
        expiration,
        &mut sections,
        AccessSection::UserExpirations,
    );
    apply_patch_entry(
        &mut cfg.access.user_data_quota,
        &user,
        body.data_quota_bytes,
        &mut sections,
        AccessSection::UserDataQuota,
    );
    if !matches!(body.rate_limit_up_bps, Patch::Unchanged)
        || !matches!(body.rate_limit_down_bps, Patch::Unchanged)
    {
        let mut rate_limit = cfg
            .access
            .user_rate_limits
            .get(&user)
            .copied()
            .unwrap_or_default();
        match body.rate_limit_up_bps {
            Patch::Unchanged => {}
            Patch::Remove => rate_limit.up_bps = 0,
            Patch::Set(limit) => rate_limit.up_bps = limit,
        }
        match body.rate_limit_down_bps {
            Patch::Unchanged => {}
            Patch::Remove => rate_limit.down_bps = 0,
            Patch::Set(limit) => rate_limit.down_bps = limit,
        }
        if rate_limit.up_bps == 0 && rate_limit.down_bps == 0 {
            cfg.access.user_rate_limits.remove(&user);
        } else {
            cfg.access.user_rate_limits.insert(user.clone(), rate_limit);
        }
        sections.push(AccessSection::UserRateLimits);
    }
    match body.max_unique_ips {
        Patch::Unchanged => {}
        Patch::Remove => {
            cfg.access.user_max_unique_ips.remove(&user);
            sections.push(AccessSection::UserMaxUniqueIps);
            effects.push(RuntimeEffect::SetIpLimit {
                user: user.clone(),
                limit: None,
            });
        }
        Patch::Set(limit) => {
            cfg.access.user_max_unique_ips.insert(user.clone(), limit);
            sections.push(AccessSection::UserMaxUniqueIps);
            effects.push(RuntimeEffect::SetIpLimit {
                user: user.clone(),
                limit: Some(limit),
            });
        }
    }
    match body.enabled {
        Patch::Unchanged => {}
        // A removed `enabled` means "back to the default", which is enabled.
        Patch::Remove | Patch::Set(true) => {
            cfg.access.user_enabled.insert(user.clone(), true);
            sections.push(AccessSection::UserEnabled);
            effects.push(RuntimeEffect::SetEnabled {
                user: user.clone(),
                enabled: true,
            });
        }
        Patch::Set(false) => {
            cfg.access.user_enabled.insert(user.clone(), false);
            sections.push(AccessSection::UserEnabled);
            effects.push(RuntimeEffect::SetEnabled {
                user: user.clone(),
                enabled: false,
            });
            effects.push(RuntimeEffect::CancelSessions { user: user.clone() });
        }
    }

    Ok(AppliedOperation {
        user,
        secret: None,
        sections,
        effects,
        retained: true,
    })
}

/// Applies one tri-state patch to a per-user map and records the dirty table.
fn apply_patch_entry<T>(
    map: &mut std::collections::HashMap<String, T>,
    user: &str,
    patch: Patch<T>,
    sections: &mut Vec<AccessSection>,
    section: AccessSection,
) {
    match patch {
        Patch::Unchanged => {}
        Patch::Remove => {
            map.remove(user);
            sections.push(section);
        }
        Patch::Set(value) => {
            map.insert(user.to_string(), value);
            sections.push(section);
        }
    }
}

fn delete_user(cfg: &mut ProxyConfig, user: Option<String>) -> OperationResult {
    let user = required_user(user)?;
    if !cfg.access.users.contains_key(&user) {
        return Err(RejectedOperation {
            code: "not_found",
            message: "User not found".to_string(),
            user: Some(user),
        });
    }
    if cfg.access.users.len() <= 1 {
        return Err(RejectedOperation {
            code: "last_user_forbidden",
            message: "Cannot delete the last configured user".to_string(),
            user: Some(user),
        });
    }

    cfg.access.users.remove(&user);
    cfg.access.user_enabled.remove(&user);
    cfg.access.user_ad_tags.remove(&user);
    cfg.access.user_max_tcp_conns.remove(&user);
    cfg.access.user_expirations.remove(&user);
    cfg.access.user_data_quota.remove(&user);
    cfg.access.user_rate_limits.remove(&user);
    cfg.access.user_max_unique_ips.remove(&user);

    Ok(AppliedOperation {
        secret: None,
        sections: vec![
            AccessSection::Users,
            AccessSection::UserEnabled,
            AccessSection::UserAdTags,
            AccessSection::UserMaxTcpConns,
            AccessSection::UserExpirations,
            AccessSection::UserDataQuota,
            AccessSection::UserRateLimits,
            AccessSection::UserMaxUniqueIps,
        ],
        effects: vec![
            RuntimeEffect::SetIpLimit {
                user: user.clone(),
                limit: None,
            },
            RuntimeEffect::ClearIps { user: user.clone() },
            RuntimeEffect::ForgetUser { user: user.clone() },
            RuntimeEffect::SetEnabled {
                user: user.clone(),
                enabled: true,
            },
            RuntimeEffect::CancelSessions { user: user.clone() },
        ],
        user,
        retained: false,
    })
}

fn set_enabled(cfg: &mut ProxyConfig, user: Option<String>, enabled: bool) -> OperationResult {
    let user = required_user(user)?;
    if !cfg.access.users.contains_key(&user) {
        return Err(RejectedOperation {
            code: "not_found",
            message: "User not found".to_string(),
            user: Some(user),
        });
    }
    cfg.access.user_enabled.insert(user.clone(), enabled);
    let mut effects = vec![RuntimeEffect::SetEnabled {
        user: user.clone(),
        enabled,
    }];
    if !enabled {
        effects.push(RuntimeEffect::CancelSessions { user: user.clone() });
    }
    Ok(AppliedOperation {
        user,
        secret: None,
        sections: vec![AccessSection::UserEnabled],
        effects,
        retained: true,
    })
}

fn rotate_secret(
    cfg: &mut ProxyConfig,
    user: Option<String>,
    body: Option<serde_json::Value>,
) -> OperationResult {
    let user = required_user(user)?;
    let body: RotateSecretRequest = decode_body(body, Some(user.clone()))?;
    let secret = body.secret.unwrap_or_else(random_user_secret);
    if !is_valid_user_secret(&secret) {
        return Err(RejectedOperation::bad_request(
            "secret must be exactly 32 hex characters",
            Some(user),
        ));
    }
    if !cfg.access.users.contains_key(&user) {
        return Err(RejectedOperation {
            code: "not_found",
            message: "User not found".to_string(),
            user: Some(user),
        });
    }
    cfg.access.users.insert(user.clone(), secret.clone());
    Ok(AppliedOperation {
        // Rotating a secret invalidates every live session for that user, so
        // they are cancelled rather than left relaying under a stale key.
        effects: vec![RuntimeEffect::CancelSessions { user: user.clone() }],
        user,
        secret: Some(secret),
        sections: vec![AccessSection::Users],
        retained: true,
    })
}
