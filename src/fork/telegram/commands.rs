//! Command parsing and replies.
//!
//! Every reply is plain text. A username or a secret must never be
//! reinterpreted as markup, and escaping each one would be one more way to
//! leak a malformed message.

use crate::api::bulk::BulkAction;
use crate::api::control::ControlPlane;

/// Commands the bot understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Command {
    /// Lists the commands this build answers.
    Help,
    /// Process, transport and connection summary.
    Status,
    /// Configured users with their live counters.
    Users,
    /// `tg://` links for one user.
    Links(String),
    /// Adds a user and returns their generated secret.
    AddUser(String),
    /// Removes a user.
    DeleteUser(String),
    /// Admits a user again.
    Enable(String),
    /// Stops admitting a user and cancels their sessions.
    Disable(String),
    /// Issues a fresh secret for a user.
    Rotate(String),
}

impl Command {
    /// Whether answering this command changes the configuration.
    pub(super) fn mutates(&self) -> bool {
        matches!(
            self,
            Command::AddUser(_)
                | Command::DeleteUser(_)
                | Command::Enable(_)
                | Command::Disable(_)
                | Command::Rotate(_)
        )
    }
}

/// Why a message was not turned into a command.
#[derive(Debug)]
pub(super) enum ParseError {
    /// The message is not addressed to this bot at all.
    NotACommand,
    /// The command exists but was given the wrong arguments.
    Usage(String),
    /// The command is not one this build answers.
    Unknown(String),
}

/// Parses one message into a command.
///
/// Accepts the `/command@botname` form Telegram uses in groups.
pub(super) fn parse(text: &str) -> Result<Command, ParseError> {
    let mut parts = text.split_whitespace();
    let Some(head) = parts.next() else {
        return Err(ParseError::NotACommand);
    };
    let Some(name) = head.strip_prefix('/') else {
        return Err(ParseError::NotACommand);
    };
    let name = name.split('@').next().unwrap_or(name).to_ascii_lowercase();
    let argument = parts.next().map(str::to_string);
    let extra = parts.next();

    let need_one = |command: fn(String) -> Command, usage: &str| match (&argument, extra) {
        (Some(value), None) => Ok(command(value.clone())),
        _ => Err(ParseError::Usage(usage.to_string())),
    };

    match name.as_str() {
        "start" | "help" => Ok(Command::Help),
        "status" => Ok(Command::Status),
        "users" => Ok(Command::Users),
        "links" => need_one(Command::Links, "/links <user>"),
        "adduser" => need_one(Command::AddUser, "/adduser <user>"),
        "deluser" => need_one(Command::DeleteUser, "/deluser <user>"),
        "enable" => need_one(Command::Enable, "/enable <user>"),
        "disable" => need_one(Command::Disable, "/disable <user>"),
        "rotate" => need_one(Command::Rotate, "/rotate <user>"),
        other => Err(ParseError::Unknown(other.to_string())),
    }
}

/// Renders the reply for one command.
pub(super) async fn answer(
    command: Command,
    control: &ControlPlane,
    allow_mutations: bool,
) -> String {
    if command.mutates() && !allow_mutations {
        return "This bot is read-only. Set [fork.telegram] allow_mutations = true to enable \
                configuration commands."
            .to_string();
    }
    match command {
        Command::Help => help_text(allow_mutations),
        Command::Status => status(control).await,
        Command::Users => users(control).await,
        Command::Links(user) => links(control, &user).await,
        Command::AddUser(user) => {
            match control
                .apply(
                    BulkAction::UserCreate,
                    None,
                    Some(serde_json::json!({ "username": user })),
                )
                .await
            {
                Ok(Some(secret)) => format!("added {user}\nsecret: {secret}"),
                Ok(None) => format!("added {user}"),
                Err(reason) => format!("could not add {user}: {reason}"),
            }
        }
        Command::DeleteUser(user) => {
            mutate(control, BulkAction::UserDelete, &user, "removed").await
        }
        Command::Enable(user) => mutate(control, BulkAction::UserEnable, &user, "enabled").await,
        Command::Disable(user) => mutate(control, BulkAction::UserDisable, &user, "disabled").await,
        Command::Rotate(user) => {
            match control
                .apply(BulkAction::UserRotateSecret, Some(user.clone()), None)
                .await
            {
                Ok(Some(secret)) => format!("rotated {user}\nsecret: {secret}"),
                Ok(None) => format!("rotated {user}"),
                Err(reason) => format!("could not rotate {user}: {reason}"),
            }
        }
    }
}

async fn mutate(control: &ControlPlane, action: BulkAction, user: &str, verb: &str) -> String {
    match control.apply(action, Some(user.to_string()), None).await {
        Ok(_) => format!("{verb} {user}"),
        Err(reason) => format!("could not {} {user}: {reason}", action.as_str()),
    }
}

fn help_text(allow_mutations: bool) -> String {
    let mut text = String::from(
        "/status - proxy state\n\
         /users - configured users\n\
         /links <user> - proxy links for one user\n",
    );
    if allow_mutations {
        text.push_str(
            "/adduser <user> - add a user and return their secret\n\
             /deluser <user> - remove a user\n\
             /enable <user> - admit a user again\n\
             /disable <user> - stop admitting a user\n\
             /rotate <user> - issue a fresh secret\n",
        );
    } else {
        text.push_str("\nConfiguration commands are disabled ([fork.telegram] allow_mutations).\n");
    }
    text
}

