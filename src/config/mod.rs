//! Configuration.

pub(crate) mod defaults;
pub mod hot_reload;
mod load;
mod types;
mod web;

#[cfg(test)]
mod tests;

pub use load::ProxyConfig;
pub use types::*;
pub use web::*;
