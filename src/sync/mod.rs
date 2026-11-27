//! `tsk-sync`, the shared-workspace server: one SQLite authority that orders ops and owns
//! task numbers. Built only with the `sync-server` feature. New code: one way, no fallbacks.

pub mod config;
pub mod db;
pub mod server;

/// The protocol version this server speaks. Requests below the minimum are refused.
pub const PROTOCOL_VERSION: u32 = 1;
