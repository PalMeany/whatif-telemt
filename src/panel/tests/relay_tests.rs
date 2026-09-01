//! The Control API relay: what reaches the node, and who may send it.

use crate::config::ClusterRole;
use crate::panel::rbac::Role;

use super::harness::{
    Browser, PanelFixture, Request, send, sign_in, start_fake_control_api, start_panel,
};

/// Signs in and clears the provisional-password gate so relay routes are open.
async fn ready_browser(fixture: &PanelFixture) -> Browser {
    let browser = sign_in(fixture).await;
    let response = send(
        fixture.address,
        browser.authorize_mutation(Request::new("POST", "/panel/api/account/password").json(
            serde_json::json!({
                "current_password": fixture.bootstrap_password,
                "new_password": "a replacement password",
            }),
        )),
    )
    .await;
    assert_eq!(response.status, 200);
    browser
}

/// Demotes the signed-in administrator to the given role.
async fn demote(fixture: &PanelFixture, role: Role) {
    let mut store = fixture.state.store.write().await;
    store.operators[0].role = role;
}

#[tokio::test]
async fn a_read_reaches_the_control_api_with_its_authorization() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;
    let browser = ready_browser(&fixture).await;

    let response = send(
        fixture.address,
        browser.authorize(Request::new("GET", "/panel/api/control/v1/stats/summary")),
    )
    .await;
    assert_eq!(response.status, 200);
    // The Control API envelope is relayed whole, revision included.
    assert_eq!(response.json()["revision"], "rev");

    let recorded = control.requests.lock().clone();
    let call = recorded.last().expect("a control api call");
    assert_eq!(call.method, "GET");
    assert_eq!(call.target, "/v1/stats/summary");
    assert_eq!(call.authorization.as_deref(), Some("Bearer control-token"));
}

#[tokio::test]
async fn the_node_selector_never_reaches_the_control_api() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;
    let browser = ready_browser(&fixture).await;

    let response = send(
        fixture.address,
        browser.authorize(Request::new(
            "GET",
            "/panel/api/control/v1/runtime/events/recent?limit=25&node=local",
        )),
    )
    .await;
    assert_eq!(response.status, 200);

    let recorded = control.requests.lock().clone();
    let call = recorded.last().expect("a control api call");
    assert_eq!(call.target, "/v1/runtime/events/recent?limit=25");
}

#[tokio::test]
async fn a_mutation_is_forwarded_with_its_body() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;
    let browser = ready_browser(&fixture).await;

    let response = send(
        fixture.address,
        browser.authorize_mutation(
            Request::new("POST", "/panel/api/control/v1/users?node=local")
                .json(serde_json::json!({"username": "alice"})),
        ),
    )
    .await;
    assert_eq!(response.status, 200);

    let recorded = control.requests.lock().clone();
    let call = recorded.last().expect("a control api call");
    assert_eq!(call.method, "POST");
    assert_eq!(call.target, "/v1/users");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&call.body).expect("body"),
        serde_json::json!({"username": "alice"})
    );
}

#[tokio::test]
async fn only_the_versioned_control_surface_is_relayed() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;
    let browser = ready_browser(&fixture).await;

    for target in [
        "/panel/api/control/metrics",
        "/panel/api/control/v2/health",
        "/panel/api/control/v1/../metrics",
    ] {
        let response = send(
            fixture.address,
            browser.authorize(Request::new("GET", target)),
        )
        .await;
        assert_eq!(response.status, 404, "{target} should not be relayed");
    }
    assert!(
        control.requests.lock().is_empty(),
        "no refused target may reach the control api"
    );
}

#[tokio::test]
async fn a_viewer_reads_but_cannot_change_users() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;
    let browser = ready_browser(&fixture).await;
    demote(&fixture, Role::Viewer).await;

    let read = send(
        fixture.address,
        browser.authorize(Request::new("GET", "/panel/api/control/v1/users")),
    )
    .await;
    assert_eq!(read.status, 200);

    let write = send(
        fixture.address,
        browser.authorize_mutation(
            Request::new("POST", "/panel/api/control/v1/users")
                .json(serde_json::json!({"username": "alice"})),
        ),
    )
    .await;
    assert_eq!(write.status, 403);
    assert_eq!(write.json()["error"]["code"], "forbidden");
}

#[tokio::test]
async fn an_operator_manages_users_but_not_configuration() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;
    let browser = ready_browser(&fixture).await;
    demote(&fixture, Role::Operator).await;

    let users = send(
        fixture.address,
        browser.authorize_mutation(
            Request::new("POST", "/panel/api/control/v1/users")
                .json(serde_json::json!({"username": "alice"})),
        ),
    )
    .await;
    assert_eq!(users.status, 200);

    for (method, target) in [
        ("PATCH", "/panel/api/control/v1/config"),
        ("POST", "/panel/api/control/v1/system/reload"),
    ] {
        let response = send(
            fixture.address,
            browser.authorize_mutation(
                Request::new(method, target).json(serde_json::json!({"general": {}})),
            ),
        )
        .await;
        assert_eq!(response.status, 403, "{method} {target}");
    }
}

#[tokio::test]
async fn an_unlinked_node_is_refused_rather_than_silently_served_locally() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Master).await;
    let browser = ready_browser(&fixture).await;

    let response = send(
        fixture.address,
        browser.authorize(Request::new(
            "GET",
            "/panel/api/control/v1/stats/summary?node=node-does-not-exist",
        )),
    )
    .await;
    assert_eq!(response.status, 404);
    assert_eq!(response.json()["error"]["code"], "unknown_node");
    assert!(control.requests.lock().is_empty());
}

#[tokio::test]
async fn naming_another_node_on_a_standalone_panel_reports_federation_off() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;
    let browser = ready_browser(&fixture).await;

    let response = send(
        fixture.address,
        browser.authorize(Request::new(
            "GET",
            "/panel/api/control/v1/stats/summary?node=node-elsewhere",
        )),
    )
    .await;
    assert_eq!(response.status, 409);
    assert_eq!(response.json()["error"]["code"], "federation_disabled");
}

#[tokio::test]
async fn a_relayed_mutation_lands_in_the_audit_log() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;
    let browser = ready_browser(&fixture).await;

    send(
        fixture.address,
        browser.authorize_mutation(
            Request::new("POST", "/panel/api/control/v1/users")
                .json(serde_json::json!({"username": "alice"})),
        ),
    )
    .await;

    let records = fixture.state.audit.tail(20).await;
    let relayed = records
        .iter()
        .find(|record| record.action == "control.post")
        .expect("the relayed mutation is audited");
    assert_eq!(relayed.target, "/v1/users");
    assert_eq!(relayed.node, "local");
    assert_eq!(relayed.actor, "admin");

    // Reads are deliberately not recorded, or the log would be unreadable.
    send(
        fixture.address,
        browser.authorize(Request::new("GET", "/panel/api/control/v1/users")),
    )
    .await;
    let records = fixture.state.audit.tail(20).await;
    assert!(records.iter().all(|record| record.action != "control.get"));
    assert!(fixture.state.audit.verify().await.valid);
}
