//! Swarm service handle.

use crate::protocol::ServerEvent;
use crate::server::{
    AwaitMembersRuntime, FileTouchService, Server, SharedContext, SwarmEvent, SwarmEventType,
    SwarmMember, SwarmMutationRuntime, SwarmState,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;
use tokio::sync::{RwLock, broadcast, mpsc};

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

/// Identity carried by a session's swarm membership, returned when a `/clear`
/// tears the old member down so the caller can re-register the fresh root with
/// the same swarm intent.
pub(crate) struct MemberIdentity {
    pub(crate) swarm_id: Option<String>,
    pub(crate) swarm_enabled: bool,
    pub(crate) friendly_name: Option<String>,
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

    /// Register (or refresh) a session as a swarm member, mirroring
    /// `SwarmServiceHandle`'s downstream clients. Idempotent: an existing
    /// member is refreshed with the connection's event senders and the resolved
    /// identity, a new one is inserted with a fresh `ready` status. When the
    /// member is brand new and swarm-enabled, it is added to `swarms_by_id` and
    /// a `joined` member-change event is recorded.
    ///
    /// The session identity values (`working_dir`, `derived_swarm_id`,
    /// `member_name`) are resolved by the caller from the agent so swarm
    /// ownership stays here while session owns its agent. Returns whether a new
    /// member was inserted.
    #[expect(
        clippy::too_many_arguments,
        reason = "registering a swarm member carries session identity, connection identity, resolved working dir / swarm id, and event senders"
    )]
    pub(crate) async fn ensure_member(
        &self,
        client_session_id: &str,
        client_connection_id: &str,
        member_name: Option<String>,
        working_dir: Option<PathBuf>,
        derived_swarm_id: Option<String>,
        swarm_enabled: bool,
        client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    ) -> bool {
        let mut inserted = false;
        {
            let mut members = self.swarm_state.members.write().await;
            if let Some(member) = members.get_mut(client_session_id) {
                member.event_tx = client_event_tx.clone();
                member
                    .event_txs
                    .insert(client_connection_id.to_string(), client_event_tx.clone());
                member.swarm_enabled = swarm_enabled;
                member.is_headless = false;
                if member_name.is_some() {
                    member.friendly_name = member_name.clone();
                }
            } else {
                let now = Instant::now();
                members.insert(
                    client_session_id.to_string(),
                    SwarmMember {
                        session_id: client_session_id.to_string(),
                        event_tx: client_event_tx.clone(),
                        event_txs: HashMap::from([(
                            client_connection_id.to_string(),
                            client_event_tx.clone(),
                        )]),
                        working_dir: working_dir.clone(),
                        swarm_id: derived_swarm_id.clone(),
                        swarm_enabled,
                        status: "ready".to_string(),
                        detail: None,
                        task_label: None,
                        friendly_name: member_name.clone(),
                        report_back_to_session_id: None,
                        latest_completion_report: None,
                        role: "agent".to_string(),
                        joined_at: now,
                        last_status_change: now,
                        is_headless: false,
                        output_tail: None,
                        todo_progress: None,
                        todo_items: Vec::new(),
                        runtime: crate::protocol::SwarmMemberRuntime::default(),
                    },
                );
                inserted = true;
            }
        }

        if inserted && let Some(ref swarm_id_ref) = derived_swarm_id {
            let mut swarms = self.swarm_state.swarms_by_id.write().await;
            swarms
                .entry(swarm_id_ref.to_string())
                .or_insert_with(HashSet::new)
                .insert(client_session_id.to_string());
            drop(swarms);
            super::super::swarm::record_swarm_event(
                &self.event_history,
                &self.event_counter,
                &self.swarm_event_tx,
                client_session_id.to_string(),
                member_name,
                Some(swarm_id_ref.to_string()),
                SwarmEventType::MemberChange {
                    action: "joined".to_string(),
                },
            )
            .await;
        }

        crate::logging::event_info(
            "SESSION_LIFECYCLE",
            vec![
                ("phase", "swarm_member_registered".to_string()),
                ("session_id", client_session_id.to_string()),
                ("client_connection_id", client_connection_id.to_string()),
                ("inserted", inserted.to_string()),
                ("swarm_enabled", swarm_enabled.to_string()),
                (
                    "swarm_id",
                    derived_swarm_id.unwrap_or_else(|| "none".to_string()),
                ),
            ],
        );

        inserted
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

    /// Tear down a session's swarm membership on `/clear`: remove the member
    /// from `members` and `swarms_by_id`, clear its file-touch tracking, and
    /// drop its channel subscriptions. Returns the member's identity so the
    /// caller can re-register the fresh replacement session with the same swarm
    /// intent. No-op (returns an empty identity) when no member was present.
    pub(crate) async fn take_session_membership(&self, client_session_id: &str) -> MemberIdentity {
        let identity = {
            let mut members = self.swarm_state.members.write().await;
            match members.remove(client_session_id) {
                Some(member) => MemberIdentity {
                    swarm_id: member.swarm_id,
                    swarm_enabled: member.swarm_enabled,
                    friendly_name: member.friendly_name,
                },
                None => MemberIdentity {
                    swarm_id: None,
                    swarm_enabled: false,
                    friendly_name: None,
                },
            }
        };
        if let Some(ref swarm_id) = identity.swarm_id {
            let mut swarms = self.swarm_state.swarms_by_id.write().await;
            if let Some(swarm) = swarms.get_mut(swarm_id) {
                swarm.remove(client_session_id);
                if swarm.is_empty() {
                    swarms.remove(swarm_id);
                }
            }
        }
        self.file_touch.clear_session(client_session_id).await;
        super::super::swarm_channels::remove_session_channel_subscriptions(
            client_session_id,
            &self.channel_subscriptions,
            &self.channel_subscriptions_by_session,
        )
        .await;
        identity
    }

    /// Update a member's status (and optional detail) through the swarm service,
    /// so session code does not reach into the raw membership map or the event
    /// sinks. Deferred to `swarm::update_member_status` which owns the
    /// coordinator-notification and event-fanout behavior.
    pub(crate) async fn set_member_status(
        &self,
        session_id: &str,
        status: &str,
        detail: Option<String>,
    ) {
        super::super::swarm::update_member_status(
            session_id,
            status,
            detail,
            &self.swarm_state.members,
            &self.swarm_state.swarms_by_id,
            Some(&self.event_history),
            Some(&self.event_counter),
            Some(&self.swarm_event_tx),
        )
        .await;
    }

    /// Remove a session from a swarm plan's participant set. Routes through the
    /// swarm service so session-lifecycle code does not reach into the raw plans
    /// map. Deferred to `swarm::remove_plan_participant`.
    pub(crate) async fn remove_plan_participant(&self, swarm_id: &str, session_id: &str) {
        super::super::swarm::remove_plan_participant(swarm_id, session_id, &self.swarm_state.plans)
            .await;
    }

    /// Rename a session in a swarm plan's participant set when its session id
    /// changes (resume under a new id). Deferred to `swarm::rename_plan_participant`.
    pub(crate) async fn rename_plan_participant(
        &self,
        swarm_id: &str,
        old_session_id: &str,
        new_session_id: &str,
    ) {
        super::super::swarm::rename_plan_participant(
            swarm_id,
            old_session_id,
            new_session_id,
            &self.swarm_state.plans,
        )
        .await;
    }

    /// Remove a session from a swarm: salvage its assignments, update
    /// `swarms_by_id`, re-elect/clean up the coordinator if it left, reparent
    /// its spawned children, persist the new membership, and broadcast status.
    /// Routes through the swarm service so teardown code does not hold the raw
    /// swarm maps. Deferred to `swarm::remove_session_from_swarm`.
    pub(crate) async fn remove_session_from_swarm(&self, session_id: &str, swarm_id: &str) {
        super::super::swarm::remove_session_from_swarm(
            session_id,
            swarm_id,
            &self.swarm_state.members,
            &self.swarm_state.swarms_by_id,
            &self.swarm_state.coordinators,
            &self.swarm_state.plans,
        )
        .await;
    }

    /// Record a swarm event into the ring buffer and broadcast it. Routes event
    /// emission through the swarm service so callers do not touch the event
    /// sinks directly. Deferred to `swarm::record_swarm_event`.
    pub(crate) async fn record_swarm_event(
        &self,
        session_id: String,
        session_name: Option<String>,
        swarm_id: Option<String>,
        event: SwarmEventType,
    ) {
        super::super::swarm::record_swarm_event(
            &self.event_history,
            &self.event_counter,
            &self.swarm_event_tx,
            session_id,
            session_name,
            swarm_id,
            event,
        )
        .await;
    }

    /// Remove a session's channel subscriptions from both indexes. Routes
    /// through the swarm service so teardown code does not touch the raw channel
    /// index. Deferred to `swarm_channels::remove_session_channel_subscriptions`.
    pub(crate) async fn remove_session_channel_subscriptions(&self, session_id: &str) {
        super::super::swarm_channels::remove_session_channel_subscriptions(
            session_id,
            &self.channel_subscriptions,
            &self.channel_subscriptions_by_session,
        )
        .await;
    }
}
