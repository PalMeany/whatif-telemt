//! Everything this fork adds on top of telemt.
//!
//! The code here is reachable only through the `[fork]` configuration section,
//! so a deployment that leaves that section out runs stock telemt behaviour.
//!
//! Submodules:
//! - `prometheus`: the built-in metrics panel
//! - `switches`: process-wide mirror of switches that cannot reach a config
//! - `web`: this fork's own WEB proxy transport, an alternative to telemt's

pub(crate) mod prometheus;
pub(crate) mod switches;
pub(crate) mod web;
