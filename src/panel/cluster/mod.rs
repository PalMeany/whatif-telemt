//! Node federation.
//!
//! One panel drives others by calling their `/cluster/v1` endpoint with an
//! HMAC-signed request. There is no separate agent process and no persistent
//! channel: every linked node already runs a panel, and that panel is the
//! remote surface.
//!
//! Submodules:
//! - `sign`: canonical request description, signatures, and the replay window
//! - `link`: the opaque token an operator carries from an agent to a master
//! - `client`: the outbound half, used by a master
//! - `inbound`: the inbound half, served by an agent
//! - `poll`: background health polling of linked nodes

pub(crate) mod client;
pub(crate) mod inbound;
pub(crate) mod link;
pub(crate) mod poll;
pub(crate) mod sign;
