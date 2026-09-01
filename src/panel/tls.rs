//! TLS termination for the panel listener.
//!
//! PEM decoding is done here rather than pulled in as a dependency: the format
//! the panel has to read is two labelled base64 blocks, and adding a crate for
//! that would widen the supply chain of a control plane for no capability.
//!
//! The leaf certificate's SHA-256 is published through
//! [`current_fingerprint`], because that is the value a master pins when an
//! operator links this node.

use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use parking_lot::Mutex;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer,
};
use tracing::info;

use crate::config::PanelTlsConfig;
use crate::crypto::sha256;
use crate::error::{ProxyError, Result};

/// SHA-256 of the leaf certificate the panel currently serves.
static CERT_FINGERPRINT: Mutex<Option<String>> = Mutex::new(None);

/// Returns the fingerprint of the certificate the panel serves, when it has one.
pub(crate) fn current_fingerprint() -> Option<String> {
    CERT_FINGERPRINT.lock().clone()
}

/// Builds the server TLS configuration from the configured PEM files.
pub(crate) async fn server_config(config: &PanelTlsConfig) -> Result<Arc<rustls::ServerConfig>> {
    let certificates = load_certificates(Path::new(&config.cert_path)).await?;
    let key = load_private_key(Path::new(&config.key_path)).await?;
    let leaf = certificates.first().ok_or_else(|| {
        ProxyError::Config("panel.tls.cert_path holds no certificate".to_string())
    })?;
    let fingerprint = hex::encode(sha256(leaf.as_ref()));

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|error| ProxyError::Config(format!("panel TLS versions: {error}")))?
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|error| ProxyError::Config(format!("panel TLS certificate: {error}")))?;

    *CERT_FINGERPRINT.lock() = Some(fingerprint.clone());
    info!(fingerprint = %fingerprint, "Panel TLS certificate loaded");
    Ok(Arc::new(server))
}

/// Reads every certificate in a PEM chain, leaf first.
async fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let content = read_pem_file(path).await?;
    let blocks = decode_blocks(&content, "CERTIFICATE");
    if blocks.is_empty() {
        return Err(ProxyError::Config(format!(
            "{} contains no CERTIFICATE block",
            path.display()
        )));
    }
    Ok(blocks.into_iter().map(CertificateDer::from).collect())
}

/// Reads the private key, accepting the three encodings openssl emits.
async fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let content = read_pem_file(path).await?;
    if let Some(block) = decode_blocks(&content, "PRIVATE KEY").into_iter().next() {
        return Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(block)));
    }
    if let Some(block) = decode_blocks(&content, "RSA PRIVATE KEY")
        .into_iter()
        .next()
    {
        return Ok(PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(block)));
    }
    if let Some(block) = decode_blocks(&content, "EC PRIVATE KEY").into_iter().next() {
        return Ok(PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(block)));
    }
    Err(ProxyError::Config(format!(
        "{} contains no PRIVATE KEY, RSA PRIVATE KEY, or EC PRIVATE KEY block",
        path.display()
    )))
}

/// Reads one PEM file as text.
async fn read_pem_file(path: &Path) -> Result<String> {
    tokio::fs::read_to_string(path)
        .await
        .map_err(|error| ProxyError::Config(format!("failed to read {}: {error}", path.display())))
}

/// Extracts every base64 body carrying the given PEM label.
fn decode_blocks(content: &str, label: &str) -> Vec<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut blocks = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find(&begin) {
        let after_begin = &rest[start + begin.len()..];
        let Some(stop) = after_begin.find(&end) else {
            break;
        };
        let body: String = after_begin[..stop]
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        if let Ok(decoded) = STANDARD.decode(body.as_bytes()) {
            blocks.push(decoded);
        }
        rest = &after_begin[stop + end.len()..];
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_block_with_the_label_is_decoded_in_order() {
        let pem = "\
-----BEGIN CERTIFICATE-----
AQID
-----END CERTIFICATE-----
noise between blocks
-----BEGIN CERTIFICATE-----
BAUG
-----END CERTIFICATE-----
";
        let blocks = decode_blocks(pem, "CERTIFICATE");
        assert_eq!(blocks, vec![vec![1u8, 2, 3], vec![4u8, 5, 6]]);
    }

    #[test]
    fn a_foreign_label_is_ignored() {
        let pem = "-----BEGIN PRIVATE KEY-----\nAQID\n-----END PRIVATE KEY-----\n";
        assert!(decode_blocks(pem, "CERTIFICATE").is_empty());
        assert_eq!(decode_blocks(pem, "PRIVATE KEY"), vec![vec![1u8, 2, 3]]);
    }

    #[test]
    fn an_unterminated_block_is_not_decoded() {
        let pem = "-----BEGIN CERTIFICATE-----\nAQID\n";
        assert!(decode_blocks(pem, "CERTIFICATE").is_empty());
    }
}
