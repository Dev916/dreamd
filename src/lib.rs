//! dreamd: out-of-band consolidation of Claude Code auto-memory. The binary
//! (`src/main.rs`) is a thin CLI over these modules; the API server and the
//! integration tests use them directly.

pub mod aggregate;
pub mod api;
pub mod apply;
pub mod claude;
pub mod cli;
pub mod config;
pub mod db;
pub mod discover;
pub mod doctor;
pub mod dream;
pub mod fsops;
pub mod model;
pub mod notify;
pub mod postcheck;
pub mod transcript;
pub mod worker;
