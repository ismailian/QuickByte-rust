//! QuickByte's download engine — the Rust port of `QuickByte.Core`.
//!
//! All downloading logic lives here: protocols, segmentation, retry, merging,
//! queues, persistence. Nothing here knows about egui or windows; front-ends
//! (the GUI, the scheduler agent, `qb`) compose these services and subscribe to
//! their events. See `rust/MIGRATION.md` for the architecture.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod commands;
pub mod enums;
pub mod error;
pub mod events;
pub mod ftp;
pub mod helpers;
pub mod models;
pub mod net;
pub mod platform;
pub mod serde_compat;
pub mod services;
pub mod streaming;
pub mod traits;

pub use error::{CoreError, Result};

/// The product version, from the workspace manifest (`[workspace.package] version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