async fn status(control: &ControlPlane) -> String {
    let runtime = control.runtime();
    let config = control.runtime_config();
    let stats = runtime.stats.as_ref();

    let mut text = String::new();
    text.push_str(&format!("{} {}\n", crate::PRODUCT, crate::VERSION));
    text.push_str(&format!(
        "uptime: {}\n",
        format_uptime(stats.uptime_secs() as u64)
    ));
    text.push_str(&format!(
        "connections: {} (direct {}, middle-end {})\n",
        stats.get_current_connections_total(),
        stats.get_current_connections_direct(),
        stats.get_current_connections_me()
    ));
    text.push_str(&format!(
        "accepted: {}, rejected: {}\n",
        stats.get_connects_all(),
        stats.get_connects_bad()
    ));
    text.push_str(&format!("users: {}\n", config.access.users.len()));

    let telemt_web = config
        .fork
        .telemt_web_enabled(config.telemt_web_requested());
    text.push_str(&format!(
        "WEB transport: telemt {}, fork {}\n",
        if telemt_web { "on" } else { "off" },
        if config.fork.web_enabled() {
            "on"
        } else {
            "off"
        }
    ));
    if !config.fork.enabled {
        text.push_str("fork features: disabled\n");
    }
    text
}

async fn users(control: &ControlPlane) -> String {
    match control.list_users().await {
        Ok(users) if users.is_empty() => "no users configured".to_string(),
        Ok(users) => {
            let mut text = String::new();
            for user in users {
                let quota = match user.quota_bytes {
                    Some(limit) => {
                        format!(" {}/{}", human_bytes(user.used_bytes), human_bytes(limit))
                    }
                    None => format!(" {}", human_bytes(user.used_bytes)),
                };
                text.push_str(&format!(
                    "{}{} conns {}{}\n",
                    user.name,
                    if user.enabled { "" } else { " [disabled]" },
                    user.connections,
                    quota
                ));
            }
            text
        }
        Err(reason) => format!("could not read users: {reason}"),
    }
}

async fn links(control: &ControlPlane, user: &str) -> String {
    match control.user_links(user).await {
        Ok(links) if links.is_empty() => format!("{user} has no links to show"),
        Ok(links) => links.join("\n"),
        Err(reason) => format!("could not read links for {user}: {reason}"),
    }
}

/// Renders a byte count for a chat line.
fn human_bytes(value: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut scaled = value as f64;
    let mut unit = 0;
    while scaled >= 1024.0 && unit < UNITS.len() - 1 {
        scaled /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{scaled:.1} {}", UNITS[unit])
    }
}

/// Renders an uptime for a chat line.
fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m {}s", seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_message_is_not_a_command() {
        assert!(matches!(parse("hello"), Err(ParseError::NotACommand)));
        assert!(matches!(parse(""), Err(ParseError::NotACommand)));
    }

    #[test]
    fn the_group_form_of_a_command_is_accepted() {
        // Telegram appends `@botname` in groups, and a bot that ignored it
        // would look dead in exactly the chat an operator shares with a team.
        assert_eq!(parse("/status@telemt_bot").unwrap(), Command::Status);
    }

    #[test]
    fn a_command_is_case_insensitive() {
        assert_eq!(parse("/STATUS").unwrap(), Command::Status);
    }

    #[test]
    fn a_command_that_needs_an_argument_reports_its_usage() {
        match parse("/links") {
            Err(ParseError::Usage(usage)) => assert_eq!(usage, "/links <user>"),
            _ => panic!("a missing argument must report the usage"),
        }
        match parse("/links alice bob") {
            Err(ParseError::Usage(usage)) => assert_eq!(usage, "/links <user>"),
            _ => panic!("a surplus argument must report the usage"),
        }
    }

    #[test]
    fn an_unknown_command_names_itself() {
        match parse("/frobnicate") {
            Err(ParseError::Unknown(name)) => assert_eq!(name, "frobnicate"),
            _ => panic!("an unknown command must name itself"),
        }
    }

    #[test]
    fn only_configuration_commands_are_marked_as_mutating() {
        assert!(!Command::Status.mutates());
        assert!(!Command::Users.mutates());
        assert!(!Command::Links("a".to_string()).mutates());
        assert!(Command::AddUser("a".to_string()).mutates());
        assert!(Command::DeleteUser("a".to_string()).mutates());
        assert!(Command::Enable("a".to_string()).mutates());
        assert!(Command::Disable("a".to_string()).mutates());
        assert!(Command::Rotate("a".to_string()).mutates());
    }

    #[test]
    fn the_help_text_hides_commands_that_would_be_refused() {
        let read_only = help_text(false);
        assert!(!read_only.contains("/adduser"));
        assert!(read_only.contains("allow_mutations"));
        assert!(help_text(true).contains("/adduser"));
    }

    #[test]
    fn byte_counts_render_with_a_unit() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
    }
}
