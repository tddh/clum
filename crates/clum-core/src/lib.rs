//! Shared types and data structures for the yunying MCP server and bridge.
//!
//! This crate provides the foundational types used across the yunying ecosystem,
//! including host configuration, session/panel metadata, and audit event records.

pub mod types;

pub use types::*;

/// Maximum allowed JSON frame size (64 MB)
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;
