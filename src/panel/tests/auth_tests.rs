//! Sign-in, session handling, and the anti-automation gates around them.

use crate::config::ClusterRole;

use super::harness::{Request, send, sign_in, start_fake_control_api, start_panel};

#[tokio::test]
async fn bootstrap_credentials_sign_in_and_issue_a_locked_down_cookie() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;

    let response = send(
        fixture.address,
        Request::new("POST", "/panel/api/session").json(serde_json::json!({
            "username": "admin",
            "password": fixture.bootstrap_password,
        })),
    )
    .await;
    assert_eq!(response.status, 200);

    let cookie = response
        .headers_all("set-cookie")
        .into_iter()
        .next()
        .expect("set-cookie");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("Secure"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");

    let data = response.data();
    assert_eq!(data["username"], "admin");
    assert_eq!(data["role"], "admin");
    // The first-start credential is provisional, and the panel says so.
    assert_eq!(data["must_change_password"], true);
    assert!(data["csrf_token"].as_str().is_some_and(|t| !t.is_empty()));
}

#[tokio::test]
async fn a_wrong_password_is_indistinguishable_from_an_unknown_account() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;

    let wrong_password = send(
        fixture.address,
        Request::new("POST", "/panel/api/session")
            .json(serde_json::json!({"username": "admin", "password": "not the password"})),
    )
    .await;
    let unknown_account = send(
        fixture.address,
        Request::new("POST", "/panel/api/session")
            .json(serde_json::json!({"username": "nobody", "password": "not the password"})),
    )
    .await;

    assert_eq!(wrong_password.status, 401);
    assert_eq!(unknown_account.status, 401);
    assert_eq!(
        wrong_password.json()["error"]["code"],
        unknown_account.json()["error"]["code"]
    );
}

#[tokio::test]
async fn repeated_failures_lock_the_account_out() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;

    for _ in 0..3 {
        let response = send(
            fixture.address,
            Request::new("POST", "/panel/api/session")
                .json(serde_json::json!({"username": "admin", "password": "wrong"})),
        )
        .await;
        assert_eq!(response.status, 401);
    }
    // The correct password no longer helps while the lockout is running.
    let locked = send(
        fixture.address,
        Request::new("POST", "/panel/api/session").json(serde_json::json!({
            "username": "admin",
            "password": fixture.bootstrap_password,
        })),
    )
    .await;
    assert_eq!(locked.status, 429);
    assert_eq!(locked.json()["error"]["code"], "locked_out");
}

#[tokio::test]
async fn panel_routes_require_the_client_header_and_a_same_origin_request() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;

    let no_header = send(fixture.address, Request::bare("GET", "/panel/api/session")).await;
    assert_eq!(no_header.status, 400);
    assert_eq!(no_header.json()["error"]["code"], "missing_client_header");

    let cross_origin = send(
        fixture.address,
        Request::new("GET", "/panel/api/session").header("origin", "https://evil.example.com"),
    )
    .await;
    assert_eq!(cross_origin.status, 403);
    assert_eq!(cross_origin.json()["error"]["code"], "cross_origin");
}

#[tokio::test]
async fn a_mutation_without_the_csrf_token_is_refused() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;
    let browser = sign_in(&fixture).await;

    let without = send(
        fixture.address,
        browser.authorize(Request::new("DELETE", "/panel/api/session")),
    )
    .await;
    assert_eq!(without.status, 403);
    assert_eq!(without.json()["error"]["code"], "bad_csrf");

    let with = send(
        fixture.address,
        browser.authorize_mutation(Request::new("DELETE", "/panel/api/session")),
    )
    .await;
    assert_eq!(with.status, 200);

    // The session is gone once it is revoked.
    let after = send(
        fixture.address,
        browser.authorize(Request::new("GET", "/panel/api/session")),
    )
    .await;
    assert_eq!(after.status, 401);
}

#[tokio::test]
async fn a_provisional_password_blocks_everything_but_replacing_it() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;
    let browser = sign_in(&fixture).await;

    let blocked = send(
        fixture.address,
        browser.authorize(Request::new("GET", "/panel/api/nodes")),
    )
    .await;
    assert_eq!(blocked.status, 403);
    assert_eq!(blocked.json()["error"]["code"], "password_change_required");

    let changed = send(
        fixture.address,
        browser.authorize_mutation(Request::new("POST", "/panel/api/account/password").json(
            serde_json::json!({
                "current_password": fixture.bootstrap_password,
                "new_password": "a replacement password",
            }),
        )),
    )
    .await;
    assert_eq!(changed.status, 200, "{:?}", changed.json());

    let allowed = send(
        fixture.address,
        browser.authorize(Request::new("GET", "/panel/api/nodes")),
    )
    .await;
    assert_eq!(allowed.status, 200);
}

#[tokio::test]
async fn changing_the_password_keeps_this_session_and_ends_the_others() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;
    let first = sign_in(&fixture).await;
    let second = sign_in(&fixture).await;

    let changed = send(
        fixture.address,
        second.authorize_mutation(Request::new("POST", "/panel/api/account/password").json(
            serde_json::json!({
                "current_password": fixture.bootstrap_password,
                "new_password": "a replacement password",
            }),
        )),
    )
    .await;
    assert_eq!(changed.status, 200);
    assert_eq!(changed.data()["revoked_sessions"], 1);

    assert_eq!(
        send(
            fixture.address,
            second.authorize(Request::new("GET", "/panel/api/session"))
        )
        .await
        .status,
        200
    );
    assert_eq!(
        send(
            fixture.address,
            first.authorize(Request::new("GET", "/panel/api/session"))
        )
        .await
        .status,
        401
    );
}

#[tokio::test]
async fn a_name_that_is_not_an_account_never_reaches_the_audit_log() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;

    // The classic slip: a password pasted into the account field. Recording it
    // verbatim would turn the audit log into a store of live credentials.
    let leaked = "S3cret-Pasted-In-The-Wrong-Field";
    let response = send(
        fixture.address,
        Request::new("POST", "/panel/api/session")
            .json(serde_json::json!({"username": leaked, "password": "whatever"})),
    )
    .await;
    assert_eq!(response.status, 401);

    let records = fixture.state.audit.tail(10).await;
    let record = records
        .iter()
        .find(|record| record.result == "unknown_account")
        .expect("the attempt is audited");
    assert_eq!(record.actor, "<unknown>");
    assert!(
        !records
            .iter()
            .any(|record| format!("{record:?}").contains(leaked)),
        "no field may carry the submitted name"
    );
    // A digest still lets an operator correlate repeated attempts.
    assert!(record.detail.starts_with("submitted_name_digest="));
}

#[tokio::test]
async fn a_real_account_name_is_still_recorded() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;

    send(
        fixture.address,
        Request::new("POST", "/panel/api/session")
            .json(serde_json::json!({"username": "admin", "password": "wrong"})),
    )
    .await;

    let records = fixture.state.audit.tail(10).await;
    let record = records
        .iter()
        .find(|record| record.result == "bad_password")
        .expect("the attempt is audited");
    assert_eq!(record.actor, "admin");
    assert!(record.detail.is_empty());
}

#[tokio::test]
async fn the_bootstrap_store_holds_exactly_one_administrator() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;
    let store = fixture.state.store.read().await;
    assert_eq!(store.operators.len(), 1);
    assert_eq!(store.operators[0].username, "admin");
    assert!(store.operators[0].must_change_password);
    assert!(!store.node.id.is_empty());
    assert!(!store.node.link_key.is_empty());
}
