//! Migration of the pre-`[fork]` configuration shape.
//!
//! Before telemt grew its own WEB proxy transport, this fork owned `[web]`
//! outright. Both sections now exist and describe different transports, so the
//! raw document is inspected once, before any strict-key checking, and a
//! `[web]` table written against the fork's schema is moved to `[fork.web]`.
//!
//! The two schemas share only `enabled`, `limits` and `timeouts`, and every
//! other key belongs to exactly one of them, so the decision is made on keys
//! that cannot appear in both.

use toml::Value;
use toml::value::Table;
use tracing::warn;

use crate::error::{ProxyError, Result};

/// `[web]` keys that only this fork's WEB transport has.
const FORK_ONLY_WEB_KEYS: &[&str] = &[
    "listen",
    "admin_listen",
    "hostname",
    "public_dir",
    "public_upstream",
    "carrier_mode",
    "derive_user_profiles",
    "trusted_proxies",
    "profiles",
];

/// `[web]` keys that only telemt's own WEB transport has.
const TELEMT_ONLY_WEB_KEYS: &[&str] = &[
    "carrier",
    "carriers",
    "carrier_learning",
    "carrier_negotiation_aggressiveness",
    "debug",
    "vhosts",
];

/// Moves a legacy fork-schema `[web]` table to `[fork.web]`.
///
/// Leaves a telemt-schema `[web]` alone, and refuses a table that mixes both
/// schemas rather than guessing which transport the operator meant.
pub(crate) fn migrate_fork_web_section(document: &mut Value) -> Result<()> {
    let Some(root) = document.as_table_mut() else {
        return Ok(());
    };
    let Some(web) = root.get("web").and_then(Value::as_table) else {
        return Ok(());
    };

    let fork_keys = present_keys(web, FORK_ONLY_WEB_KEYS);
    if fork_keys.is_empty() {
        return Ok(());
    }
    let telemt_keys = present_keys(web, TELEMT_ONLY_WEB_KEYS);
    if !telemt_keys.is_empty() {
        return Err(ProxyError::Config(format!(
            "[web] mixes two different WEB transports: {} belong to this fork's transport (now \
             [fork.web]) and {} belong to telemt's own. Split them into [fork.web] and [web].",
            fork_keys.join(", "),
            telemt_keys.join(", ")
        )));
    }

    if fork_web_table_exists(root) {
        return Err(ProxyError::Config(
            "[fork.web] and a legacy fork-schema [web] are both present; keep [fork.web] and \
             remove [web]"
                .to_string(),
        ));
    }

    let Some(web) = root.remove("web") else {
        return Ok(());
    };
    let fork = root
        .entry("fork".to_string())
        .or_insert_with(|| Value::Table(Table::new()));
    let Some(fork) = fork.as_table_mut() else {
        return Err(ProxyError::Config(
            "[fork] must be a table before a legacy [web] section can move into it".to_string(),
        ));
    };
    fork.insert("web".to_string(), web);

    warn!(
        "[web] was read as this fork's WEB transport and moved to [fork.web]; telemt 3.5.x owns \
         [web] for its own WEB transport, so rename the section in your configuration"
    );
    Ok(())
}

/// Reports whether `[fork.web]` is already written out.
fn fork_web_table_exists(root: &Table) -> bool {
    root.get("fork")
        .and_then(Value::as_table)
        .is_some_and(|fork| fork.contains_key("web"))
}

/// Collects which of `candidates` the table actually carries.
fn present_keys(table: &Table, candidates: &[&'static str]) -> Vec<&'static str> {
    candidates
        .iter()
        .filter(|key| table.contains_key(**key))
        .copied()
        .collect()
}
