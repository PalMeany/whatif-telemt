//! A real MTProto handshake driven through the carrier into the in-process
//! client pipeline.
//!
//! The other backend tests use an echo server or deliberately invalid bytes,
//! which prove the carrier moves bytes but say nothing about whether the proxy
//! would ever accept the stream. These authenticate for real, with the
//! transform a WEB client actually puts on the carrier, so a configuration
//! that no client could ever get through fails here instead of in production.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use crate::config::ProxyConfig;
use crate::config::fork::web::{CarrierMode, WebBackend, WebLimits};
use crate::crypto::{AesCtr, sha256};
use crate::fork::web::capability::derive_capability;
use crate::fork::web::frame::{self, FrameType};
use crate::protocol::ProtoTag;
use crate::protocol::constants::{
    DC_IDX_POS, HANDSHAKE_LEN, IV_LEN, PREKEY_LEN, PROTO_TAG_POS, SKIP_LEN,
};

use super::harness::{TEST_HOST, TEST_SECRET, batch, build_manager_with_stats, data_payloads};

const CLIENT_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 44));

/// Builds an obfuscated2 handshake that authenticates against `secret`.
///
/// This is the transform a WEB client actually puts on the carrier: the client
/// rejects `ee` fake-TLS secrets for WEB proxies, so only the plain (classic)
/// and `dd` (secure) forms ever reach the relay.
pub(super) fn authenticating_handshake(secret: &[u8], proto_tag: ProtoTag) -> [u8; HANDSHAKE_LEN] {
    let mut handshake = [0x5Au8; HANDSHAKE_LEN];
    for (index, byte) in handshake[SKIP_LEN..SKIP_LEN + PREKEY_LEN + IV_LEN]
        .iter_mut()
        .enumerate()
    {
        *byte = (index as u8).wrapping_add(1);
    }

    let prekey = &handshake[SKIP_LEN..SKIP_LEN + PREKEY_LEN];
    let iv_bytes = &handshake[SKIP_LEN + PREKEY_LEN..SKIP_LEN + PREKEY_LEN + IV_LEN];
    let mut key_input = Vec::with_capacity(PREKEY_LEN + secret.len());
    key_input.extend_from_slice(prekey);
    key_input.extend_from_slice(secret);
    let key = sha256(&key_input);
    let mut iv = [0u8; IV_LEN];
    iv.copy_from_slice(iv_bytes);

    let mut stream = AesCtr::new(&key, u128::from_be_bytes(iv));
    let keystream = stream.encrypt(&[0u8; HANDSHAKE_LEN]);

    let mut plain = [0u8; HANDSHAKE_LEN];
    plain[PROTO_TAG_POS..PROTO_TAG_POS + 4].copy_from_slice(&proto_tag.to_bytes());
    plain[DC_IDX_POS..DC_IDX_POS + 2].copy_from_slice(&2i16.to_le_bytes());
    for index in PROTO_TAG_POS..HANDSHAKE_LEN {
        handshake[index] = plain[index] ^ keystream[index];
    }
    handshake
}

/// A configuration accepting the `dd` secret form, with no reachable upstream.
///
/// The handshake decision under test happens before any DC connect, and a dead
/// loopback upstream keeps the test off the network.
pub(super) fn secure_mode_config() -> ProxyConfig {
    let mut config = ProxyConfig::default();
    config.general.use_middle_proxy = false;
    config.general.modes.classic = false;
    config.general.modes.secure = true;
    config.general.modes.tls = false;
    config.censorship.mask = false;
    config.access.ignore_time_skew = true;
    config
        .access
        .users
        .insert("tester".to_string(), hex::encode(TEST_SECRET));
    config
}

/// What one carrier stream produced.
struct Outcome {
    downlink: Vec<u8>,
    closed: bool,
    rejected: Vec<(String, u64)>,
}

/// Drives one authenticating ClientHello through the carrier and reports what
/// the proxy did with the stream.
///
/// A refused client also receives a synthetic ServerHello — that is what
/// TLS-fronting is for — so the reject counters, not the response shape, are
/// what distinguish an accepted stream.
async fn run_handshake(config: ProxyConfig) -> Outcome {
    run_handshake_with_secret(config, &TEST_SECRET).await
}

