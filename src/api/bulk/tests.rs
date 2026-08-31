//! Batch application against a candidate configuration.
//!
//! These drive `apply_operation` directly. It is the part that decides what a
//! batch does — which tables are dirtied, which runtime effects are queued, and
//! what is refused — while the surrounding route only loads, writes and runs
//! the effects it is handed.

use super::apply::{RuntimeEffect, apply_operation};
use super::model::BulkAction;
use crate::api::config_store::AccessSection;
use crate::config::ProxyConfig;

fn config_with_users(users: &[&str]) -> ProxyConfig {
    let mut cfg = ProxyConfig::default();
    cfg.access.users.clear();
    for (index, user) in users.iter().enumerate() {
        cfg.access
            .users
            .insert((*user).to_string(), format!("{:032x}", index + 1));
    }
    cfg
}

fn create_body(username: &str) -> Option<serde_json::Value> {
    Some(serde_json::json!({ "username": username }))
}

#[test]
fn creating_a_user_dirties_only_the_users_table() {
    let mut cfg = config_with_users(&["alice"]);

    let applied = apply_operation(&mut cfg, BulkAction::UserCreate, None, create_body("bob"))
        .expect("a fresh username must be accepted");

    assert_eq!(applied.user, "bob");
    assert_eq!(applied.sections, vec![AccessSection::Users]);
    assert!(applied.effects.is_empty());
    assert!(applied.retained);
    assert!(cfg.access.users.contains_key("bob"));
    let secret = applied.secret.expect("a generated secret must be returned");
    assert_eq!(secret.len(), 32);
    assert_eq!(cfg.access.users.get("bob"), Some(&secret));
}

#[test]
fn creating_a_user_that_exists_is_refused_without_touching_the_config() {
    let mut cfg = config_with_users(&["alice"]);
    let before = cfg.access.users.clone();

    let rejected = apply_operation(&mut cfg, BulkAction::UserCreate, None, create_body("alice"))
        .expect_err("a duplicate username must be refused");

    assert_eq!(rejected.code, "user_exists");
    assert_eq!(cfg.access.users, before);
}

#[test]
fn creating_a_disabled_user_queues_the_runtime_effects_the_route_must_run() {
    let mut cfg = config_with_users(&["alice"]);

    let applied = apply_operation(
        &mut cfg,
        BulkAction::UserCreate,
        None,
        Some(serde_json::json!({ "username": "bob", "enabled": false })),
    )
    .expect("a disabled create must be accepted");

    assert!(applied.sections.contains(&AccessSection::UserEnabled));
    assert_eq!(cfg.access.user_enabled.get("bob"), Some(&false));
    assert!(matches!(
        applied.effects.as_slice(),
        [
            RuntimeEffect::SetEnabled { enabled: false, .. },
            RuntimeEffect::CancelSessions { .. }
        ]
    ));
}

#[test]
fn deleting_the_last_user_is_refused() {
    // The single-operation route refuses this, and a batch must not become a
    // way around it.
    let mut cfg = config_with_users(&["alice"]);

    let rejected = apply_operation(
        &mut cfg,
        BulkAction::UserDelete,
        Some("alice".to_string()),
        None,
    )
    .expect_err("removing the last user must be refused");

    assert_eq!(rejected.code, "last_user_forbidden");
    assert!(cfg.access.users.contains_key("alice"));
}

#[test]
fn deleting_a_user_clears_every_table_keyed_by_their_name() {
    let mut cfg = config_with_users(&["alice", "bob"]);
    cfg.access
        .user_ad_tags
        .insert("bob".to_string(), "0".repeat(32));
    cfg.access.user_max_tcp_conns.insert("bob".to_string(), 4);
    cfg.access.user_data_quota.insert("bob".to_string(), 1024);
    cfg.access.user_max_unique_ips.insert("bob".to_string(), 2);

    let applied = apply_operation(
        &mut cfg,
        BulkAction::UserDelete,
        Some("bob".to_string()),
        None,
    )
    .expect("deleting a non-last user must be accepted");

    assert!(!applied.retained);
    assert!(!cfg.access.users.contains_key("bob"));
    assert!(!cfg.access.user_ad_tags.contains_key("bob"));
    assert!(!cfg.access.user_max_tcp_conns.contains_key("bob"));
    assert!(!cfg.access.user_data_quota.contains_key("bob"));
    assert!(!cfg.access.user_max_unique_ips.contains_key("bob"));
    assert!(
        applied
            .effects
            .iter()
            .any(|effect| matches!(effect, RuntimeEffect::ForgetUser { .. })),
        "a deleted user's process-scoped quota must be dropped"
    );
}

#[test]
fn a_batch_of_creates_dirties_the_users_table_once_per_operation() {
    // The point of the route: N operations, one write. The section list is
    // deduplicated by the persistence layer, so repeats here are expected and
    // must not multiply the write.
    let mut cfg = config_with_users(&["alice"]);
    let mut sections = Vec::new();

    for name in ["bob", "carol", "dave"] {
        let applied = apply_operation(&mut cfg, BulkAction::UserCreate, None, create_body(name))
            .expect("each fresh username must be accepted");
        sections.extend(applied.sections);
    }

    assert_eq!(cfg.access.users.len(), 4);
    assert!(
        sections
            .iter()
            .all(|section| *section == AccessSection::Users)
    );
}

