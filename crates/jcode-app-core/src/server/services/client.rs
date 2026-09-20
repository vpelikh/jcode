//! Client service handle.

use crate::provider::Provider;
use crate::server::debug::ClientConnectionInfo;
use crate::server::Server;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Owns the client connection registry and connection-scoped state.
///
/// Zero-behavior grouping for now; future client service methods
/// (`register_connection`, `cleanup_connection`, `connected_clients_snapshot`,
/// ...) live here.
#[derive(Clone)]
pub(crate) struct ClientServiceHandle {
    /// Number of connected clients.
    pub(crate) client_count: Arc<RwLock<usize>>,
    /// Connected client mapping (client_id -> session_info).
    pub(crate) client_connections: Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    /// The provider template used to fork per-session providers.
    pub(crate) provider: Arc<dyn Provider>,
}

impl ClientServiceHandle {
    pub(crate) fn from_server(server: &Server) -> Self {
        Self {
            client_count: Arc::clone(&server.client_count),
            client_connections: Arc::clone(&server.client_connections),
            provider: Arc::clone(&server.provider),
        }
    }
}