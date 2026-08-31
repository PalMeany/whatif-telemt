//! Configuration of the fork's additions to the control-plane HTTP API.

use serde::{Deserialize, Serialize};

use crate::error::{ProxyError, Result};

use super::defaults::{default_bulk_max_operations, default_bulk_timeout_secs};

/// Hard ceiling on `bulk.max_operations`, so one request cannot be made to
/// exceed the API listener's own connection deadline by configuration alone.
const MAX_BULK_OPERATIONS: usize = 1000;

/// Fork-only control-plane API settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkApiConfig {
    /// Serves `POST /v1/bulk`. Off by default.
    #[serde(default)]
    pub bulk_enabled: bool,

    /// Operations one bulk request may carry.
    #[serde(default = "default_bulk_max_operations")]
    pub bulk_max_operations: usize,

    /// Wall-clock budget for one bulk request, in seconds.
    ///
    /// The API listener drops a connection after 15 seconds with no response
    /// at all, so a bulk that would outrun that budget is cut short and
    /// reports what it completed instead.
    #[serde(default = "default_bulk_timeout_secs")]
    pub bulk_timeout_secs: u16,
}

impl Default for ForkApiConfig {
    fn default() -> Self {
        Self {
            bulk_enabled: false,
            bulk_max_operations: default_bulk_max_operations(),
            bulk_timeout_secs: default_bulk_timeout_secs(),
        }
    }
}

impl ForkApiConfig {
    /// Validates the fork API settings when any of them is enabled.
    pub(super) fn validate(&self) -> Result<()> {
        if !self.bulk_enabled {
            return Ok(());
        }
        if self.bulk_max_operations == 0 || self.bulk_max_operations > MAX_BULK_OPERATIONS {
            return Err(ProxyError::Config(format!(
                "fork.api.bulk_max_operations must be 1..={MAX_BULK_OPERATIONS}"
            )));
        }
        if self.bulk_timeout_secs == 0 || self.bulk_timeout_secs > 14 {
            return Err(ProxyError::Config(
                "fork.api.bulk_timeout_secs must be 1..=14, below the API connection deadline"
                    .to_string(),
            ));
        }
        Ok(())
    }
}
