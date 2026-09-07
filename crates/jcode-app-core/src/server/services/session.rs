//! Session service handle.

use crate::agent::Agent;
use crate::protocol::ServerEvent;
use crate::server::{Server, SessionInterruptQueues};
use jcode_agent_runtime::InterruptSignal;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, broadcast};

/// Owns session identity, active sessions, shutdown signals, and soft-interrupt
/// queues. This is the session domain of the server's shared state.
///
/// Zero-behavior grouping for now; future session service methods
/// (`attach_client`, `resume_session`, `queue_soft_interrupt`,
/// `fanout_session_event`, ...) live here.
#[derive(Clone)]
pub(crate) struct SessionServiceHandle {
    pub(crate) sessions: Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>,
    pub(crate) event_tx: broadcast::Sender<ServerEvent>,
    /// Default/global session id tracking.
    pub(crate) session_id: Arc<RwLock<String>>,
    /// Current processing state.
    pub(crate) is_processing: Arc<RwLock<bool>>,
    /// Graceful shutdown signals by session id.
    pub(crate) shutdown_signals: Arc<RwLock<HashMap<String, InterruptSignal>>>,
    /// Soft-interrupt queues by session id.
    pub(crate) soft_interrupt_queues: SessionInterruptQueues,
}

impl SessionServiceHandle {
    pub(crate) fn from_server(server: &Server) -> Self {
        Self {
            sessions: Arc::clone(&server.sessions),
            event_tx: server.event_tx.clone(),
            session_id: Arc::clone(&server.session_id),
            is_processing: Arc::clone(&server.is_processing),
            shutdown_signals: Arc::clone(&server.shutdown_signals),
            soft_interrupt_queues: Arc::clone(&server.soft_interrupt_queues),
        }
    }
}