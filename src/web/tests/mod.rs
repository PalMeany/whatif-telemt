//! Integration tests for the WEB relay.
//!
//! Submodules:
//! - `harness`: shared fixtures, a loopback echo backend, and a raw HTTP client
//! - `session_tests`: session, stream, and carrier-queue behaviour
//! - `budget_tests`: pending-budget conservation and the ceilings that use it
//! - `lifecycle_tests`: bootstrap, session, and capability lifetimes
//! - `internal_backend_tests`: a real MTProto handshake through the carrier
//! - `http_tests`: the public HTTP surface driven over a real listener
//! - `upstream_tests`: application mode, where the operator's site is a proxy
//! - `ws_tests`: the WebSocket carriers driven over a real listener

mod budget_tests;
mod harness;
mod http_tests;
mod internal_backend_tests;
mod lifecycle_tests;
mod session_tests;
mod upstream_tests;
mod ws_tests;
