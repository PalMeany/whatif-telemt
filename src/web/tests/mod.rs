//! Integration tests for the WEB relay.
//!
//! Submodules:
//! - `harness`: shared fixtures, a loopback echo backend, and a raw HTTP client
//! - `session_tests`: session, stream, and carrier-queue behaviour
//! - `internal_backend_tests`: a real MTProto handshake through the carrier
//! - `http_tests`: the public HTTP surface driven over a real listener
//! - `ws_tests`: the WebSocket carriers driven over a real listener

mod harness;
mod http_tests;
mod internal_backend_tests;
mod session_tests;
mod ws_tests;
