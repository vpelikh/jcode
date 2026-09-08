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

    /// Whether a freshly arrived (or reconnecting) member should be marked
    /// `ready` after subscribe: `true` unless the member is currently mid-turn
    /// (`running`). Moving this read behind the swarm service keeps session
    /// lifecycle code from reaching into the raw membership map.
    pub(crate) async fn member_should_mark_ready(&self, session_id: &str) -> bool {
        let members = self.swarm_state.members.read().await;
        members
            .get(session_id)
            .is_none_or(|member| member.status != "running")
    }

    /// Rename a swarm member's identity when its session id changes (resume /
    /// re-subscribe under a new id). Preserves the spawn tree by re-pointing
    /// `report_back_to_session_id` from the old id to the new one, and moves
    /// the member between `swarms_by_id` sets without holding both maps at
    /// once.
    pub(crate) async fn rename_member_session(&self, old_session_id: &str, new_session_id: &str) {
        // Never hold both swarm maps at once. Coordinator cleanup reads them in
        // the opposite order, so retaining the member write guard while waiting
        // for the swarm map can permanently deadlock reconnects and every later
        // subscribe.
        let renamed_swarm_id = {
            let mut members = self.swarm_state.members.write().await;
            let renamed_swarm_id = members.remove(old_session_id).and_then(|mut member| {
                let swarm_id = member.swarm_id.clone();
                member.session_id = new_session_id.to_string();
                member.status = "ready".to_string();
                member.detail = None;
                members.insert(new_session_id.to_string(), member);
                swarm_id
            });

            // Keep the spawn tree intact across the rename: children that
            // reported back to the old session id must follow it.
            for member in members.values_mut() {
                if member.report_back_to_session_id.as_deref() == Some(old_session_id) {
                    member.report_back_to_session_id = Some(new_session_id.to_string());
                }
            }
            renamed_swarm_id
        };

        if let Some(swarm_id) = renamed_swarm_id {
            let mut swarms = self.swarm_state.swarms_by_id.write().await;
            if let Some(swarm) = swarms.get_mut(&swarm_id) {
                swarm.remove(old_session_id);
                swarm.insert(new_session_id.to_string());
            }
        }
    }
}