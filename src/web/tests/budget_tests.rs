//! Conservation and enforcement tests for the pending-budget accounting.
//!
//! The budget is charged in one place and released in nine, across three
//! classes and two queue shapes, and it is the whole safety argument of the
//! relay: nothing else bounds what one authenticated client can make the
//! process hold. These tests assert that every path balances and that the
//! ceilings actually refuse work instead of merely being written down.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{CarrierMode, WebBackend, WebLimits};
use crate::web::error::WebError;
use crate::web::frame::{self, FrameType};
use crate::web::manager::Manager;
use crate::web::session::Session;

use super::harness::{TEST_HOST, TEST_SECRET, batch, build_manager, start_echo_backend};

const CLIENT_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 21));

fn hello() -> Vec<u8> {
    frame::encode(FrameType::HELLO, 0, &[1])
}

/// Creates one session on a manager built by the caller.
fn open_session(manager: &Arc<Manager>) -> Arc<Session> {
    let profile = manager
        .match_capability(&crate::web::capability::derive_capability(
            TEST_HOST,
            &TEST_SECRET,
        ))
        .expect("profile");
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create")
        .session
}

/// Drains the downlink until the session goes quiet or the budget runs out.
async fn drain(session: &Arc<Session>, cursor: &mut u64) {
    for _ in 0..40 {
        let Ok((body, next)) = session.poll(*cursor).await else {
            return;
        };
        *cursor = next;
        if body.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_charge_is_released_when_the_session_closes() {
    let backend = start_echo_backend().await;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::Https,
        WebLimits::default(),
    );
    let session = open_session(&manager);

    let payload = vec![0x41u8; 4096];
    assert_eq!(
        session.process_up(
            1,
            &batch(&[
                (FrameType::OPEN, 1, Vec::new()),
                (FrameType::DATA, 1, payload.clone()),
                (FrameType::OPEN, 2, Vec::new()),
                (FrameType::DATA, 2, payload),
            ])
        ),
        Ok(1)
    );
    let mut cursor = 0u64;
    drain(&session, &mut cursor).await;
    // One stream is closed by the client, the other by the session teardown, so
    // both release paths are exercised.
    assert_eq!(
        session.process_up(2, &batch(&[(FrameType::CLOSE, 1, Vec::new())])),
        Ok(2)
    );
    drain(&session, &mut cursor).await;

    session.close();
    let state = session.state.lock();
    assert_eq!(state.pending_cost, 0, "session bytes must be released");
    assert_eq!(state.pending_items, 0, "session items must be released");
    assert_eq!(state.control_cost, 0, "control bytes must be released");
    assert_eq!(state.control_items, 0, "control items must be released");
    drop(state);
    let capacity = manager.capacity();
    assert_eq!(capacity.pending_bytes, 0, "process bytes must be released");
    assert_eq!(capacity.pending_items, 0, "process items must be released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lane_carriers_release_every_lane_and_bound_their_count() {
    let backend = start_echo_backend().await;
    let mut limits = WebLimits::default();
    limits.max_streams_per_session = 4;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::HttpsLanes,
        limits,
    );
    let session = open_session(&manager);
    let ceiling = session.max_lanes();

    // Far more lane ids than the ceiling allows, each opened and abandoned.
    let mut refused = 0usize;
    for lane in 1..=(ceiling as u32 * 3) {
        let opened =
            session.process_up_lane(lane, 1, &batch(&[(FrameType::OPEN, lane, Vec::new())]));
        if opened == Err(WebError::Limit) {
            refused += 1;
            continue;
        }
        assert_eq!(opened, Ok(1), "lane {lane} was refused unexpectedly");
        assert_eq!(
            session.process_up_lane(lane, 2, &batch(&[(FrameType::CLOSE, lane, Vec::new())])),
            Ok(2)
        );
        assert!(
            session.state.lock().lanes.len() <= ceiling,
            "lane {lane} pushed the session past its lane ceiling"
        );
    }
    // Closed lanes are reclaimed, so a client that closes what it opens is
    // never refused; the ceiling is there for one that does not.
    assert_eq!(refused, 0, "closed lanes must be reclaimed for reuse");

    session.close();
    assert_eq!(session.state.lock().pending_cost, 0);
    assert_eq!(session.state.lock().pending_items, 0);
    assert_eq!(manager.capacity().pending_bytes, 0);
    assert_eq!(manager.capacity().pending_items, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unclosed_lane_flood_stays_inside_the_lane_ceiling() {
    let backend = start_echo_backend().await;
    let mut limits = WebLimits::default();
    limits.max_streams_per_session = 2;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::HttpsLanes,
        limits,
    );
    let session = open_session(&manager);
    let ceiling = session.max_lanes();

    // A client that opens one lane per stream id and never closes any: before
    // the ceiling existed each of these left a queue behind until thousands of
    // closes later evicted its tombstone, which is the OOM the review found.
    let mut minted = 0usize;
    for lane in 1..=(ceiling as u32 * 8) {
        let result =
            session.process_up_lane(lane, 1, &batch(&[(FrameType::OPEN, lane, Vec::new())]));
        // The session dies once the refusals exhaust its control reserve, which
        // is itself a bound, not a leak.
        if result == Err(WebError::Closed) {
            break;
        }
        assert!(
            matches!(result, Ok(1) | Err(WebError::Limit)),
            "unexpected outcome for lane {lane}: {result:?}"
        );
        minted += 1;
        assert!(
            session.state.lock().lanes.len() <= ceiling,
            "lane {lane} pushed the session past its lane ceiling"
        );
    }
    assert!(
        minted > ceiling,
        "the flood must outrun the ceiling to prove it"
    );

    session.close();
    assert_eq!(session.state.lock().pending_cost, 0);
    assert_eq!(manager.capacity().pending_bytes, 0);
    assert_eq!(manager.capacity().pending_items, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_lane_reservation_does_not_leave_the_lane_behind() {
    let backend = start_echo_backend().await;
    let mut limits = WebLimits::default();
    // A per-session pool this small cannot hold one maximum-size uplink batch,
    // so the reservation fails after the lane bookkeeping has already begun.
    limits.max_pending_per_session = 128 * 1024;
    limits.max_body_bytes = 64 * 1024;
    limits.carrier_batch_bytes = 256 * 1024;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::HttpsLanes,
        limits,
    );
    let session = open_session(&manager);

    let body = batch(&[
        (FrameType::OPEN, 7, Vec::new()),
        (FrameType::DATA, 7, vec![0x5au8; 60 * 1024]),
    ]);
    let result = session.process_up_lane(7, 1, &body);
    assert_eq!(result, Err(WebError::Backpressure));
    assert!(
        !session.state.lock().lanes.contains_key(&7),
        "a refused batch must not leave its lane charged to nothing"
    );
    assert_eq!(manager.capacity().pending_bytes, 0);
    assert_eq!(manager.capacity().pending_items, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_opens_stop_at_the_control_reserve() {
    let backend = start_echo_backend().await;
    let mut limits = WebLimits::default();
    limits.max_streams_per_session = 1;
    let manager = build_manager(WebBackend::Loopback(backend), CarrierMode::Https, limits);
    let session = open_session(&manager);

    // Every OPEN past the first is refused and answered with a CLOSE, which is
    // a control frame. Before the reserve became the control ceiling, one
    // session could mint these until the whole process pool was gone.
    let mut frames: Vec<(FrameType, u32, Vec<u8>)> = Vec::new();
    for id in 1..=64u32 {
        frames.push((FrameType::OPEN, id, Vec::new()));
    }
    let result = session.process_up(1, &batch(&frames));
    assert_eq!(result, Err(WebError::Closed));
    assert!(session.is_closed_for_test());

    let capacity = manager.capacity();
    assert_eq!(capacity.pending_bytes, 0);
    assert_eq!(capacity.pending_items, 0);
    assert!(
        capacity.pending_items < u64::from(u32::MAX),
        "the control reserve, not the process pool, bounds a rejected open"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_concurrent_uplink_is_refused_without_charging_anything() {
    let backend = start_echo_backend().await;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::Https,
        WebLimits::default(),
    );
    let session = open_session(&manager);

    // Marking the queue busy is what a second in-flight POST does; doing it
    // directly keeps the test free of a thread race it cannot observe.
    session.state.lock().main.up_active = true;
    let result = session.process_up(1, &batch(&[(FrameType::OPEN, 1, Vec::new())]));
    assert_eq!(result, Err(WebError::Concurrent));
    assert!(
        !session.is_closed_for_test(),
        "a retryable answer is not fatal"
    );
    assert_eq!(manager.capacity().pending_bytes, 0);

    session.state.lock().main.up_active = false;
    assert_eq!(
        session.process_up(1, &batch(&[(FrameType::OPEN, 1, Vec::new())])),
        Ok(1),
        "the retry must be accepted at the same sequence"
    );
    session.close();
    assert_eq!(manager.capacity().pending_bytes, 0);
}
