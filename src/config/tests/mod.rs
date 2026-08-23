//! Configuration tests that are grouped by subsystem rather than by loader
//! stage. The older `load_*_tests.rs` files in this directory are attached to
//! `config::load` with `#[path]`; new per-subsystem suites live here.

mod web_tests;