#[test]
fn a_later_operation_sees_what_an_earlier_one_did() {
    // Operations apply to one candidate config in order, so a batch can create
    // a user and then disable them.
    let mut cfg = config_with_users(&["alice"]);

    apply_operation(&mut cfg, BulkAction::UserCreate, None, create_body("bob"))
        .expect("create must be accepted");
    let applied = apply_operation(
        &mut cfg,
        BulkAction::UserDisable,
        Some("bob".to_string()),
        None,
    )
    .expect("disabling the user just created must be accepted");

    assert_eq!(applied.sections, vec![AccessSection::UserEnabled]);
    assert_eq!(cfg.access.user_enabled.get("bob"), Some(&false));
}

#[test]
fn enabling_removes_the_key_instead_of_writing_true() {
    // `POST /v1/users/{user}/enable` drops the key, and writing `true` here
    // would change the config revision for a no-op and invalidate every other
    // caller's If-Match.
    let mut cfg = config_with_users(&["alice"]);
    cfg.access.user_enabled.insert("alice".to_string(), false);

    let applied = apply_operation(
        &mut cfg,
        BulkAction::UserEnable,
        Some("alice".to_string()),
        None,
    )
    .expect("enabling an existing user must be accepted");

    assert_eq!(applied.sections, vec![AccessSection::UserEnabled]);
    assert!(!cfg.access.user_enabled.contains_key("alice"));
}

#[test]
fn patching_enabled_true_also_removes_the_key() {
    let mut cfg = config_with_users(&["alice"]);
    cfg.access.user_enabled.insert("alice".to_string(), false);

    apply_operation(
        &mut cfg,
        BulkAction::UserPatch,
        Some("alice".to_string()),
        Some(serde_json::json!({ "enabled": true })),
    )
    .expect("a patch enabling the user must be accepted");

    assert!(!cfg.access.user_enabled.contains_key("alice"));
}

#[test]
fn rotating_a_secret_replaces_it_and_leaves_live_sessions_alone() {
    let mut cfg = config_with_users(&["alice"]);
    let before = cfg.access.users.get("alice").cloned().unwrap();

    let applied = apply_operation(
        &mut cfg,
        BulkAction::UserRotateSecret,
        Some("alice".to_string()),
        None,
    )
    .expect("rotating an existing user's secret must be accepted");

    let after = cfg.access.users.get("alice").cloned().unwrap();
    assert_ne!(before, after);
    assert_eq!(applied.secret.as_deref(), Some(after.as_str()));
    // The single-operation route leaves live sessions alone, and the same named
    // operation must not mean two different things by route.
    assert!(applied.effects.is_empty());
}

#[test]
fn a_pinned_secret_of_the_wrong_shape_is_refused() {
    let mut cfg = config_with_users(&["alice"]);
    let before = cfg.access.users.get("alice").cloned().unwrap();

    let rejected = apply_operation(
        &mut cfg,
        BulkAction::UserRotateSecret,
        Some("alice".to_string()),
        Some(serde_json::json!({ "secret": "nothex" })),
    )
    .expect_err("a malformed secret must be refused");

    assert_eq!(rejected.code, "bad_request");
    assert_eq!(cfg.access.users.get("alice"), Some(&before));
}

#[test]
fn an_operation_without_a_user_is_refused_rather_than_guessed_at() {
    let mut cfg = config_with_users(&["alice"]);

    let rejected = apply_operation(&mut cfg, BulkAction::UserDelete, None, None)
        .expect_err("a missing `user` must be refused");

    assert_eq!(rejected.code, "bad_request");
    assert!(rejected.message.contains("user"));
}

#[test]
fn patching_a_missing_user_is_a_not_found_rather_than_a_create() {
    let mut cfg = config_with_users(&["alice"]);

    let rejected = apply_operation(
        &mut cfg,
        BulkAction::UserPatch,
        Some("ghost".to_string()),
        Some(serde_json::json!({ "max_tcp_conns": 4 })),
    )
    .expect_err("patching an unknown user must be refused");

    assert_eq!(rejected.code, "not_found");
    assert!(!cfg.access.user_max_tcp_conns.contains_key("ghost"));
}

#[test]
fn patching_removes_a_field_when_it_is_set_to_null() {
    // The tri-state patch semantics of `PATCH /v1/users/{user}` have to survive
    // being carried inside a batch body.
    let mut cfg = config_with_users(&["alice"]);
    cfg.access.user_max_tcp_conns.insert("alice".to_string(), 4);

    let applied = apply_operation(
        &mut cfg,
        BulkAction::UserPatch,
        Some("alice".to_string()),
        Some(serde_json::json!({ "max_tcp_conns": null })),
    )
    .expect("a removal patch must be accepted");

    assert_eq!(applied.sections, vec![AccessSection::UserMaxTcpConns]);
    assert!(!cfg.access.user_max_tcp_conns.contains_key("alice"));
}

#[test]
fn patching_leaves_absent_fields_alone() {
    let mut cfg = config_with_users(&["alice"]);
    cfg.access.user_max_tcp_conns.insert("alice".to_string(), 4);

    let applied = apply_operation(
        &mut cfg,
        BulkAction::UserPatch,
        Some("alice".to_string()),
        Some(serde_json::json!({ "data_quota_bytes": 2048 })),
    )
    .expect("a partial patch must be accepted");

    assert_eq!(applied.sections, vec![AccessSection::UserDataQuota]);
    assert_eq!(cfg.access.user_max_tcp_conns.get("alice"), Some(&4));
    assert_eq!(cfg.access.user_data_quota.get("alice"), Some(&2048));
}
