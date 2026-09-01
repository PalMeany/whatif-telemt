//! Configuration tests that are grouped by subsystem rather than by loader
//! stage. The older `load_*_tests.rs` files in this directory are attached to
//! `config::load` with `#[path]`; new per-subsystem suites live here.

mod fork_tests;
mod fork_web_tests;
mod panel_tests;
