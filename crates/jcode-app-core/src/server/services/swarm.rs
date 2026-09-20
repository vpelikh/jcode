//! Swarm service handle.

use crate::server::{
    AwaitMembersRuntime, FileTouchService, Server, SharedContext, SwarmEvent, SwarmMutationRuntime,
    SwarmState,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

/// Channel subscriptions (swarm_id -> channel -> session_ids).
type ChannelSubscriptions = Arc<RwLock<HashMap<String, HashMap<String, HashSet<String>>>>>;

/// Owns swarm membership, plans, shared context, channel subscriptions,
/// event history/broadcast, file-touch tracking, and the persisted coordination
/// runtimes.
///
/// Zero-behavior grouping for now; future swarm service methods
/// (`join_swarm`, `set_member_status`, `update_plan`, `subscribe_channel`,
/// `record_file_touch`, ...) live here.
#[derive(Clone)]
pub(crate) struct SwarmServiceHandle {
    /// Shared ownership of core swarm coordination state.
    pub(crate) swarm_state: SwarmState,
    /// Shared context by swarm (swarm_id -> key -> SharedContext).
    pub(crate) shared_context: Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
    /// File-touch tracking service (forward path index + reverse session index).
    pub(crate) file_touch: FileTouchService,
    /// Channel subscriptions forward index.
    pub(crate) channel_subscriptions: ChannelSubscriptions,
    /// Channel subscriptions reverse index (session_id -> swarm_id -> channels).
    pub(crate) channel_subscriptions_by_session: ChannelSubscriptions,
    /// Event history for real-time event subscription (ring buffer).
    pub(crate) event_history: Arc<RwLock<VecDeque<SwarmEvent>>>,
    /// Counter for event IDs.
    pub(crate) event_counter: Arc<AtomicU64>,
    /// Broadcast channel for swarm event subscriptions.
    pub(crate) swarm_event_tx: broadcast::Sender<SwarmEvent>,
    /// Persisted communicate await_members wait registry.
    pub(crate) await_members_runtime: AwaitMembersRuntime,
    /// Persisted dedupe registry for mutating swarm coordinator operations.
    pub(crate) swarm_mutation_runtime: SwarmMutationRuntime,
}

impl SwarmServiceHandle {
    pub(crate) fn from_server(server: &Server) -> Self {
        Self {
            swarm_state: server.swarm_state.clone(),
            shared_context: Arc::clone(&server.shared_context),
            file_touch: server.file_touch.clone(),
            channel_subscriptions: Arc::clone(&server.channel_subscriptions),
            channel_subscriptions_by_session: Arc::clone(&server.channel_subscriptions_by_session),
            event_history: Arc::clone(&server.event_history),
            event_counter: Arc::clone(&server.event_counter),
            swarm_event_tx: server.swarm_event_tx.clone(),
            await_members_runtime: server.await_members_runtime.clone(),
            swarm_mutation_runtime: server.swarm_mutation_runtime.clone(),
        }
    }
}