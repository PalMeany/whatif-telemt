//! Configuration of the Telegram admin bot.
//!
//! The bot is a control-plane client: it long-polls the Bot API and answers a
//! fixed command set from the same runtime state the HTTP API reads. It is a
//! process-scoped task, so it survives configuration reloads.

use serde::{Deserialize, Serialize};

use crate::error::{ProxyError, Result};

use super::defaults::{
    default_telegram_api_base, default_telegram_poll_timeout_secs,
    default_telegram_request_timeout_secs,
};

/// Largest admin list a deployment may configure.
const MAX_ADMINS: usize = 64;

/// Telegram admin bot settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkTelegramConfig {
    /// Runs the bot. Off by default.
    #[serde(default)]
    pub enabled: bool,

    /// Bot token issued by @BotFather.
    ///
    /// Held as a plain string like every other credential in this
    /// configuration; treat the config file as secret material.
    #[serde(default)]
    pub token: String,

    /// Telegram user ids allowed to talk to the bot.
    ///
    /// An update from any other chat is dropped without a reply, so the bot
    /// does not confirm its own existence to a stranger.
    #[serde(default)]
    pub admins: Vec<i64>,

    /// Permits commands that change configuration.
    ///
    /// Off by default: an enabled bot answers read-only commands only, so a
    /// leaked token cannot rotate a secret or delete a user.
    #[serde(default)]
    pub allow_mutations: bool,

    /// Bot API origin, for a self-hosted Bot API server.
    #[serde(default = "default_telegram_api_base")]
    pub api_base: String,

    /// `getUpdates` long-poll timeout, in seconds.
    #[serde(default = "default_telegram_poll_timeout_secs")]
    pub poll_timeout_secs: u16,

    /// Per-request timeout applied to every Bot API call, in seconds.
    #[serde(default = "default_telegram_request_timeout_secs")]
    pub request_timeout_secs: u16,

    /// Chats that receive unsolicited notices, such as a failed reload.
    ///
    /// Empty sends nothing; the bot only answers what it is asked.
    #[serde(default)]
    pub notify_chats: Vec<i64>,
}

impl Default for ForkTelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token: String::new(),
            admins: Vec::new(),
            allow_mutations: false,
            api_base: default_telegram_api_base(),
            poll_timeout_secs: default_telegram_poll_timeout_secs(),
            request_timeout_secs: default_telegram_request_timeout_secs(),
            notify_chats: Vec::new(),
        }
    }
}

impl ForkTelegramConfig {
    /// Validates the bot settings when it is enabled.
    pub(super) fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !token_is_well_formed(&self.token) {
            return Err(ProxyError::Config(
                "fork.telegram.token must look like `<bot id>:<secret>` as issued by @BotFather"
                    .to_string(),
            ));
        }
        if self.admins.is_empty() {
            return Err(ProxyError::Config(
                "fork.telegram.admins must list at least one Telegram user id; a bot with no \
                 admins would answer nobody"
                    .to_string(),
            ));
        }
        if self.admins.len() > MAX_ADMINS {
            return Err(ProxyError::Config(format!(
                "fork.telegram.admins must contain at most {MAX_ADMINS} entries"
            )));
        }
        if self.admins.contains(&0) {
            return Err(ProxyError::Config(
                "fork.telegram.admins must not contain 0".to_string(),
            ));
        }
        if !self.api_base.starts_with("https://") && !self.api_base.starts_with("http://") {
            return Err(ProxyError::Config(
                "fork.telegram.api_base must be an http:// or https:// origin".to_string(),
            ));
        }
        if self.api_base.ends_with('/') {
            return Err(ProxyError::Config(
                "fork.telegram.api_base must not end with '/'".to_string(),
            ));
        }
        // Telegram caps a long poll at 50 seconds, and a 0 turns the poll into
        // a busy loop against the Bot API.
        if self.poll_timeout_secs == 0 || self.poll_timeout_secs > 50 {
            return Err(ProxyError::Config(
                "fork.telegram.poll_timeout_secs must be 1..=50".to_string(),
            ));
        }
        // The request has to outlive the long poll it carries, or every poll
        // is cancelled by its own timeout.
        if self.request_timeout_secs <= self.poll_timeout_secs {
            return Err(ProxyError::Config(
                "fork.telegram.request_timeout_secs must be greater than poll_timeout_secs"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

/// Checks the `<digits>:<secret>` shape without accepting the secret's content.
fn token_is_well_formed(token: &str) -> bool {
    let Some((id, secret)) = token.split_once(':') else {
        return false;
    };
    !id.is_empty()
        && id.bytes().all(|b| b.is_ascii_digit())
        && secret.len() >= 8
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