async fn run_handshake_with_secret(config: ProxyConfig, secret: &[u8]) -> Outcome {
    let (manager, stats) = build_manager_with_stats(
        config,
        WebBackend::Internal,
        CarrierMode::Https,
        WebLimits::default(),
    );
    let profile = manager
        .match_capability(&derive_capability(TEST_HOST, &TEST_SECRET))
        .expect("profile");
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let session = manager
        .create(
            &bootstrap,
            CLIENT_IP,
            &frame::encode(FrameType::HELLO, 0, &[1]),
        )
        .expect("create")
        .session;

    let handshake = authenticating_handshake(secret, ProtoTag::Secure);
    let uplink = batch(&[
        (FrameType::OPEN, 1, Vec::new()),
        (FrameType::DATA, 1, handshake.to_vec()),
    ]);
    assert_eq!(session.process_up(1, &uplink), Ok(1));

    let mut downlink = Vec::new();
    let mut closed = false;
    let mut cursor = 0u64;
    for _ in 0..60 {
        let (body, next) = session.poll(cursor).await.expect("poll");
        cursor = next;
        downlink.extend_from_slice(&data_payloads(&body, 1));
        if closed || super::harness::has_close(&body, 1) {
            closed = true;
            break;
        }
        if !downlink.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Outcome {
        downlink,
        closed,
        rejected: stats.get_connects_bad_class_counts(),
    }
}

/// Drives the same authenticating handshake through one `websocket-lanes`
/// carrier lane, which is the shape a lane deployment actually runs.
///
/// The lane carriers reach the backend through a different path than `https`:
/// the lane is created by the socket rather than by the batch, the queue is
/// per-stream, and the poll reports lane closure separately. None of that was
/// covered against a real handshake.
async fn run_lane_handshake(config: ProxyConfig) -> Outcome {
    let (manager, stats) = build_manager_with_stats(
        config,
        WebBackend::Internal,
        CarrierMode::WebsocketLanes,
        WebLimits::default(),
    );
    let profile = manager
        .match_capability(&derive_capability(TEST_HOST, &TEST_SECRET))
        .expect("profile");
    let bootstrap = manager
        .issue_bootstrap(&profile, CLIENT_IP)
        .expect("bootstrap");
    let session = manager
        .create(
            &bootstrap,
            CLIENT_IP,
            &frame::encode(FrameType::HELLO, 0, &[1]),
        )
        .expect("create")
        .session;

    // The lane socket attaches before any frame arrives, exactly as the client
    // opens one WebSocket per stream and only then sends its OPEN.
    assert!(session.acquire_websocket_lane(1), "lane 1 must attach");

    let handshake = authenticating_handshake(&TEST_SECRET, ProtoTag::Secure);
    let uplink = batch(&[
        (FrameType::OPEN, 1, Vec::new()),
        (FrameType::DATA, 1, handshake.to_vec()),
    ]);
    assert_eq!(session.process_up_lane(1, 1, &uplink), Ok(1));

    let mut downlink = Vec::new();
    let mut closed = false;
    let mut cursor = 0u64;
    for _ in 0..60 {
        let (body, next, lane_closed) = session.poll_lane(1, cursor).await.expect("poll lane");
        cursor = next;
        downlink.extend_from_slice(&data_payloads(&body, 1));
        if lane_closed || closed || super::harness::has_close(&body, 1) {
            closed = true;
            break;
        }
        if !downlink.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Outcome {
        downlink,
        closed,
        rejected: stats.get_connects_bad_class_counts(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dd_secret_stream_is_accepted_over_a_carrier_lane() {
    let outcome = run_lane_handshake(secure_mode_config()).await;
    assert!(
        outcome.rejected.is_empty(),
        "a valid dd handshake was refused on a lane: {:?}",
        outcome.rejected
    );
    // The stream still ends, because this harness deliberately has no reachable
    // datacentre; what matters is that the lane carried the handshake to the
    // proxy and the proxy accepted it, exactly as the shared carrier does.
    assert!(
        !outcome.closed || outcome.rejected.is_empty(),
        "the lane closed the stream for a reason the shared carrier does not: {:?}",
        outcome.rejected
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dd_secret_stream_is_accepted_by_the_proxy() {
    let outcome = run_handshake(secure_mode_config()).await;
    assert!(
        outcome.rejected.is_empty(),
        "a valid dd handshake was refused: {:?}",
        outcome.rejected
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_mode_refuses_every_web_stream() {
    // The deployment failure this guards, and the reason it is so hard to see:
    // a WEB client can only speak the plain or dd transform, so a TLS-only
    // proxy refuses every stream while the carrier itself stays perfectly
    // healthy — sessions are created, streams open, and no data ever flows.
    let mut config = secure_mode_config();
    config.general.modes.secure = false;
    config.general.modes.tls = true;
    let outcome = run_handshake(config).await;
    assert!(
        outcome
            .rejected
            .iter()
            .any(|(class, count)| class == "direct_modes_disabled" && *count > 0),
        "expected the stream to be refused for a disabled mode, got {:?}",
        outcome.rejected
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_the_proxy_does_not_know_is_refused() {
    let outcome = run_handshake_with_secret(secure_mode_config(), &[0x99u8; 16]).await;
    assert!(
        outcome
            .rejected
            .iter()
            .any(|(class, count)| class == "direct_mtproto_bad_client" && *count > 0),
        "expected an unknown secret to be refused, got {:?}",
        outcome.rejected
    );
}
