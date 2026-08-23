//! Bootstrap, session, and capability lifecycle under real timeouts.
//!
//! Every ceiling here is measured in minutes in production, so a fixture that
//! keeps the default timeouts can never reach the reaper, the bootstrap
//! eviction, or the closed-token window — which are exactly the paths that
//! mutate the counters the DoS ceilings read.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{CarrierMode, WebBackend, WebLimits, WebProfileLimits, WebTimeouts};
use crate::web::capability::derive_capability;
use crate::web::error::WebError;
use crate::web::frame::{self, FrameType};
use crate::web::manager::{Manager, WebProfile};

use super::harness::{TEST_HOST, TEST_SECRET, build_manager_with_timeouts, start_echo_backend};

const CLIENT_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 31));

fn hello() -> Vec<u8> {
    frame::encode(FrameType::HELLO, 0, &[1])
}

/// Timeouts short enough that a test can actually reach the reaper.
fn brief_timeouts() -> WebTimeouts {
    WebTimeouts {
        reconnect_grace_ms: 1,
        bootstrap_lifetime_ms: 1,
        ..WebTimeouts::default()
    }
}

async fn manager_with(limits: WebLimits, timeouts: WebTimeouts) -> Arc<Manager> {
    let backend = start_echo_backend().await;
    build_manager_with_timeouts(
        WebBackend::Loopback(backend),
        CarrierMode::Https,
        limits,
        timeouts,
    )
}

