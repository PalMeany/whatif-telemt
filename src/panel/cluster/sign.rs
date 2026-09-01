//! Request signing between a master panel and a linked node.
//!
//! Every cluster request carries an HMAC-SHA256 signature over a canonical
//! description of the request. The signature binds the method, the target, the
//! node identity, a timestamp, a nonce, and the exact body bytes, so a captured
//! request cannot be replayed against a different route, a different node, or
//! after its window closes.

use std::collections::VecDeque;

use parking_lot::Mutex;
use std::collections::HashSet;

use crate::crypto::sha256;
use crate::panel::crypto::{decode, encode, hmac_sha256, secure_eq};

/// Header carrying the identifier of the node the request is addressed to.
///
/// The target's identifier is signed, not the caller's: an agent holds exactly
/// one link key, so binding the signature to the agent's own identity is what
/// stops a request captured from one agent being replayed against another that
/// an operator linked with the same key by mistake.
pub(crate) const HEADER_NODE: &str = "x-telemt-node";

/// Header carrying the request timestamp in unix milliseconds.
pub(crate) const HEADER_TIMESTAMP: &str = "x-telemt-timestamp";

/// Header carrying the per-request nonce.
pub(crate) const HEADER_NONCE: &str = "x-telemt-nonce";

/// Header carrying the request signature.
pub(crate) const HEADER_SIGNATURE: &str = "x-telemt-signature";

/// Domain separator prefixed to every canonical string.
///
/// It keeps a signature minted for this protocol from being meaningful under
/// any future one that reuses the same link key.
const DOMAIN: &str = "TELEMT-CLUSTER-V1";

/// The material a signature covers.
pub(crate) struct SignedRequest<'a> {
    /// HTTP method, uppercase.
    pub(crate) method: &'a str,
    /// Request target including its query string.
    pub(crate) path: &'a str,
    /// Identifier of the node the request is addressed to.
    pub(crate) node_id: &'a str,
    /// Unix milliseconds the request was minted at.
    pub(crate) timestamp_ms: u64,
    /// Per-request nonce, canonical unpadded base64url.
    pub(crate) nonce: &'a str,
    /// Exact request body bytes.
    pub(crate) body: &'a [u8],
}

impl SignedRequest<'_> {
    /// Renders the canonical string the signature is computed over.
    ///
    /// Newline separation is safe here, unlike in the audit chain: hyper rejects
    /// a request line or a header value containing one, the nonce is validated
    /// as canonical base64url, and the node identifier is compared against this
    /// node's own before verification runs. No component can carry a separator.
    fn canonical(&self) -> String {
        format!(
            "{DOMAIN}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.method,
            self.path,
            self.node_id,
            self.timestamp_ms,
            self.nonce,
            hex::encode(sha256(self.body)),
        )
    }

    /// Computes the signature under one link key.
    pub(crate) fn sign(&self, link_key: &[u8]) -> String {
        encode(&hmac_sha256(link_key, self.canonical().as_bytes()))
    }

    /// Verifies a submitted signature in constant time.
    pub(crate) fn verify(&self, link_key: &[u8], signature: &str) -> bool {
        let Some(submitted) = decode(signature) else {
            return false;
        };
        secure_eq(
            &self.sign(link_key).into_bytes(),
            &encode(&submitted).into_bytes(),
        )
    }
}

/// Why a signed request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignatureError {
    /// A required header was absent or malformed.
    MalformedHeaders,
    /// The timestamp lies outside the accepted clock-skew window.
    StaleTimestamp,
    /// The nonce was already spent inside the window.
    ReplayedNonce,
    /// The signature did not match.
    BadSignature,
}

impl SignatureError {
    /// Machine-readable code returned to the caller.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SignatureError::MalformedHeaders => "malformed_signature_headers",
            SignatureError::StaleTimestamp => "stale_timestamp",
            SignatureError::ReplayedNonce => "replayed_nonce",
            SignatureError::BadSignature => "bad_signature",
        }
    }
}

/// Bounded replay window shared by every inbound cluster request.
///
/// One nonce is accepted once. The window is a plain FIFO of the capacity the
/// operator configured: an attacker who floods it evicts their own captured
/// nonce as fast as anyone else's, and the timestamp check still bounds how
/// long any capture stays usable.
pub(crate) struct NonceWindow {
    capacity: usize,
    inner: Mutex<NonceWindowInner>,
}

struct NonceWindowInner {
    order: VecDeque<String>,
    seen: HashSet<String>,
}

impl NonceWindow {
    /// Builds an empty window.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: Mutex::new(NonceWindowInner {
                order: VecDeque::with_capacity(capacity.min(1_024)),
                seen: HashSet::new(),
            }),
        }
    }

    /// Claims one nonce, reporting whether it was still unspent.
    pub(crate) fn claim(&self, node_id: &str, nonce: &str) -> bool {
        let key = format!("{node_id}\u{0}{nonce}");
        let mut inner = self.inner.lock();
        if !inner.seen.insert(key.clone()) {
            return false;
        }
        inner.order.push_back(key);
        while inner.order.len() > self.capacity {
            if let Some(evicted) = inner.order.pop_front() {
                inner.seen.remove(&evicted);
            }
        }
        true
    }
}

