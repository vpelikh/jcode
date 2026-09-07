use super::client_lifecycle::handle_client;
use super::debug::handle_debug_client;
use super::services::{
    ClientServiceHandle, DebugServiceHandle, MaintenanceServiceHandle, SessionServiceHandle,
    SwarmServiceHandle,
};
use super::util::get_shared_mcp_pool;
use super::util::ServerIdentity;
use crate::ambient_runner::AmbientRunnerHandle;
use crate::gateway::GatewayClient;
use crate::transport::{Listener, Stream};
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Owns every connection task spawned by a server runtime.
///
/// Dropping a `JoinHandle` detaches its task, so accepting a connection must not
/// discard the handle. This scope gives the accept loops and their children one
/// cancellation boundary and lets server shutdown wait until all children have
/// observed cancellation and released their resources.
#[derive(Default)]
struct RuntimeTaskScope {
    cancellation: CancellationToken,
    tasks: Mutex<JoinSet<()>>,
}

impl RuntimeTaskScope {
    async fn spawn<F, Fut>(&self, task: F) -> bool
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if self.cancellation.is_cancelled() {
            return false;
        }

        let mut tasks = self.tasks.lock().await;
        while let Some(result) = tasks.try_join_next() {
            log_task_completion(result);
        }
        if self.cancellation.is_cancelled() {
            return false;
        }

        tasks.spawn(task(self.cancellation.child_token()));
        true
    }

    async fn shutdown(&self) {
        self.cancellation.cancel();
        // Drain the set before awaiting children. An accept task may already be
        // waiting to register a just-accepted connection; leaving the mutex
        // held while joining would deadlock that task. Once cancelled, any
        // late registration observes cancellation and is rejected.
        let mut tasks = {
            let mut owned_tasks = self.tasks.lock().await;
            std::mem::take(&mut *owned_tasks)
        };
        while let Some(result) = tasks.join_next().await {
            log_task_completion(result);
        }
    }

    #[cfg(test)]
    async fn task_count(&self) -> usize {
        self.tasks.lock().await.len()
    }
}

fn log_task_completion(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result
        && !error.is_cancelled()
    {
        crate::logging::error(&format!("Server connection task failed: {error}"));
    }
}

#[derive(Clone)]
pub(super) struct ServerRuntime {
    client_count: Arc<RwLock<usize>>,
    server_name: String,
    server_icon: String,
    server_identity: ServerIdentity,
    ambient_runner: Option<AmbientRunnerHandle>,
    tasks: Arc<RuntimeTaskScope>,

    /// Service handles (server service split, Slice 2). These group the shared
    /// state by ownership domain and route `handle_client` / `handle_debug_client`
    /// through typed handles instead of the flat field bag.
    session_service: SessionServiceHandle,
    client_service: ClientServiceHandle,
    swarm_service: SwarmServiceHandle,
    debug_service: DebugServiceHandle,
    maintenance_service: MaintenanceServiceHandle,
}

impl ServerRuntime {
    pub(super) fn from_server(server: &super::Server) -> Self {
        Self {
            client_count: Arc::clone(&server.client_count),
            server_name: server.identity.name.clone(),
            server_icon: server.identity.icon.clone(),
            server_identity: server.identity.clone(),
            ambient_runner: server.ambient_runner.clone(),
            tasks: Arc::new(RuntimeTaskScope::default()),

            session_service: SessionServiceHandle::from_server(server),
            client_service: ClientServiceHandle::from_server(server),
            swarm_service: SwarmServiceHandle::from_server(server),
            debug_service: DebugServiceHandle::from_server(server),
            maintenance_service: MaintenanceServiceHandle::from_server(server),
        }
    }

