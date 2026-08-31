//! WhatIfTelemt — Telegram MTProto Proxy
//!
//! An unofficial, modified version of Telemt (<https://github.com/telemt/telemt>).
//! Not affiliated with, endorsed by, or supported by the Telemt project.

/// Product name shown to operators.
///
/// Deliberately not "Telemt": TELEMT PUBLIC LICENSE 3.3 §3 grants no permission
/// to use the Telemt name or branding, and permits naming a modified version
/// only when it is clearly identified as modified and unofficial. See NOTICE.md.
pub const PRODUCT: &str = "WhatIfTelemt";

/// Full version: the upstream Telemt release this forked from, plus this fork's
/// own sub-version.
///
/// `Cargo.toml` deliberately keeps the upstream number (`3.4.25`) so it stays
/// obvious which Telemt release this tree came from; the suffix identifies the
/// fork build. Bump the suffix HERE, not in `Cargo.toml`.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), ".1w");

mod api;
mod cli;
mod config;
mod conntrack_control;
mod crypto;
#[cfg(unix)]
mod daemon;
mod error;
mod fork;
mod healthcheck;
mod ip_tracker;
#[cfg(test)]
#[path = "tests/ip_tracker_encapsulation_adversarial_tests.rs"]
mod ip_tracker_encapsulation_adversarial_tests;
#[cfg(test)]
#[path = "tests/ip_tracker_hotpath_adversarial_tests.rs"]
mod ip_tracker_hotpath_adversarial_tests;
#[cfg(test)]
#[path = "tests/ip_tracker_regression_tests.rs"]
mod ip_tracker_regression_tests;
mod logging;
mod maestro;
mod metrics;
mod network;
mod protocol;
mod proxy;
mod quota_state;
mod service;
mod startup;
mod stats;
mod stream;
mod synlimit_control;
mod tls_front;
mod transport;
mod util;
mod web;

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Install rustls crypto provider early
    let _ = rustls::crypto::ring::default_provider().install_default();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = cli::parse_command(&args);

    // Handle subcommands that don't need the server (stop, reload, status, init)
    if let Some(exit_code) = cli::execute_subcommand(&cmd) {
        std::process::exit(exit_code);
    }

    #[cfg(unix)]
    {
        let daemon_opts = cmd.daemon_opts;

        // Daemonize BEFORE runtime
        if daemon_opts.should_daemonize() {
            match daemon::daemonize(daemon_opts.working_dir.as_deref()) {
                Ok(daemon::DaemonizeResult::Parent) => {
                    std::process::exit(0);
                }
                Ok(daemon::DaemonizeResult::Child) => {
                    // continue
                }
                Err(e) => {
                    eprintln!("[telemt] Daemonization failed: {}", e);
                    std::process::exit(1);
                }
            }
        }

        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(maestro::run_with_daemon(daemon_opts))
    }

    #[cfg(not(unix))]
    {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(maestro::run())
    }
}
