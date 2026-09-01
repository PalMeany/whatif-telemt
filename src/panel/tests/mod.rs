//! Panel integration tests.
//!
//! Submodules:
//! - `harness`: fixtures, a stand-in Control API, and a raw HTTP client
//! - `auth_tests`: sign-in, session cookies, CSRF, and throttling
//! - `relay_tests`: the Control API relay and its role gate
//! - `cluster_tests`: the signed node-to-node endpoint
//! - `surface_tests`: the static shell and the hardening headers

mod auth_tests;
mod cluster_tests;
mod harness;
mod relay_tests;
mod surface_tests;
