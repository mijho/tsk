//! `tsk-sync`, the shared-workspace server: one SQLite authority that orders ops and owns
//! task numbers. Built only with the `sync-server` feature. New code: one way, no fallbacks.

pub mod authority;
pub mod config;
pub mod db;
pub mod server;
pub mod tokens;

/// The protocol version this server speaks.
pub const PROTOCOL_VERSION: u32 = 1;
