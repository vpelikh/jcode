//! Branded identity types for the session log and beyond (deepseek-harness #12).
//!
//! This module re-exports the canonical branded identity types from the shared
//! `jcode-id-types` leaf crate. Historically these were defined inline here
//! (via the local `branded_id!` macro); they now live in the shared crate so
//! the identity-bearing `-types` crates (`jcode-message-types`,
//! `jcode-protocol`, `jcode-background-types`, ...) can depend on them without
//! a dependency cycle, since none of those crates depends on `jcode-base`.
//!
//! `event_types.rs` imports via `crate::session::{CompactionId, EventId, MessageId}`,
//! so this re-export keeps every existing path resolving unchanged.

pub use jcode_id_types::{
    branded_id, CompactionId, EventId, JobId, MessageId, SessionId, ToolCallId,
};