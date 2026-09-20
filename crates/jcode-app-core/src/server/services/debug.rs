//! Debug service handle.

use crate::server::debug::ClientDebugState;
use crate::server::debug_jobs::DebugJob;
use crate::server::Server;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

/// Owns debug socket routing state, debug jobs, and the client-debug bridge.
///
/// Zero-behavior grouping for now; future debug service methods
/// (`run_debug_connection`, `submit_debug_job`, `server_snapshot`, ...) live
/// here.
#[derive(Clone)]
pub(crate) struct DebugServiceHandle {
    /// Active and available TUI debug channels.
    pub(crate) client_debug_state: Arc<RwLock<ClientDebugState>>,
    /// Channel to receive client debug responses from TUI.
    pub(crate) client_debug_response_tx: broadcast::Sender<(u64, String)>,
    /// Background debug jobs (async debug commands).
    pub(crate) debug_jobs: Arc<RwLock<HashMap<String, DebugJob>>>,
}

impl DebugServiceHandle {
    pub(crate) fn from_server(server: &Server) -> Self {
        Self {
            client_debug_state: Arc::clone(&server.client_debug_state),
            client_debug_response_tx: server.client_debug_response_tx.clone(),
            debug_jobs: Arc::clone(&server.debug_jobs),
        }
    }
}