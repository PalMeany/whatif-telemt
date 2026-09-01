//! TLS termination settings for the panel listener.

use serde::{Deserialize, Serialize};

use crate::error::{ProxyError, Result};

/// Optional built-in TLS for the panel listener.
///
/// The panel is a control plane: it carries session cookies and secrets, so it
/// is either terminated here with an operator-supplied certificate or fronted
/// by a TLS proxy that is itself named in `panel.trusted_proxies`. Plaintext on
/// a routable address is refused outright by [`super::PanelConfig::validate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PanelTlsConfig {
    /// Terminates TLS inside the process instead of behind a front proxy.
    #[serde(default)]
    pub enabled: bool,

    /// PEM certificate chain, leaf first.
    #[serde(default)]
    pub cert_path: String,

    /// PEM private key, PKCS#8 or PKCS#1 or SEC1.
    #[serde(default)]
    pub key_path: String,
}

impl PanelTlsConfig {
    /// Validates the certificate material paths when TLS is enabled.
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.cert_path.is_empty() || self.key_path.is_empty() {
            return Err(ProxyError::Config(
                "panel.tls.enabled requires panel.tls.cert_path and panel.tls.key_path".to_string(),
            ));
        }
        Ok(())
    }
}
