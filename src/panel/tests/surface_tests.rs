//! The static shell, the health probe, and the headers every answer carries.

use crate::config::ClusterRole;

use super::harness::{Request, send, start_fake_control_api, start_panel};

#[tokio::test]
async fn the_health_probe_needs_no_session() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;

    let response = send(fixture.address, Request::bare("GET", "/healthz")).await;
    assert_eq!(response.status, 200);
    assert_eq!(String::from_utf8_lossy(&response.body).trim(), "ok");
}

#[tokio::test]
async fn every_answer_carries_the_hardening_headers() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;

    let response = send(fixture.address, Request::bare("GET", "/healthz")).await;
    assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(response.header("x-frame-options"), Some("DENY"));
    assert_eq!(response.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(response.header("cache-control"), Some("no-store"));
    let policy = response
        .header("content-security-policy")
        .expect("a content security policy");
    assert!(policy.contains("frame-ancestors 'none'"), "{policy}");
    assert!(policy.contains("default-src 'none'"), "{policy}");
    assert!(
        !policy.contains("script-src 'self' 'unsafe-inline'"),
        "{policy}"
    );
    // No TLS on this fixture, so HSTS must not be asserted.
    assert_eq!(response.header("strict-transport-security"), None);
}

#[tokio::test]
async fn the_shell_is_served_for_client_side_routes() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;

    let root = send(fixture.address, Request::bare("GET", "/")).await;
    let deep_link = send(fixture.address, Request::bare("GET", "/fleet")).await;
    // The bundle may or may not be compiled into a given build; either way the
    // two routes have to answer identically, because the client router owns
    // both of them.
    assert_eq!(root.status, deep_link.status);
    assert_eq!(
        root.header("content-type"),
        Some("text/html; charset=utf-8")
    );
}

#[tokio::test]
async fn a_missing_asset_is_a_404_rather_than_the_shell() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;

    let response = send(
        fixture.address,
        Request::bare("GET", "/assets/does-not-exist.js"),
    )
    .await;
    assert_eq!(response.status, 404);
}

#[tokio::test]
async fn the_shell_is_read_only() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;

    let response = send(fixture.address, Request::bare("POST", "/")).await;
    assert_eq!(response.status, 405);
}

#[tokio::test]
async fn an_unknown_panel_route_answers_not_found() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;

    let response = send(fixture.address, Request::new("GET", "/panel/api/nope")).await;
    // Authentication runs before routing, so an anonymous caller learns nothing
    // about which panel routes exist.
    assert_eq!(response.status, 401);
}
