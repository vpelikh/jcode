//! Maintenance service handle.

use crate::server::Server;
use std::sync::Arc;
use tokio::sync::OnceCell;

/// Owns background/maintenance state. Currently carries only the shared MCP
/// pool, which the runtime warms lazily when accepting a connection.
///
/// Zero-behavior grouping for now; future maintenance service methods
/// (`handle_reload_signal`, `spawn_background_loops`, `run_bus_monitor`, ...)
/// live here.
#[derive(Clone)]
pub(crate) struct MaintenanceServiceHandle {
    /// Shared MCP server pool (processes shared across sessions), lazily
    /// initialized.
    pub(crate) mcp_pool: Arc<OnceCell<Arc<crate::mcp::SharedMcpPool>>>,
}

impl MaintenanceServiceHandle {
    pub(crate) fn from_server(server: &Server) -> Self {
        Self {
            mcp_pool: Arc::clone(&server.mcp_pool),
        }
    }
}