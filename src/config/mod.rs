//! Configuration.

pub(crate) mod defaults;
pub mod fork;
pub mod hot_reload;
mod load;
mod types;

#[cfg(test)]
mod tests;

pub use fork::{
    ForkApiConfig, ForkConfig, ForkPrometheusConfig, ForkRuntimeConfig, ForkTelegramConfig,
    WebImplementation,
};
pub use load::ProxyConfig;
pub(crate) use load::{ConfigSourceGraph, LoadedConfig};
pub use types::*;
