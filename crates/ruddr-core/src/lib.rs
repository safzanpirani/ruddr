//! Shared Ruddr building blocks. Every other crate depends on this one, and
//! nothing here starts processes or threads: it holds the contracts (state
//! files, exit codes, control messages, JSON-RPC framing) and pure helpers.

pub mod control;
pub mod duration;
pub mod error;
pub mod fsutil;
pub mod jsonrpc;
pub mod models;
pub mod paths;
pub mod process;
pub mod provider;
pub mod registry;
pub mod session;
pub mod state;
pub mod time;

pub use error::{Error, Exit, Result};

/// The Ruddr release this binary belongs to.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