    pub(super) fn spawn_main_accept_loop(&self, listener: Listener) -> tokio::task::JoinHandle<()> {
        let runtime = self.clone();
        let cancellation = self.tasks.cancellation.child_token();
        tokio::spawn(async move {
            #[cfg(windows)]
            let mut listener = listener;

            loop {
                let accepted = tokio::select! {
                    _ = cancellation.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                match accepted {
                    Ok((stream, _)) => {
                        runtime.increment_client_count().await;
                        if !runtime
                            .spawn_client_task(stream, "Client error", true)
                            .await
                        {
                            runtime.decrement_client_count().await;
                            break;
                        }
                    }
                    Err(e) => {
                        crate::logging::error(&format!("Main accept error: {}", e));
                    }
                }
            }
        })
    }

    pub(super) fn spawn_debug_accept_loop(
        &self,
        listener: Listener,
        server_start_time: Instant,
    ) -> tokio::task::JoinHandle<()> {
        let runtime = self.clone();
        let cancellation = self.tasks.cancellation.child_token();
        tokio::spawn(async move {
            #[cfg(windows)]
            let mut listener = listener;

            loop {
                let accepted = tokio::select! {
                    _ = cancellation.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                match accepted {
                    Ok((stream, _)) => {
                        // Debug clients do not participate in idle-timeout accounting.
                        if !runtime
                            .spawn_debug_client_task(stream, server_start_time)
                            .await
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        crate::logging::error(&format!("Debug accept error: {}", e));
                    }
                }
            }
        })
    }

    pub(super) async fn spawn_gateway_accept_loop(
        &self,
        mut client_rx: mpsc::UnboundedReceiver<GatewayClient>,
    ) -> bool {
        let runtime = self.clone();
        self.tasks
            .spawn(move |cancellation| async move {
                loop {
                    let gw_client = tokio::select! {
                        _ = cancellation.cancelled() => break,
                        client = client_rx.recv() => match client {
                            Some(client) => client,
                            None => break,
                        },
                    };
                    runtime.increment_client_count().await;
                    crate::logging::info(&format!(
                        "Gateway client connected: {} ({})",
                        gw_client.device_name, gw_client.device_id
                    ));
                    // Preserve prior behavior: gateway sessions do not nudge the
                    // ambient runner on disconnect.
                    if !runtime.spawn_gateway_client_task(gw_client).await {
                        runtime.decrement_client_count().await;
                        break;
                    }
                }
            })
            .await
    }

    pub(super) async fn spawn_background_task<Fut>(&self, task: Fut) -> bool
    where
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.tasks
            .spawn(move |cancellation| async move {
                tokio::select! {
                    _ = cancellation.cancelled() => {}
                    _ = task => {}
                }
            })
            .await
    }

    async fn spawn_client_task(
        &self,
        stream: Stream,
        error_prefix: &'static str,
        nudge_ambient: bool,
    ) -> bool {
        let runtime = self.clone();
        self.tasks
            .spawn(move |cancellation| async move {
                runtime
                    .run_client_stream(stream, error_prefix, nudge_ambient, cancellation)
                    .await;
            })
            .await
    }

    async fn spawn_gateway_client_task(&self, gw_client: GatewayClient) -> bool {
        let runtime = self.clone();
        self.tasks
            .spawn(move |cancellation| async move {
                runtime
                    .run_client_stream(
                        gw_client.stream,
                        "Gateway client error",
                        false,
                        cancellation,
                    )
                    .await;
            })
            .await
    }

    async fn spawn_debug_client_task(&self, stream: Stream, server_start_time: Instant) -> bool {
        let runtime = self.clone();
        self.tasks
            .spawn(move |cancellation| async move {
                runtime
                    .run_debug_stream(stream, server_start_time, cancellation)
                    .await;
            })
            .await
    }

    pub(super) async fn shutdown(&self) {
        self.tasks.shutdown().await;
    }

    async fn increment_client_count(&self) {
        *self.client_count.write().await += 1;
        crate::runtime_memory_log::emit_event(
            crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                "client_connected",
                "client_count_incremented",
            ),
        );
    }

    async fn decrement_client_count(&self) {
        *self.client_count.write().await -= 1;
        crate::runtime_memory_log::emit_event(
            crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                "client_disconnected",
                "client_count_decremented",
            ),
        );
    }

    async fn run_client_stream(
        self,
        stream: Stream,
        error_prefix: &'static str,
        nudge_ambient: bool,
        cancellation: CancellationToken,
    ) {
        let result = {
            let client = async {
                let mcp_pool = get_shared_mcp_pool(&self.maintenance_service.mcp_pool).await;
                handle_client(
                    stream,
                    self.session_service.clone(),
                    self.client_service.clone(),
                    self.swarm_service.clone(),
                    self.debug_service.clone(),
                    self.server_name.clone(),
                    self.server_icon.clone(),
                    mcp_pool,
                )
                .await
            };
            tokio::pin!(client);
            tokio::select! {
                result = &mut client => Some(result),
                _ = cancellation.cancelled() => None,
            }
        };

        self.decrement_client_count().await;

        if nudge_ambient && let Some(ref runner) = self.ambient_runner {
            runner.nudge();
        }

        if let Some(Err(e)) = result {
            crate::logging::error(&format!("{}: {}", error_prefix, e));
        }
    }

    async fn run_debug_stream(
        self,
        stream: Stream,
        server_start_time: Instant,
        cancellation: CancellationToken,
    ) {
        let client = async {
            let mcp_pool = Some(get_shared_mcp_pool(&self.maintenance_service.mcp_pool).await);
            handle_debug_client(
                stream,
                self.session_service.clone(),
                self.client_service.clone(),
                self.swarm_service.clone(),
                self.debug_service.clone(),
                self.server_identity.clone(),
                server_start_time,
                self.ambient_runner.clone(),
                mcp_pool,
            )
            .await
        };
        tokio::pin!(client);
        if let Some(Err(e)) = tokio::select! {
            result = &mut client => Some(result),
            _ = cancellation.cancelled() => None,
        } {
            crate::logging::error(&format!("Debug client error: {}", e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RuntimeTaskScope;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn runtime_task_scope_cancels_and_joins_owned_tasks() {
        let scope = RuntimeTaskScope::default();
        let dropped = Arc::new(AtomicBool::new(false));
        let task_dropped = Arc::clone(&dropped);

        assert!(
            scope
                .spawn(move |cancellation| async move {
                    let _drop_flag = DropFlag(task_dropped);
                    cancellation.cancelled().await;
                })
                .await
        );
        assert_eq!(scope.task_count().await, 1);

        tokio::time::timeout(Duration::from_secs(1), scope.shutdown())
            .await
            .expect("runtime task scope should join cancelled tasks");

        assert!(dropped.load(Ordering::SeqCst));
        assert_eq!(scope.task_count().await, 0);
        assert!(
            !scope
                .spawn(|_| async { panic!("task spawned after shutdown") })
                .await
        );
    }
}
