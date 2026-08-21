//! End-to-end session tests over a loopback backend.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use crate::config::{CarrierMode, WebBackend, WebLimits};
use crate::web::error::WebError;
use crate::web::frame::{self, FrameType};

use super::harness::{
    TEST_HOST, TEST_SECRET, batch, build_manager, data_payloads, has_close, start_echo_backend,
};

const CLIENT_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));

fn hello() -> Vec<u8> {
    frame::encode(FrameType::HELLO, 0, &[1])
}

/// Polls until the expected payload arrives or the attempt budget runs out.
async fn poll_until(
    session: &std::sync::Arc<crate::web::session::Session>,
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
        .match_capability(&crate::web::capability::derive_capability(
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
        .match_capability(&crate::web::capability::derive_capability(
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
        .match_capability(&crate::web::capability::derive_capability(
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
        .match_capability(&crate::web::capability::derive_capability(
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
        .match_capability(&crate::web::capability::derive_capability(
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
        .match_capability(&crate::web::capability::derive_capability(
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

    // A cross-lane frame fails only that lane, never the session.
    let cross = batch(&[(FrameType::DATA, 2, b"bad".to_vec())]);
    assert_eq!(
        session.process_up_lane(1, 2, &cross),
        Err(WebError::Protocol)
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
        .match_capability(&crate::web::capability::derive_capability(
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
        .match_capability(&crate::web::capability::derive_capability(
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
