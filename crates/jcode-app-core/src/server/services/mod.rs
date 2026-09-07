//! In-process service handles for the shared server.
//!
//! This module is the first step of the server service split (see
//! `docs/plans/SERVER_SERVICE_SPLIT_PLAN.md`). It introduces thin, cloneable
//! handle structs that group the server's shared state by ownership domain:
//! session, client, swarm, debug, and maintenance.
//!
//! The handles wrap the exact same `Arc`/`Arc<RwLock<_>>` fields that the
//! server already holds, so construction is a zero-behavior clone of the current
//! state bag. They introduce *ownership grouping and a home for future service
//! methods*, not a transport or process change.
//!
//! No logic is moved yet. Once these handles exist, `ServerRuntime` can hold
//! them instead of a flat field-by-field clone, and the wide `handle_client` /
//! `handle_debug_client` argument lists can be narrowed to a few typed handles.
//! Each later move narrows dependencies without changing behavior.

mod client;
mod debug;
mod maintenance;
mod session;
mod swarm;

pub(crate) use self::{client::*, debug::*, maintenance::*, session::*, swarm::*};