//! End-to-end session tests over a loopback backend.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

use crate::config::fork::web::{CarrierMode, WebBackend, WebLimits, WebTimeouts};
use crate::fork::web::error::WebError;
use crate::fork::web::frame::{self, FrameType};
use crate::fork::web::manager::Manager;
use crate::fork::web::session::Session;

use super::harness::{
    TEST_HOST, TEST_SECRET, batch, build_manager, build_manager_with_timeouts, data_payloads,
    has_close, start_echo_backend,
};

const CLIENT_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));

fn hello() -> Vec<u8> {
    frame::encode(FrameType::HELLO, 0, &[1])
}

/// Creates one session on a manager the caller built.
fn open_session(manager: &Arc<Manager>) -> Arc<Session> {
    let profile = manager
        .match_capability(&crate::fork::web::capability::derive_capability(
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

/// Polls until the expected payload arrives or the attempt budget runs out.
async fn poll_until(
    session: &std::sync::Arc<crate::fork::web::session::Session>,
    cursor: &mut u64,
    wanted: usize,
    stream_id: u32,
) -> Vec<u8> {
    let mut collected = Vec::new();
    for _ in 0..40 {
        let (body, next) = session.poll(*cursor).await.expect("poll");
        *cursor = next;
        collected.extend_from_slice(&data_payloads(&body, stream_id));
        if collected.len() >= wanted {
            return collected;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    collected
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relays_bytes_through_the_loopback_backend() {
    let backend = start_echo_backend().await;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::Https,
        WebLimits::default(),
    );
    let profile = manager
        .match_capability(&crate::fork::web::capability::derive_capability(
            TEST_HOST,
            &TEST_SECRET,
        ))
        .expect("profile");
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let created = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create");
    let session = created.session.clone();

    let payload = b"mtproto-obfuscated-bytes".to_vec();
    let uplink = batch(&[
        (FrameType::OPEN, 1, Vec::new()),
        (FrameType::DATA, 1, payload.clone()),
    ]);
    assert_eq!(session.process_up(1, &uplink), Ok(1));

    let mut cursor = 0u64;
    let echoed = poll_until(&session, &mut cursor, payload.len(), 1).await;
    assert_eq!(echoed, payload);

    // Closing the stream must not close the session or its siblings.
    let close = batch(&[(FrameType::CLOSE, 1, Vec::new())]);
    assert_eq!(session.process_up(2, &close), Ok(2));
    assert_eq!(manager.capacity().sessions, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backend_eof_closes_only_that_stream() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    let manager = build_manager(
        WebBackend::Loopback(address),
        CarrierMode::Https,
        WebLimits::default(),
    );
    let profile = manager
        .match_capability(&crate::fork::web::capability::derive_capability(
            TEST_HOST,
            &TEST_SECRET,
        ))
        .expect("profile");
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let session = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create")
        .session;

    let uplink = batch(&[(FrameType::OPEN, 5, Vec::new())]);
    assert_eq!(session.process_up(1, &uplink), Ok(1));

    let mut cursor = 0u64;
    let mut closed = false;
    for _ in 0..40 {
        let (body, next) = session.poll(cursor).await.expect("poll");
        cursor = next;
        if has_close(&body, 5) {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(closed, "relay must report the backend close");
    assert!(!session.is_closed_for_test());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn internal_backend_runs_the_stream_through_the_client_pipeline() {
    let manager = build_manager(
        WebBackend::Internal,
        CarrierMode::Https,
        WebLimits::default(),
    );
    let profile = manager
        .match_capability(&crate::fork::web::capability::derive_capability(
            TEST_HOST,
            &TEST_SECRET,
        ))
        .expect("profile");
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let session = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create")
        .session;

    // The payload is not a valid MTProto handshake, so the in-process client
    // pipeline rejects it: the stream must close while the session survives.
    let uplink = batch(&[
        (FrameType::OPEN, 9, Vec::new()),
        (FrameType::DATA, 9, vec![0x11; 64]),
    ]);
    assert_eq!(session.process_up(1, &uplink), Ok(1));

    let mut cursor = 0u64;
    let mut closed = false;
    let mut granted_window = false;
    for _ in 0..80 {
        let (body, next) = session.poll(cursor).await.expect("poll");
        cursor = next;
        if !body.is_empty() {
            for value in frame::parse_all(&body, frame::MAX_PAYLOAD).expect("parse") {
                if value.kind == FrameType::WINDOW && value.stream_id == 9 {
                    granted_window = true;
                }
                if value.kind == FrameType::CLOSE && value.stream_id == 9 {
                    closed = true;
                }
            }
        }
        if closed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        granted_window,
        "consumed uplink bytes must return WINDOW credit"
    );
    assert!(closed, "a rejected stream must be closed");
    assert!(!session.is_closed_for_test());
    // The stream permit is released when the backend task finishes, which can
    // trail the CLOSE frame by a scheduling tick.
    for _ in 0..40 {
        if manager.capacity().streams == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(manager.capacity().streams, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uplink_replay_is_idempotent_and_gaps_are_fatal() {
    let backend = start_echo_backend().await;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::Https,
        WebLimits::default(),
    );
    let profile = manager
        .match_capability(&crate::fork::web::capability::derive_capability(
            TEST_HOST,
            &TEST_SECRET,
        ))
        .expect("profile");
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let session = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create")
        .session;

    let uplink = batch(&[(FrameType::OPEN, 1, Vec::new())]);
    assert_eq!(session.process_up(1, &uplink), Ok(1));
    // A byte-identical retry of the committed sequence is acknowledged again.
    assert_eq!(session.process_up(1, &uplink), Ok(1));
    // A different body for the same sequence is a protocol violation.
    let different = batch(&[(FrameType::DATA, 1, b"x".to_vec())]);
    assert_eq!(session.process_up(1, &different), Err(WebError::Protocol));
    assert!(session.is_closed_for_test());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn downlink_cursor_replays_the_same_batch() {
    let backend = start_echo_backend().await;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::Https,
        WebLimits::default(),
    );
    let profile = manager
        .match_capability(&crate::fork::web::capability::derive_capability(
            TEST_HOST,
            &TEST_SECRET,
        ))
        .expect("profile");
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let session = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create")
        .session;

    let uplink = batch(&[
        (FrameType::OPEN, 3, Vec::new()),
        (FrameType::DATA, 3, b"replay-me".to_vec()),
    ]);
    assert_eq!(session.process_up(1, &uplink), Ok(1));

    let mut first = Vec::new();
    let mut cursor = 0u64;
    let mut delivered = 0u64;
    for _ in 0..40 {
        let (body, next) = session.poll(cursor).await.expect("poll");
        if !body.is_empty() {
            first = body.to_vec();
            delivered = next;
            break;
        }
        cursor = next;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!first.is_empty(), "expected a downlink batch");
    // Repeating the old cursor replays the unacknowledged batch byte-for-byte.
    let (replay, next) = session.poll(cursor).await.expect("replay");
    assert_eq!(replay.as_ref(), first.as_slice());
    assert_eq!(next, delivered);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lane_carrier_keeps_streams_independent() {
    let backend = start_echo_backend().await;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::HttpsLanes,
        WebLimits::default(),
    );
    let profile = manager
        .match_capability(&crate::fork::web::capability::derive_capability(
            TEST_HOST,
            &TEST_SECRET,
        ))
        .expect("profile");
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let session = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create")
        .session;

    // Each lane owns its own uplink sequence numbering.
    let lane_one = batch(&[
        (FrameType::OPEN, 1, Vec::new()),
        (FrameType::DATA, 1, b"one".to_vec()),
    ]);
    let lane_two = batch(&[
        (FrameType::OPEN, 2, Vec::new()),
        (FrameType::DATA, 2, b"two".to_vec()),
    ]);
    assert_eq!(session.process_up_lane(1, 1, &lane_one), Ok(1));
    assert_eq!(session.process_up_lane(2, 1, &lane_two), Ok(1));

    for (lane, expected) in [(1u32, b"one".as_ref()), (2u32, b"two".as_ref())] {
        let mut cursor = 0u64;
        let mut collected = Vec::new();
        for _ in 0..40 {
            let (body, next, _) = session.poll_lane(lane, cursor).await.expect("poll lane");
            cursor = next;
            collected.extend_from_slice(&data_payloads(&body, lane));
            if collected.len() >= expected.len() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(collected, expected);
    }

    // A cross-lane frame closes the whole session on an `https-lanes` carrier.
    // `PROTOCOL.md` scopes lane-only teardown to `websocket-lanes`; here the
    // frame is a violation of the shared session grammar, and the reference
    // ends the session for it.
    let cross = batch(&[(FrameType::DATA, 2, b"bad".to_vec())]);
    assert_eq!(
        session.process_up_lane(1, 2, &cross),
        Err(WebError::Protocol)
    );
    assert!(
        session.is_closed_for_test(),
        "a cross-lane frame is a session-level protocol failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_creation_is_idempotent_for_one_bootstrap() {
    let backend = start_echo_backend().await;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::Https,
        WebLimits::default(),
    );
    let profile = manager
        .match_capability(&crate::fork::web::capability::derive_capability(
            TEST_HOST,
            &TEST_SECRET,
        ))
        .expect("profile");
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let first = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create");
    let second = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("retry");
    assert_eq!(first.token, second.token);
    assert_eq!(manager.capacity().sessions, 1);

    // A different creation body for a consumed bootstrap is rejected.
    let other = frame::encode(FrameType::HELLO, 0, &[1]);
    let mut tampered = other.clone();
    tampered.extend_from_slice(&frame::encode(FrameType::PONG, 0, &[]));
    assert_eq!(
        manager
            .create(&bootstrap, CLIENT_IP, &tampered)
            .err()
            .map(|_| ()),
        Some(())
    );

    manager.close_token(&first.token).expect("close");
    // Deleting an already closed session stays idempotent.
    manager.close_token(&first.token).expect("close again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_limit_rejects_extra_streams_without_killing_the_session() {
    let backend = start_echo_backend().await;
    let mut limits = WebLimits::default();
    limits.max_streams_per_session = 1;
    let manager = build_manager(WebBackend::Loopback(backend), CarrierMode::Https, limits);
    let profile = manager
        .match_capability(&crate::fork::web::capability::derive_capability(
            TEST_HOST,
            &TEST_SECRET,
        ))
        .expect("profile");
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let session = manager
        .create(&bootstrap, CLIENT_IP, &hello())
        .expect("create")
        .session;

    let uplink = batch(&[
        (FrameType::OPEN, 1, Vec::new()),
        (FrameType::OPEN, 2, Vec::new()),
    ]);
    assert_eq!(session.process_up(1, &uplink), Ok(1));
    assert!(!session.is_closed_for_test());

    let mut cursor = 0u64;
    let mut rejected = false;
    for _ in 0..40 {
        let (body, next) = session.poll(cursor).await.expect("poll");
        cursor = next;
        if has_close(&body, 2) {
            rejected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(rejected, "the over-limit stream must be refused with CLOSE");
}

/// Waits until a downlink poll has parked on the queue the caller names.
///
/// Spawning the second poll before the first one has registered would leave the
/// test asserting nothing at all: two polls that never overlap in time cannot
/// exercise supersede.
async fn wait_for_parked_poll(session: &Arc<Session>, lane: Option<u32>) -> bool {
    for _ in 0..400 {
        {
            let state = session.state.lock();
            let parked = match lane {
                Some(id) => state.lanes.get(&id).is_some_and(|queue| queue.down_active),
                None => state.main.down_active,
            };
            if parked {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_poll_supersedes_the_parked_one_on_the_shared_carrier() {
    // Two polls overlap on every flaky reconnect: the client parks one, the
    // network stalls, and the replacement arrives before the first request's
    // socket has died. Refusing the newer one leaves the client retrying against
    // a poll that will not answer for another long-poll period, which is
    // indistinguishable from a dead relay. The older poll therefore has to come
    // back empty *at its own cursor* — any other cursor and the client's next
    // request is a protocol violation.
    let backend = start_echo_backend().await;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::Https,
        WebLimits::default(),
    );
    let session = open_session(&manager);

    let parked = tokio::spawn({
        let session = session.clone();
        async move { session.poll(0).await }
    });
    assert!(
        wait_for_parked_poll(&session, None).await,
        "the first poll never parked, so nothing was superseded"
    );
    let newest = tokio::spawn({
        let session = session.clone();
        async move { session.poll(0).await }
    });

    let superseded = tokio::time::timeout(Duration::from_secs(5), parked)
        .await
        .expect("the superseded poll was not released by the newer one")
        .expect("poll task");
    assert_eq!(
        superseded,
        Ok((Bytes::new(), 0)),
        "a superseded poll must answer empty at its own cursor, never be refused"
    );

    // The newest poll owns the queue, so the next queued frame belongs to it.
    let window = frame::encode(FrameType::WINDOW, 1, &frame::window_payload(2));
    {
        let mut state = session.state.lock();
        assert!(
            session.queue_frame_locked(&mut state, FrameType::WINDOW, 1, &frame::window_payload(2)),
            "could not queue a downlink frame"
        );
    }
    let winner = tokio::time::timeout(Duration::from_secs(5), newest)
        .await
        .expect("the newest poll never observed the queued frame")
        .expect("poll task");
    assert_eq!(winner, Ok((Bytes::from(window), 1)));
    assert!(
        !session.is_closed_for_test(),
        "superseding a poll must not close the session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_poll_supersedes_the_parked_one_on_a_carrier_lane() {
    // The lanes carriers reach the same parking code through a per-stream queue,
    // and a lane that answers a supersede with anything but its own cursor
    // strands exactly one stream while the rest of the session looks healthy —
    // the hardest failure of the four carriers to attribute from a client.
    let backend = start_echo_backend().await;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::HttpsLanes,
        WebLimits::default(),
    );
    let session = open_session(&manager);
    assert_eq!(
        session.process_up_lane(4, 1, &batch(&[(FrameType::OPEN, 4, Vec::new())])),
        Ok(1)
    );

    let parked = tokio::spawn({
        let session = session.clone();
        async move { session.poll_lane(4, 0).await }
    });
    assert!(
        wait_for_parked_poll(&session, Some(4)).await,
        "the first lane poll never parked, so nothing was superseded"
    );
    let newest = tokio::spawn({
        let session = session.clone();
        async move { session.poll_lane(4, 0).await }
    });

    let superseded = tokio::time::timeout(Duration::from_secs(5), parked)
        .await
        .expect("the superseded lane poll was not released by the newer one")
        .expect("poll task");
    assert_eq!(
        superseded,
        Ok((Bytes::new(), 0, false)),
        "a superseded lane poll must answer empty at its own cursor, and the lane is not closed"
    );

    let window = frame::encode(FrameType::WINDOW, 4, &frame::window_payload(2));
    {
        let mut state = session.state.lock();
        assert!(
            session.queue_frame_locked(&mut state, FrameType::WINDOW, 4, &frame::window_payload(2)),
            "could not queue a frame on the lane"
        );
    }
    let winner = tokio::time::timeout(Duration::from_secs(5), newest)
        .await
        .expect("the newest lane poll never observed the queued frame")
        .expect("poll task");
    assert_eq!(winner, Ok((Bytes::from(window), 1, false)));
    assert!(
        !session.is_closed_for_test(),
        "superseding a lane poll must not close the session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_frame_for_an_evicted_stream_id_is_acknowledged_and_ignored() {
    // Both reference relays pin this rule independently, which is the strongest
    // evidence available that it is a real trap rather than defensive coding. A
    // client that closes a stream and then retransmits its last unacknowledged
    // batch is ordinary; once the tombstone ring has rotated past that id there
    // is nothing left to recognise the frame by. Failing the session there costs
    // the client every other stream it was running, over a race it cannot avoid.
    let backend = start_echo_backend().await;
    let mut limits = WebLimits::default();
    limits.max_closed_stream_ids = 2;
    let manager = build_manager(
        WebBackend::Loopback(backend),
        CarrierMode::HttpsLanes,
        limits,
    );
    let session = open_session(&manager);

    // Five open/close cycles rotate the two-entry tombstone ring three times, so
    // ids 1..=3 lose both their tombstone and their carrier lane.
    for lane in 1..=5u32 {
        assert_eq!(
            session.process_up_lane(lane, 1, &batch(&[(FrameType::OPEN, lane, Vec::new())])),
            Ok(1),
            "lane {lane} was refused"
        );
        assert_eq!(
            session.process_up_lane(lane, 2, &batch(&[(FrameType::CLOSE, lane, Vec::new())])),
            Ok(2),
            "lane {lane} could not be closed"
        );
    }
    {
        let state = session.state.lock();
        assert!(
            !state.closed_streams.contains(&1),
            "id 1 must have aged out of the tombstone ring for this test to mean anything"
        );
        assert!(
            !state.lanes.contains_key(&1),
            "the evicted tombstone must have taken its carrier lane with it"
        );
    }

    for (name, late) in [
        ("DATA", batch(&[(FrameType::DATA, 1, b"late".to_vec())])),
        (
            "WINDOW",
            batch(&[(FrameType::WINDOW, 1, frame::window_payload(64).to_vec())]),
        ),
        ("CLOSE", batch(&[(FrameType::CLOSE, 1, Vec::new())])),
    ] {
        assert_eq!(
            session.process_up_lane(1, 7, &late),
            Ok(7),
            "a late {name} for an evicted stream id was not acknowledged"
        );
        assert!(
            !session.is_closed_for_test(),
            "a late {name} for an evicted stream id closed the session"
        );
        assert!(
            !session.state.lock().lanes.contains_key(&1),
            "a late {name} minted a carrier lane for a dead stream id"
        );
    }
}

/// Starts a loopback backend that reports how many bytes it has received.
///
/// The echo backend cannot answer "have my uplink bytes reached you yet", and
/// the refused-WINDOW test has to know the backend read already happened before
/// it looks at the stream table, or it would be asserting on a race.
async fn start_counting_backend() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind counter");
    let address = listener.local_addr().expect("counter addr");
    let received = Arc::new(AtomicUsize::new(0));
    let counter = received.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let counter = counter.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 16 * 1024];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => {
                            counter.fetch_add(read, Ordering::Relaxed);
                        }
                    }
                }
            });
        }
    });
    (address, received)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_window_closes_one_stream_and_leaves_the_session_running() {
    // `PROTOCOL.md:436-437` is normative: a stream the relay cannot serve gets a
    // `CLOSE` for its own id, and the authenticated session and its other streams
    // keep running. A WINDOW that will not fit the queue is that same condition
    // arriving from the backend side, and ending the session for it turns one
    // saturated stream into a full re-bootstrap for every stream the client had.
    let (backend, received) = start_counting_backend().await;
    let mut limits = WebLimits::default();
    limits.max_streams_per_session = 2;
    limits.max_pending_per_session = 4 * 1024 * 1024;
    // The *item* ceiling is what refuses the WINDOW below. One legal batch
    // charges an item for its uplink DATA and an item for every CLOSE its
    // over-limit OPENs provoke, and the queue accepts refusals until exactly one
    // item short of the ceiling — so `1 + (ceiling - 1)` fills it precisely.
    limits.max_pending_items_per_session = 32;
    let mut timeouts = WebTimeouts::default();
    // Bounds the downlink drain below; the production 25 s park would make a
    // missing CLOSE fail as a hang rather than as an assertion.
    timeouts.long_poll_ms = 500;
    let manager = build_manager_with_timeouts(
        WebBackend::Loopback(backend),
        CarrierMode::Https,
        limits.clone(),
        timeouts,
    );
    let session = open_session(&manager);

    let refused_opens = limits.max_pending_items_per_session - 1;
    let mut frames: Vec<(FrameType, u32, Vec<u8>)> = vec![
        (FrameType::OPEN, 1, Vec::new()),
        // Larger than the 64 KiB backend copy buffer, so the first backend read
        // drains part of one queued write: it hands bytes back to the budget but
        // no whole queue item, which is what leaves the item ceiling full.
        (FrameType::DATA, 1, vec![0x7eu8; 96 * 1024]),
        (FrameType::OPEN, 2, Vec::new()),
    ];
    for offset in 0..refused_opens as u32 {
        frames.push((FrameType::OPEN, 3 + offset, Vec::new()));
    }
    assert_eq!(session.process_up(1, &batch(&frames)), Ok(1));
    assert_eq!(
        session.state.lock().pending_items,
        limits.max_pending_items_per_session,
        "the batch must leave the item ceiling exactly full for the WINDOW to be refused"
    );

    // The WINDOW attempt happens inside the read that hands these bytes on, so
    // seeing them at the backend means the refusal has already been decided.
    for _ in 0..400 {
        if received.load(Ordering::Relaxed) >= 64 * 1024 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        received.load(Ordering::Relaxed) >= 64 * 1024,
        "the backend never read the uplink payload, so no WINDOW was ever attempted"
    );

    {
        let state = session.state.lock();
        assert!(
            !state.streams.contains_key(&1),
            "the refused WINDOW must close its own stream"
        );
        assert!(
            state.streams.contains_key(&2),
            "the refused WINDOW must leave a sibling stream running"
        );
    }
    assert!(
        !session.is_closed_for_test(),
        "the refused WINDOW must not close the authenticated session"
    );

    // The client learns about the one stream through the frame the protocol
    // reserves for it, and never receives the WINDOW that was refused.
    let mut cursor = 0u64;
    let mut closed = false;
    let mut windows = 0usize;
    for _ in 0..8 {
        let (body, next) = session.poll(cursor).await.expect("poll");
        cursor = next;
        for value in frame::parse_all(&body, frame::MAX_PAYLOAD).expect("parse downlink") {
            if value.stream_id != 1 {
                continue;
            }
            match value.kind {
                FrameType::CLOSE => closed = true,
                FrameType::WINDOW => windows += 1,
                _ => {}
            }
        }
        if closed {
            break;
        }
    }
    assert!(
        closed,
        "the refused WINDOW must be reported to the client as a CLOSE for that stream"
    );
    assert_eq!(windows, 0, "a refused WINDOW must not also be delivered");
}
