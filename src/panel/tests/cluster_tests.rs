//! The signed node-to-node endpoint.

use crate::config::ClusterRole;
use crate::panel::cluster::sign::{
    HEADER_NODE, HEADER_NONCE, HEADER_SIGNATURE, HEADER_TIMESTAMP, SignedRequest,
};
use crate::panel::crypto::{decode, encode};
use crate::panel::state::unix_now_ms;

use super::harness::{PanelFixture, Request, send, start_fake_control_api, start_panel};

/// Builds a signed cluster request against the fixture's own identity.
async fn signed(fixture: &PanelFixture, method: &str, path: &str, body: Vec<u8>) -> Request {
    let (node_id, link_key) = {
        let store = fixture.state.store.read().await;
        (store.node.id.clone(), store.node.link_key.clone())
    };
    let key = decode(&link_key).expect("link key decodes");
    let nonce = encode(&[7u8; 32]);
    let timestamp = unix_now_ms();
    let signature = SignedRequest {
        method,
        path,
        node_id: &node_id,
        timestamp_ms: timestamp,
        nonce: &nonce,
        body: &body,
    }
    .sign(&key);
    Request::new(method, path)
        .header(HEADER_NODE, &node_id)
        .header(HEADER_TIMESTAMP, &timestamp.to_string())
        .header(HEADER_NONCE, &nonce)
        .header(HEADER_SIGNATURE, &signature)
        .body(body)
}

#[tokio::test]
async fn a_standalone_node_exposes_no_cluster_endpoint() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Standalone).await;

    let request = signed(&fixture, "GET", "/cluster/v1/hello", Vec::new()).await;
    let response = send(fixture.address, request).await;
    assert_eq!(response.status, 404);
}

#[tokio::test]
async fn an_agent_answers_a_correctly_signed_hello() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Agent).await;

    let request = signed(&fixture, "GET", "/cluster/v1/hello", Vec::new()).await;
    let response = send(fixture.address, request).await;
    assert_eq!(response.status, 200, "{:?}", response.json());

    let payload = response.json();
    let store = fixture.state.store.read().await;
    assert_eq!(payload["node_id"], store.node.id.as_str());
    assert_eq!(payload["role"], "agent");
    assert_eq!(payload["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn an_unsigned_cluster_request_is_refused() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Agent).await;

    let response = send(fixture.address, Request::new("GET", "/cluster/v1/hello")).await;
    assert_eq!(response.status, 401);
    assert_eq!(
        response.json()["error"]["code"],
        "malformed_signature_headers"
    );
}

#[tokio::test]
async fn a_tampered_body_invalidates_the_signature() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Agent).await;

    let request = signed(
        &fixture,
        "POST",
        "/cluster/v1/control/v1/users",
        br#"{"username":"alice"}"#.to_vec(),
    )
    .await;
    let tampered = Request {
        body: br#"{"username":"mallory"}"#.to_vec(),
        ..request
    };
    let response = send(fixture.address, tampered).await;
    assert_eq!(response.status, 401);
    assert_eq!(response.json()["error"]["code"], "bad_signature");
    assert!(control.requests.lock().is_empty());
}

#[tokio::test]
async fn a_replayed_request_is_refused_the_second_time() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Agent).await;

    let first = signed(&fixture, "GET", "/cluster/v1/hello", Vec::new()).await;
    let node = first
        .headers
        .iter()
        .find(|(name, _)| name == HEADER_NODE)
        .map(|(_, value)| value.clone())
        .expect("node header");
    let timestamp = first
        .headers
        .iter()
        .find(|(name, _)| name == HEADER_TIMESTAMP)
        .map(|(_, value)| value.clone())
        .expect("timestamp header");
    let nonce = first
        .headers
        .iter()
        .find(|(name, _)| name == HEADER_NONCE)
        .map(|(_, value)| value.clone())
        .expect("nonce header");
    let signature = first
        .headers
        .iter()
        .find(|(name, _)| name == HEADER_SIGNATURE)
        .map(|(_, value)| value.clone())
        .expect("signature header");

    assert_eq!(send(fixture.address, first).await.status, 200);

    let replay = Request::new("GET", "/cluster/v1/hello")
        .header(HEADER_NODE, &node)
        .header(HEADER_TIMESTAMP, &timestamp)
        .header(HEADER_NONCE, &nonce)
        .header(HEADER_SIGNATURE, &signature);
    let response = send(fixture.address, replay).await;
    assert_eq!(response.status, 401);
    assert_eq!(response.json()["error"]["code"], "replayed_nonce");
}

#[tokio::test]
async fn an_agent_replays_a_signed_control_request_against_its_own_api() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::MasterAgent).await;

    let request = signed(
        &fixture,
        "GET",
        "/cluster/v1/control/v1/stats/summary?limit=5",
        Vec::new(),
    )
    .await;
    let response = send(fixture.address, request).await;
    assert_eq!(response.status, 200, "{:?}", response.json());

    let recorded = control.requests.lock().clone();
    let call = recorded.last().expect("a control api call");
    assert_eq!(call.method, "GET");
    assert_eq!(call.target, "/v1/stats/summary?limit=5");
}

#[tokio::test]
async fn the_cluster_endpoint_relays_only_the_control_surface() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Agent).await;

    let request = signed(&fixture, "GET", "/cluster/v1/control/metrics", Vec::new()).await;
    let response = send(fixture.address, request).await;
    assert_eq!(response.status, 404);
    assert!(control.requests.lock().is_empty());
}

#[tokio::test]
async fn a_signature_for_one_path_does_not_authorise_another() {
    let control = start_fake_control_api().await;
    let fixture = start_panel(&control, ClusterRole::Agent).await;

    let request = signed(&fixture, "GET", "/cluster/v1/hello", Vec::new()).await;
    let moved = Request {
        target: "/cluster/v1/control/v1/users".to_string(),
        ..request
    };
    let response = send(fixture.address, moved).await;
    assert_eq!(response.status, 401);
    assert_eq!(response.json()["error"]["code"], "bad_signature");
}
