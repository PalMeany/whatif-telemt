//! The Telegram admin bot.
//!
//! A process-scoped task that long-polls the Bot API and answers a fixed
//! command set from the same control plane the HTTP API writes through. It is
//! process-scoped rather than generation-scoped on purpose: a bot cancelled and
//! respawned on every reload would drop its long poll and re-deliver whatever
//! was in flight.
//!
//! Off by default, read-only when on: an enabled bot answers status commands
//! only until `[fork.telegram] allow_mutations` is set, so a leaked token
//! cannot rotate a secret or delete a user.
//!
//! Submodules:
//! - `client`: the two Bot API calls this bot makes
//! - `commands`: command parsing and replies
//! - `transport`: HTTPS to the Bot API through the configured upstreams

use std::collections::HashSet;
use std::time::Duration;

use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::api::control::ControlPlane;
use crate::config::ProxyConfig;

mod client;
mod commands;
#[cfg(test)]
mod tests;
mod transport;

use client::BotClient;
use commands::ParseError;

/// Back-off applied after a failed poll, so an unreachable Bot API is not
/// hammered while the proxy keeps serving traffic.
const POLL_BACKOFF: Duration = Duration::from_secs(5);

/// Handle to a running bot, so shutdown can stop it.
struct ActiveBot {
    /// Fires when the process is shutting down.
    shutdown: CancellationToken,
}

/// Process-global handle, mirroring how the WEB transports are held.
static ACTIVE_BOT: Mutex<Option<ActiveBot>> = Mutex::new(None);

/// Starts the bot when it is enabled.
///
/// Never fatal: a proxy whose bot cannot start is still a working proxy, and
/// refusing to serve traffic because a chat integration is unreachable would
/// be the wrong trade.
pub(crate) fn start(config: &ProxyConfig, control: ControlPlane) {
    if !config.fork.telegram_enabled() {
        return;
    }
    let settings = config.fork.telegram.clone();
    let admins: HashSet<i64> = settings.admins.iter().copied().collect();
    let shutdown = CancellationToken::new();
    *ACTIVE_BOT.lock() = Some(ActiveBot {
        shutdown: shutdown.clone(),
    });

    info!(
        admins = admins.len(),
        mutations = settings.allow_mutations,
        "Telegram admin bot started"
    );
    tokio::spawn(async move {
        let client = BotClient::new(&settings);
        let mut offset: i64 = 0;
        loop {
            // The upstream manager belongs to the live generation, so it is
            // resolved per poll: a reload must move the bot's egress with it.
            // With no scope configured the bot dials directly, so a Bot API
            // outage can never mark a client-traffic upstream unhealthy.
            let egress = (!settings.upstream_scope.is_empty()).then(|| {
                (
                    control.runtime().upstream_manager.clone(),
                    settings.upstream_scope.clone(),
                )
            });
            let polled = tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                polled = client.poll(offset, egress.clone()) => polled,
            };
            let (commands, next_offset) = match polled {
                Ok(polled) => polled,
                Err(error) => {
                    debug!(error = %error, "Telegram poll failed");
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => break,
                        _ = tokio::time::sleep(POLL_BACKOFF) => {}
                    }
                    continue;
                }
            };
            offset = next_offset;

            for command in commands {
                // Both the sender and the chat have to be allowed. Checking the
                // sender alone would let an admin type `/rotate alice` in a
                // shared group and have the bot post the new secret to everyone
                // in it, because the reply goes to the chat the command arrived
                // in. A group is usable deliberately, by putting its (negative)
                // chat id in `admins`.
                if !admins.contains(&command.from_id) || !admins.contains(&command.chat_id) {
                    // Answering would confirm the bot exists to whoever found
                    // the token or guessed the username.
                    debug!(
                        update_id = command.update_id,
                        "Dropping a Telegram update from a chat or sender that is not an admin"
                    );
                    continue;
                }
                let reply = match commands::parse(&command.text) {
                    Ok(parsed) => {
                        commands::answer(parsed, &control, settings.allow_mutations).await
                    }
                    Err(ParseError::NotACommand) => continue,
                    Err(ParseError::Usage(usage)) => format!("usage: {usage}"),
                    Err(ParseError::Unknown(name)) => {
                        format!("unknown command /{name}; try /help")
                    }
                };
                if let Err(error) = client.send(command.chat_id, &reply, egress.clone()).await {
                    warn!(error = %error, "Telegram reply failed");
                }
            }
        }
        info!("Telegram admin bot stopped");
    });
}

/// Stops the bot, if one is running.
pub(crate) fn shutdown() {
    if let Some(bot) = ACTIVE_BOT.lock().take() {
        bot.shutdown.cancel();
    }
}