/// Verifies headers, window, and signature for one inbound request.
pub(crate) fn verify_inbound(
    request: &SignedRequest<'_>,
    link_key: &[u8],
    signature: &str,
    now_ms: u64,
    clock_skew_secs: u64,
    window: &NonceWindow,
) -> Result<(), SignatureError> {
    if request.nonce.len() < 16 || decode(request.nonce).is_none() {
        return Err(SignatureError::MalformedHeaders);
    }
    let skew_ms = clock_skew_secs.saturating_mul(1_000);
    let difference = now_ms.abs_diff(request.timestamp_ms);
    if difference > skew_ms {
        return Err(SignatureError::StaleTimestamp);
    }
    // The signature is checked before the nonce is spent: an unauthenticated
    // caller must not be able to burn nonces a legitimate master will reuse.
    if !request.verify(link_key, signature) {
        return Err(SignatureError::BadSignature);
    }
    if !window.claim(request.node_id, request.nonce) {
        return Err(SignatureError::ReplayedNonce);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(
        method: &'a str,
        path: &'a str,
        nonce: &'a str,
        ts: u64,
        body: &'a [u8],
    ) -> SignedRequest<'a> {
        SignedRequest {
            method,
            path,
            node_id: "node-a",
            timestamp_ms: ts,
            nonce,
            body,
        }
    }

    #[test]
    fn a_signature_covers_every_bound_field() {
        let key = b"link-key";
        let nonce = encode(&[7u8; 16]);
        let base = request("POST", "/cluster/v1/api", &nonce, 1_000, b"{}");
        let signature = base.sign(key);
        assert!(base.verify(key, &signature));

        // Each field is load-bearing.
        assert!(!request("GET", "/cluster/v1/api", &nonce, 1_000, b"{}").verify(key, &signature));
        assert!(
            !request("POST", "/cluster/v1/other", &nonce, 1_000, b"{}").verify(key, &signature)
        );
        assert!(!request("POST", "/cluster/v1/api", &nonce, 1_001, b"{}").verify(key, &signature));
        assert!(!request("POST", "/cluster/v1/api", &nonce, 1_000, b"{ }").verify(key, &signature));
        assert!(!base.verify(b"other-key", &signature));
    }

    #[test]
    fn the_window_spends_each_nonce_once() {
        let window = NonceWindow::new(4);
        assert!(window.claim("a", "n1"));
        assert!(!window.claim("a", "n1"));
        // The same nonce from a different node is a different claim.
        assert!(window.claim("b", "n1"));
    }

    #[test]
    fn the_window_evicts_in_arrival_order() {
        let window = NonceWindow::new(2);
        assert!(window.claim("a", "n1"));
        assert!(window.claim("a", "n2"));
        assert!(window.claim("a", "n3"));
        // n1 fell out of the window, so it is claimable again; the timestamp
        // check is what bounds how long that matters.
        assert!(window.claim("a", "n1"));
        assert!(!window.claim("a", "n3"));
    }

    #[test]
    fn inbound_verification_enforces_skew_signature_and_replay() {
        let key = b"link-key";
        let window = NonceWindow::new(16);
        let nonce = encode(&[9u8; 16]);
        let signed = request("POST", "/cluster/v1/api", &nonce, 100_000, b"payload");
        let signature = signed.sign(key);

        assert_eq!(
            verify_inbound(&signed, key, &signature, 100_000, 60, &window),
            Ok(())
        );
        assert_eq!(
            verify_inbound(&signed, key, &signature, 100_000, 60, &window),
            Err(SignatureError::ReplayedNonce)
        );

        let fresh_nonce = encode(&[10u8; 16]);
        let fresh = request("POST", "/cluster/v1/api", &fresh_nonce, 100_000, b"payload");
        let fresh_signature = fresh.sign(key);
        assert_eq!(
            verify_inbound(&fresh, key, &fresh_signature, 400_000, 60, &window),
            Err(SignatureError::StaleTimestamp)
        );
        assert_eq!(
            verify_inbound(&fresh, key, "AAAA", 100_000, 60, &window),
            Err(SignatureError::BadSignature)
        );

        let short = request("POST", "/cluster/v1/api", "abc", 100_000, b"payload");
        assert_eq!(
            verify_inbound(&short, key, &signature, 100_000, 60, &window),
            Err(SignatureError::MalformedHeaders)
        );
    }

    #[test]
    fn a_rejected_signature_does_not_spend_the_nonce() {
        let key = b"link-key";
        let window = NonceWindow::new(16);
        let nonce = encode(&[11u8; 16]);
        let signed = request("POST", "/cluster/v1/api", &nonce, 100_000, b"payload");
        assert_eq!(
            verify_inbound(&signed, key, "AAAA", 100_000, 60, &window),
            Err(SignatureError::BadSignature)
        );
        let signature = signed.sign(key);
        assert_eq!(
            verify_inbound(&signed, key, &signature, 100_000, 60, &window),
            Ok(())
        );
    }
}
