//! S2-backed multi-user synchronization protocol and server.

pub(crate) mod client;
mod config;
mod projection;
pub mod protocol;
mod s2log;
mod server;
mod validation;

pub use config::{load_config, ServerConfig};
pub use server::run;
