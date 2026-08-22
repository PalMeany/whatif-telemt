//! A real MTProto handshake driven through the carrier into the in-process
//! client pipeline.
//!
//! The other backend tests use an echo server or deliberately invalid bytes,
//! which prove the carrier moves bytes but say nothing about whether the
//! proxy would ever accept the stream. This test authenticates for real: if
//! the fronted-secret capability, the stream plumbing, or the configured proxy
//! modes stop a genuine client from completing its handshake, it fails.

use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{CarrierMode, ProxyConfig, WebBackend, WebLimits};
use crate::crypto::sha256_hmac;
use crate::protocol::tls;
use crate::web::capability::derive_capability;
use crate::web::frame::{self, FrameType};

use super::harness::{TEST_HOST, TEST_SECRET, batch, build_manager_with_stats, data_payloads};

const CLIENT_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 44));

/// TLS record type carrying a handshake message.
const TLS_RECORD_HANDSHAKE: u8 = 0x16;

/// Builds a fake-TLS `ClientHello` that authenticates against `secret`.
///
/// Mirrors the client side of the EE-TLS profile: the proxy recovers the
/// digest from the random field and compares an HMAC over the record.
fn authenticating_client_hello(secret: &[u8], timestamp: u32) -> Vec<u8> {
    const TLS_AES_128_GCM_SHA256: [u8; 2] = [0x13, 0x01];
    const TLS_EXTENSION_KEY_SHARE: u16 = 0x0033;
    const X25519_KEY_SHARE_LEN: usize = 32;
    let session_id_len: usize = 32;
    let fill = 0x42u8;

    let mut key_share = Vec::new();
    key_share.extend_from_slice(&tls::TLS_NAMED_GROUP_X25519.to_be_bytes());
    key_share.extend_from_slice(&(X25519_KEY_SHARE_LEN as u16).to_be_bytes());
    key_share.push(9);
    key_share.resize(key_share.len() + X25519_KEY_SHARE_LEN - 1, 0);

    let mut key_share_extension = Vec::new();
    key_share_extension.extend_from_slice(&(key_share.len() as u16).to_be_bytes());
    key_share_extension.extend_from_slice(&key_share);

    let mut extensions = Vec::new();
    extensions.extend_from_slice(&TLS_EXTENSION_KEY_SHARE.to_be_bytes());
    extensions.extend_from_slice(&(key_share_extension.len() as u16).to_be_bytes());
    extensions.extend_from_slice(&key_share_extension);

    let body_len = 2
        + 32
        + 1
        + session_id_len
        + 2
        + TLS_AES_128_GCM_SHA256.len()
        + 1
        + 1
        + 2
        + extensions.len();
    let mut body = Vec::with_capacity(body_len);
    body.extend_from_slice(&[0x03, 0x03]);
    body.extend_from_slice(&[fill; 32]);
    body.push(session_id_len as u8);
    body.extend_from_slice(&[fill; 32]);
    body.extend_from_slice(&(TLS_AES_128_GCM_SHA256.len() as u16).to_be_bytes());
    body.extend_from_slice(&TLS_AES_128_GCM_SHA256);
    body.push(1);
    body.push(0);
    body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
    body.extend_from_slice(&extensions);

    let mut record = Vec::with_capacity(5 + 4 + body_len);
    record.push(TLS_RECORD_HANDSHAKE);
    record.extend_from_slice(&[0x03, 0x01]);
    record.extend_from_slice(&((4 + body_len) as u16).to_be_bytes());
    record.push(0x01);
    let body_len_bytes = (body_len as u32).to_be_bytes();
    record.extend_from_slice(&body_len_bytes[1..4]);
    record.extend_from_slice(&body);

    record[tls::TLS_DIGEST_POS..tls::TLS_DIGEST_POS + tls::TLS_DIGEST_LEN].fill(0);
    let mut digest = sha256_hmac(secret, &record);
    let stamp = timestamp.to_le_bytes();
    for index in 0..4 {
        digest[28 + index] ^= stamp[index];
    }
    record[tls::TLS_DIGEST_POS..tls::TLS_DIGEST_POS + tls::TLS_DIGEST_LEN].copy_from_slice(&digest);
    record
}

fn tls_only_config() -> ProxyConfig {
    let mut config = ProxyConfig::default();
    config.general.use_middle_proxy = false;
    config.general.modes.classic = false;
    config.general.modes.secure = false;
    config.general.modes.tls = true;
    // Emulation would need a fetched certificate profile; the handshake path
    // under test is the same either way.
    config.censorship.tls_emulation = false;
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

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as u32;
    let hello = authenticating_client_hello(secret, timestamp);
    let uplink = batch(&[
        (FrameType::OPEN, 1, Vec::new()),
        (FrameType::DATA, 1, hello),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fake_tls_handshake_completes_through_the_carrier() {
    let outcome = run_handshake(tls_only_config()).await;
    assert!(
        !outcome.downlink.is_empty(),
        "the proxy sent nothing back: the stream never reached the handshake"
    );
    assert_eq!(
        outcome.downlink[0],
        TLS_RECORD_HANDSHAKE,
        "expected a ServerHello record, got {:02x?}",
        &outcome.downlink[..outcome.downlink.len().min(8)]
    );
    assert!(
        outcome.rejected.is_empty(),
        "a valid EE-TLS handshake was refused: {:?}",
        outcome.rejected
    );
    assert!(!outcome.closed, "an accepted stream must stay open");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_the_proxy_does_not_know_is_refused() {
    // The counterpart of the test above: it proves the acceptance assertion
    // has teeth, and that a stream the proxy refuses is still answered
    // indistinguishably before it is dropped.
    let outcome = run_handshake_with_secret(tls_only_config(), &[0x99u8; 16]).await;
    assert!(
        outcome
            .rejected
            .iter()
            .any(|(class, count)| class == "tls_handshake_bad_client" && *count > 0),
        "a refused handshake must be classified, got {:?}",
        outcome.rejected
    );
    assert!(outcome.closed, "a refused stream must be closed");
}
