//! The Bot API calls this bot makes.
//!
//! Two of them: `getUpdates` to long-poll, and `sendMessage` to answer. Both
//! are hand-rolled against `serde_json` rather than a Bot API crate, so the
//! bot adds no dependency and its request shapes are visible here.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tracing::debug;

use crate::config::ForkTelegramConfig;
use crate::error::{ProxyError, Result};
use crate::transport::UpstreamManager;

use super::transport::post_json;

/// Updates requested per poll.
///
/// Small on purpose: an operator's chat is not a firehose, and a bounded batch
/// keeps one slow command from delaying every queued one behind it.
const UPDATE_BATCH: u32 = 20;

/// Longest message body Telegram accepts, in UTF-16 code units.
///
/// Measured in characters here, which is conservative for every character
/// outside the basic plane and therefore never exceeds the real limit.
pub(super) const MAX_MESSAGE_CHARS: usize = 4000;

/// One update the bot cares about: a text message in an allowed chat.
#[derive(Debug)]
pub(super) struct Command {
    /// Update sequence number, used to acknowledge the batch.
    pub(super) update_id: i64,
    /// Chat the reply goes to.
    pub(super) chat_id: i64,
    /// Sender, checked against the admin list.
    pub(super) from_id: i64,
    /// Message text as sent.
    pub(super) text: String,
}

/// Bot API client bound to one token and origin.
pub(super) struct BotClient {
    origin: String,
    token: String,
    poll_timeout: Duration,
    request_timeout: Duration,
}

impl BotClient {
    /// Builds a client from the validated configuration.
    pub(super) fn new(config: &ForkTelegramConfig) -> Self {
        Self {
            origin: config.api_base.clone(),
            token: config.token.clone(),
            poll_timeout: Duration::from_secs(u64::from(config.poll_timeout_secs)),
            request_timeout: Duration::from_secs(u64::from(config.request_timeout_secs)),
        }
    }

    /// Long-polls for updates after `offset`.
    ///
    /// Returns the commands worth answering and the offset to acknowledge next.
    pub(super) async fn poll(
        &self,
        offset: i64,
        egress: Option<(Arc<UpstreamManager>, String)>,
    ) -> Result<(Vec<Command>, i64)> {
        let payload = serde_json::json!({
            "offset": offset,
            "limit": UPDATE_BATCH,
            "timeout": self.poll_timeout.as_secs(),
            // Everything else is noise for a control bot, and asking for less
            // keeps the token from being usable to read unrelated chats.
            "allowed_updates": ["message"],
        })
        .to_string();
        let response = post_json(
            &self.origin,
            &self.token,
            "getUpdates",
            &payload,
            egress,
            self.request_timeout,
        )
        .await?;
        if response.status != 200 {
            return Err(ProxyError::Proxy(format!(
                "getUpdates answered HTTP {}",
                response.status
            )));
        }
        let parsed: ApiEnvelope<Vec<Update>> = serde_json::from_slice(&response.body)
            .map_err(|error| ProxyError::Proxy(format!("getUpdates reply is not JSON: {error}")))?;
        if !parsed.ok {
            return Err(ProxyError::Proxy(format!(
                "getUpdates was refused: {}",
                parsed
                    .description
                    .unwrap_or_else(|| "no reason".to_string())
            )));
        }

        let updates = parsed.result.unwrap_or_default();
        let mut next_offset = offset;
        let mut commands = Vec::new();
        for update in updates {
            next_offset = next_offset.max(update.update_id + 1);
            let Some(message) = update.message else {
                continue;
            };
            let (Some(text), Some(from)) = (message.text, message.from) else {
                continue;
            };
            commands.push(Command {
                update_id: update.update_id,
                chat_id: message.chat.id,
                from_id: from.id,
                text,
            });
        }
        Ok((commands, next_offset))
    }

    /// Sends one reply, splitting it when it exceeds the message limit.
    pub(super) async fn send(
        &self,
        chat_id: i64,
        text: &str,
        egress: Option<(Arc<UpstreamManager>, String)>,
    ) -> Result<()> {
        for chunk in split_message(text) {
            let payload = serde_json::json!({
                "chat_id": chat_id,
                "text": chunk,
                // Plain text: a username or a secret must never be reinterpreted
                // as markup, and escaping every reply would be one more way to
                // leak a malformed one.
                "disable_web_page_preview": true,
            })
            .to_string();
            let response = post_json(
                &self.origin,
                &self.token,
                "sendMessage",
                &payload,
                egress.clone(),
                self.request_timeout,
            )
            .await?;
            if response.status != 200 {
                debug!(status = response.status, "sendMessage was refused");
                return Err(ProxyError::Proxy(format!(
                    "sendMessage answered HTTP {}",
                    response.status
                )));
            }
        }
        Ok(())
    }
}

/// Splits a reply into pieces the Bot API will accept.
///
/// Splits on line boundaries so a user list never breaks mid-row.
fn split_message(text: &str) -> Vec<String> {
    if text.chars().count() <= MAX_MESSAGE_CHARS {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let line_len = line.chars().count() + 1;
        if current.chars().count() + line_len > MAX_MESSAGE_CHARS && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        // One line longer than the whole limit still has to go somewhere, so it
        // is cut rather than dropped.
        if line_len > MAX_MESSAGE_CHARS {
            let truncated: String = line.chars().take(MAX_MESSAGE_CHARS - 1).collect();
            chunks.push(truncated);
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[derive(Deserialize)]
struct ApiEnvelope<T> {
    ok: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    result: Option<T>,
}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    chat: Chat,
    #[serde(default)]
    from: Option<Sender>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct Chat {
    id: i64,
}

#[derive(Deserialize)]
struct Sender {
    id: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_reply_is_sent_whole() {
        let chunks = split_message("one\ntwo\n");
        assert_eq!(chunks, vec!["one\ntwo\n".to_string()]);
    }

    #[test]
    fn a_long_reply_is_split_on_line_boundaries() {
        let line = "x".repeat(100);
        let text = std::iter::repeat_n(line.as_str(), 80)
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = split_message(&text);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= MAX_MESSAGE_CHARS);
            // Every piece must still be whole lines, or a user row would be
            // torn across two messages.
            for row in chunk.lines() {
                assert_eq!(row.chars().count(), 100);
            }
        }
    }

    #[test]
    fn a_single_oversized_line_is_cut_rather_than_dropped() {
        let text = "y".repeat(MAX_MESSAGE_CHARS * 2);
        let chunks = split_message(&text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chars().count(), MAX_MESSAGE_CHARS - 1);
    }
}
