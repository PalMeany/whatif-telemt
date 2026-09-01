//! The local node's link token: what an operator carries to a master.
//!
//! Kept apart from the node list because it is the one route that hands out
//! credential material, and its guards are its own: the agent role, the
//! administrator permission, and an advertise URL a master can actually dial.

use std::sync::Arc;

use hyper::{Response, StatusCode};

use crate::panel::cluster::link::LinkToken;
use crate::panel::crypto::{encode, random_secret};
use crate::panel::rbac::Permission;
use crate::panel::state::PanelState;

use super::Caller;
use super::account_routes::internal;
use super::node_routes::record_audit;
use super::respond::{self, PanelBody};

/// Returns the token an operator pastes into a master.
pub(crate) async fn link_token(caller: &Caller, state: &Arc<PanelState>) -> Response<PanelBody> {
    if let Err(response) = caller.require(Permission::ManageNodes) {
        return response;
    }
    if !state.cluster_role().is_agent() {
        return respond::error(
            StatusCode::CONFLICT,
            "not_an_agent",
            "Set panel.cluster.role to agent or master-agent to be linkable",
        );
    }
    let store = state.store.read().await;
    let url = if state.config.cluster.advertise_url.is_empty() {
        String::new()
    } else {
        state
            .config
            .cluster
            .advertise_url
            .trim_end_matches('/')
            .to_string()
    };
    if url.is_empty() {
        return respond::error(
            StatusCode::CONFLICT,
            "no_advertise_url",
            "Set panel.cluster.advertise_url so a master knows how to reach this node",
        );
    }
    let fingerprint = crate::panel::tls::current_fingerprint();
    let token = LinkToken::new(
        store.node.id.clone(),
        store.node.name.clone(),
        url.clone(),
        store.node.link_key.clone(),
        fingerprint.clone(),
    );
    respond::json(
        StatusCode::OK,
        serde_json::json!({
            "token": token.render(),
            "node_id": store.node.id,
            "node_name": store.node.name,
            "url": url,
            "fingerprint": fingerprint,
        }),
    )
}

/// Mints a fresh link key, invalidating every existing link to this node.
pub(crate) async fn rotate_link_key(caller: Caller, state: Arc<PanelState>) -> Response<PanelBody> {
    if let Err(response) = caller.require(Permission::ManageNodes) {
        return response;
    }
    let key = encode(&random_secret(&state.random));
    {
        let mut store = state.store.write().await;
        store.node.link_key = key;
    }
    if let Err(error) = state.persist().await {
        return internal(error.to_string());
    }
    record_audit(
        &state,
        &caller,
        "node.link_key.rotate",
        "self",
        "ok",
        String::new(),
    )
    .await;
    respond::json(
        StatusCode::OK,
        serde_json::json!({"rotated": true, "relink_required": true}),
    )
}