fn profile_of(manager: &Arc<Manager>) -> Arc<WebProfile> {
    manager
        .match_capability(&derive_capability(TEST_HOST, &TEST_SECRET))
        .expect("profile")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reaper_closes_a_session_past_its_reconnect_grace() {
    let manager = manager_with(WebLimits::default(), brief_timeouts()).await;
    let profile = profile_of(&manager);
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let session = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create")
        .session;
    assert_eq!(manager.capacity().sessions, 1);

    tokio::time::sleep(Duration::from_millis(20)).await;
    manager.reap();
    assert!(
        session.is_closed_for_test(),
        "an idle session must be reaped"
    );
    assert_eq!(manager.capacity().sessions, 0);
    assert_eq!(manager.capacity().pending_bytes, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_bootstrap_is_dropped_and_gives_its_per_address_slot_back() {
    let mut limits = WebLimits::default();
    limits.max_bootstraps_per_ip = 1;
    let manager = manager_with(limits, brief_timeouts()).await;
    let profile = profile_of(&manager);

    let first = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    // The one slot this address has is taken while the bootstrap is live.
    assert_eq!(
        manager.issue_bootstrap(&profile, CLIENT_IP).err(),
        Some(WebError::Limit)
    );

    tokio::time::sleep(Duration::from_millis(20)).await;
    manager.reap();
    assert!(
        !manager.has_bootstrap(&first),
        "an expired bootstrap is gone"
    );
    assert!(
        manager.issue_bootstrap(&profile, CLIENT_IP).is_ok(),
        "expiry must return the per-address slot"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_bootstrap_table_evicts_the_oldest_unredeemed_entry() {
    let mut limits = WebLimits::default();
    limits.max_bootstraps_global = 2;
    let manager = manager_with(limits, WebTimeouts::default()).await;
    let profile = profile_of(&manager);

    let oldest = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("first bootstrap");
    let middle = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("second bootstrap");
    let newest = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("third bootstrap");

    assert!(
        !manager.has_bootstrap(&oldest),
        "the oldest must be evicted"
    );
    assert!(manager.has_bootstrap(&middle));
    assert!(manager.has_bootstrap(&newest));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_token_stops_being_accepted_once_its_window_expires() {
    let manager = manager_with(WebLimits::default(), brief_timeouts()).await;
    let profile = profile_of(&manager);
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let created = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create");

    manager.close_token(&created.token).expect("close");
    // Immediately after, the bearer is still remembered so a client retrying
    // its own DELETE is not told the session never existed.
    manager.close_token(&created.token).expect("close again");

    tokio::time::sleep(Duration::from_millis(20)).await;
    manager.reap();
    assert_eq!(
        manager.close_token(&created.token).err(),
        Some(WebError::Authentication),
        "the closed-token window must expire"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_per_address_session_ceiling_counts_an_ipv6_client_per_prefix() {
    let mut limits = WebLimits::default();
    limits.max_sessions_per_ip = 1;
    let manager = manager_with(limits, WebTimeouts::default()).await;
    let profile = profile_of(&manager);

    let first: IpAddr = "2001:db8::1".parse().expect("ip");
    let sibling: IpAddr = "2001:db8::dead:beef".parse().expect("ip");
    let elsewhere = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 1));

    let bootstrap = manager.issue_bootstrap(&profile, first).expect("bootstrap");
    manager.create(&bootstrap, first, &hello()).expect("create");

    // A different address inside the same /64 is the same subscriber.
    let bootstrap = manager
        .issue_bootstrap(&profile, sibling)
        .expect("bootstrap");
    assert_eq!(
        manager.create(&bootstrap, sibling, &hello()).err(),
        Some(WebError::Limit),
        "an IPv6 client must not walk its own /64 past the ceiling"
    );

    // A different /64 is a different client and is admitted.
    let bootstrap = manager
        .issue_bootstrap(&profile, elsewhere)
        .expect("bootstrap");
    assert!(manager.create(&bootstrap, elsewhere, &hello()).is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reconnecting_client_is_not_locked_out_by_its_own_dead_sessions() {
    // The per-address ceiling counts live sessions, and an abandoned one stays
    // live for the whole reconnect grace. A client whose network flaps must not
    // fill its own ceiling with its own corpses: that reads as "the proxy does
    // not work on the first try, but works after switching away and back".
    let mut limits = WebLimits::default();
    limits.max_sessions_per_ip = 1;
    let timeouts = WebTimeouts {
        // A carrier is considered abandoned once it has been silent for the
        // reconnect grace. That is deliberately wider than the long-poll
        // period, because a WebSocket carrier is kept alive by protocol
        // ping/pong rather than by a poll.
        reconnect_grace_ms: 10,
        ..WebTimeouts::default()
    };
    let manager = manager_with(limits, timeouts).await;
    let profile = profile_of(&manager);

    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let abandoned = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create")
        .session;

    // The client goes away without a clean DELETE and comes back.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("second bootstrap");
    let reconnected = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("a reconnect must displace the client's own dead session");

    assert!(
        abandoned.is_closed_for_test(),
        "the corpse must be reclaimed"
    );
    assert!(!reconnected.session.is_closed_for_test());
    assert_eq!(manager.capacity().sessions, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_session_keeps_its_slot_against_a_neighbour() {
    // Behind a carrier NAT the bucket is shared, so reclaiming must never take
    // a session that is still driving its carrier.
    let mut limits = WebLimits::default();
    limits.max_sessions_per_ip = 1;
    let manager = manager_with(limits, WebTimeouts::default()).await;
    let profile = profile_of(&manager);

    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let live = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create")
        .session;

    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("second bootstrap");
    assert_eq!(
        manager.create(&bootstrap, CLIENT_IP, &hello()).err(),
        Some(WebError::Limit),
        "an active session must not be displaced"
    );
    assert!(!live.is_closed_for_test());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotating_a_secret_closes_the_sessions_it_opened() {
    let manager = manager_with(WebLimits::default(), WebTimeouts::default()).await;
    let profile = profile_of(&manager);
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let session = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create")
        .session;

    // A reload that changes nothing must not disturb a live session.
    let unchanged = Arc::new(WebProfile {
        name: profile.name.clone(),
        backend: profile.backend,
        carrier: profile.carrier,
        capabilities: profile.capabilities.clone(),
        limits: WebProfileLimits::default().with_defaults(&manager.limits),
    });
    manager
        .replace_profiles(vec![unchanged])
        .expect("identical reload");
    assert!(!session.is_closed_for_test());

    // Rotating the secret revokes the capability the session was created from.
    let rotated = Arc::new(WebProfile {
        name: profile.name.clone(),
        backend: profile.backend,
        carrier: profile.carrier,
        capabilities: vec![derive_capability(TEST_HOST, &[0xAAu8; 16])],
        limits: WebProfileLimits::default().with_defaults(&manager.limits),
    });
    manager.replace_profiles(vec![rotated]).expect("rotation");
    assert!(
        session.is_closed_for_test(),
        "a rotated secret must not leave its sessions relaying"
    );
    assert_eq!(manager.capacity().sessions, 0);
    assert_eq!(manager.capacity().pending_bytes, 0);
}
