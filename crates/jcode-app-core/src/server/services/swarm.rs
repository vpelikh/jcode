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

/// Borrowed view of the swarm event emission sources handed to read callers:
/// the ring-buffer history, the event-id counter, and the broadcast sender.
type EventSources<'a> = (
    &'a Arc<RwLock<VecDeque<SwarmEvent>>>,
    &'a Arc<AtomicU64>,
    &'a broadcast::Sender<SwarmEvent>,
);

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
    swarm_state: SwarmState,
    /// Shared context by swarm (swarm_id -> key -> SharedContext).
    shared_context: Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
    /// File-touch tracking service (forward path index + reverse session index).
    file_touch: FileTouchService,
    /// Channel subscriptions forward index.
    channel_subscriptions: ChannelSubscriptions,
    /// Channel subscriptions reverse index (session_id -> swarm_id -> channels).
    channel_subscriptions_by_session: ChannelSubscriptions,
    /// Event history for real-time event subscription (ring buffer).
    event_history: Arc<RwLock<VecDeque<SwarmEvent>>>,
    /// Counter for event IDs.
    event_counter: Arc<AtomicU64>,
    /// Broadcast channel for swarm event subscriptions.
    swarm_event_tx: broadcast::Sender<SwarmEvent>,
    /// Persisted communicate await_members wait registry.
    await_members_runtime: AwaitMembersRuntime,
    /// Persisted dedupe registry for mutating swarm coordinator operations.
    swarm_mutation_runtime: SwarmMutationRuntime,
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

    /// Borrow the file-touch tracking service.
    ///
    /// Reads and writes both route through the encapsulated `FileTouchService`
    /// (`record_touch`, `snapshot`, `reverse_snapshot`, `clear_session`,
    /// `expire_older_than`, `sorted_file_strings_for_session`,
    /// `accesses_for_path`); callers do not reach into the raw index maps.
    /// Exists so the field stays encapsulated (Tier 3).
    pub(crate) fn file_touch(&self) -> &FileTouchService {
        &self.file_touch
    }

    /// Borrow the persisted dedupe registry for mutating swarm coordinator
    /// operations.
    ///
    /// Reads and writes route through the encapsulated `SwarmMutationRuntime`;
    /// callers do not reach into the raw registry maps. Exists so the field
    /// stays encapsulated (Tier 3).
    pub(crate) fn swarm_mutation_runtime(&self) -> &SwarmMutationRuntime {
        &self.swarm_mutation_runtime
    }

    /// Borrow the persisted communicate await_members wait registry.
    ///
    /// Reads and writes route through the encapsulated `AwaitMembersRuntime`;
    /// callers do not reach into the raw waiters/active-key maps. Callers that
    /// need an owned handle clone it via the accessor (the service is
    /// `Clone`, sharing the underlying `Arc`-backed maps). Exists so the field
    /// stays encapsulated (Tier 3).
    pub(crate) fn await_members_runtime(&self) -> &AwaitMembersRuntime {
        &self.await_members_runtime
    }

    /// Borrow the shared core swarm coordination state.
    ///
    /// The returned `SwarmState` holds the members / swarms_by_id / plans /
    /// coordinators maps as shared `Arc<RwLock<..>>` handles; callers read or
    /// write individual maps through those handles. Mutations to membership and
    /// roles should route through the behavior methods (`ensure_member`,
    /// `set_member_status`, `remove_session_from_swarm`, ...) where one exists;
    /// this accessor is for the read paths and the remaining coordination
    /// plumbing that needs the raw maps. Exists so the field stays encapsulated
    /// (Tier 3).
    pub(crate) fn swarm_state(&self) -> &SwarmState {
        &self.swarm_state
    }

    /// Borrow the swarm event emission sources (`history`, `counter`,
    /// `broadcast sender`). Reads only: mutations route through
    /// `record_swarm_event`. Exists so the private event-sink fields stay
    /// encapsulated (Tier 3).
    pub(crate) fn read_event_sources(&self) -> EventSources<'_> {
        (
            &self.event_history,
            &self.event_counter,
            &self.swarm_event_tx,
        )
    }

    /// Construct an otherwise-default handle with specific event sources.
    /// Test-only: lets `TestSwarmBuilder` seed the (now-private) event sinks
    /// without exposing them as mutable fields.
    #[cfg(test)]
    pub(crate) fn with_event_sources(
        mut self,
        event_history: Arc<RwLock<VecDeque<SwarmEvent>>>,
        event_counter: Arc<AtomicU64>,
        swarm_event_tx: broadcast::Sender<SwarmEvent>,
    ) -> Self {
        self.event_history = event_history;
        self.event_counter = event_counter;
        self.swarm_event_tx = swarm_event_tx;
        self
    }

    /// Build an all-default handle for tests. Test-only: provides a
    /// constructor for the (now-private) fields while callers configure them
    /// through `with_event_sources` / `with_swarm_state`.
    #[cfg(test)]
    pub(crate) fn test_with_state(
        swarm_state: SwarmState,
        shared_context: Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
        channel_subscriptions: ChannelSubscriptions,
        channel_subscriptions_by_session: ChannelSubscriptions,
        swarm_mutation_runtime: SwarmMutationRuntime,
    ) -> Self {
        Self {
            swarm_state,
            shared_context,
            file_touch: FileTouchService::new(),
            channel_subscriptions,
            channel_subscriptions_by_session,
            event_history: Arc::new(RwLock::new(VecDeque::new())),
            event_counter: Arc::new(AtomicU64::new(0)),
            swarm_event_tx: broadcast::channel(16).0,
            await_members_runtime: AwaitMembersRuntime::default(),
            swarm_mutation_runtime,
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

    /// Update a member's status including a completion report, routing through
    /// the swarm service. Deferred to
    /// `swarm::update_member_status_with_report`.
    pub(crate) async fn set_member_status_with_report(
        &self,
        session_id: &str,
        status: &str,
        detail: Option<String>,
        completion_report: Option<String>,
    ) {
        super::super::swarm::update_member_status_with_report(
            session_id,
            status,
            detail,
            completion_report,
            &self.swarm_state.members,
            &self.swarm_state.swarms_by_id,
            Some(&self.event_history),
            Some(&self.event_counter),
            Some(&self.swarm_event_tx),
        )
        .await;
    }

    /// Update a member's status including a completion report and a tldr,
    /// routing through the swarm service. Deferred to
    /// `swarm::update_member_status_with_report_tldr`.
    pub(crate) async fn set_member_status_with_report_tldr(
        &self,
        session_id: &str,
        status: &str,
        detail: Option<String>,
        completion_report: Option<String>,
        report_tldr: Option<String>,
    ) {
        super::super::swarm::update_member_status_with_report_tldr(
            session_id,
            status,
            detail,
            completion_report,
            report_tldr,
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

    /// Upsert a shared-context entry for `swarm_id` / `key`. `append` toggles
    /// the append semantics of `comm_context:write` (a trailing line joined to
    /// the existing value). Preserves the original `created_at` on refresh.
    /// Routes the shared-context map mutation through the swarm service so
    /// callers do not touch the raw map (Tier 3).
    pub(crate) async fn set_shared_context(
        &self,
        swarm_id: &str,
        key: &str,
        value: String,
        from_session: &str,
        from_name: Option<String>,
        append: bool,
    ) {
        let mut shared_ctx = self.shared_context.write().await;
        let swarm_ctx = shared_ctx.entry(swarm_id.to_string()).or_default();
        let now = Instant::now();
        let created_at = swarm_ctx.get(key).map(|c| c.created_at).unwrap_or(now);
        let stored_value = if append {
            swarm_ctx
                .get(key)
                .map(|existing| {
                    if existing.value.is_empty() {
                        value.clone()
                    } else {
                        format!("{}\n{}", existing.value, value)
                    }
                })
                .unwrap_or_else(|| value.clone())
        } else {
            value.clone()
        };
        swarm_ctx.insert(
            key.to_string(),
            SharedContext {
                key: key.to_string(),
                value: stored_value.clone(),
                from_session: from_session.to_string(),
                from_name,
                created_at,
                updated_at: now,
            },
        );
    }

    /// Read a single shared-context entry for `swarm_id` / `key`, if present.
    pub(crate) async fn get_shared_context(
        &self,
        swarm_id: &str,
        key: &str,
    ) -> Option<SharedContext> {
        self.shared_context
            .read()
            .await
            .get(swarm_id)
            .and_then(|swarm_ctx| swarm_ctx.get(key))
            .cloned()
    }

    /// Read a snapshot of all shared-context entries for `swarm_id`.
    pub(crate) async fn shared_context_entries(&self, swarm_id: &str) -> Vec<SharedContext> {
        self.shared_context
            .read()
            .await
            .get(swarm_id)
            .map(|swarm_ctx| swarm_ctx.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Borrow the whole shared-context map for read-only snapshot consumers
    /// (debug `swarm:context` / `server_state` observation paths). Callers
    /// must not mutate through this handle; all writes route through
    /// `set_shared_context` / `remove_shared_context`.
    pub(crate) fn shared_context_map(
        &self,
    ) -> &Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>> {
        &self.shared_context
    }

    /// Remove a single shared-context entry for `swarm_id` / `key`.
    pub(crate) async fn remove_shared_context(&self, swarm_id: &str, key: &str) {
        if let Some(swarm_ctx) = self.shared_context.write().await.get_mut(swarm_id) {
            swarm_ctx.remove(key);
        }
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

    /// Subscribe a session to a channel within a swarm. Routes the channel
    /// index updates through the swarm service (Tier 3).
    pub(crate) async fn subscribe_session_to_channel(
        &self,
        session_id: &str,
        swarm_id: &str,
        channel: &str,
    ) {
        super::super::swarm_channels::subscribe_session_to_channel(
            session_id,
            swarm_id,
            channel,
            &self.channel_subscriptions,
            &self.channel_subscriptions_by_session,
        )
        .await;
    }

    /// Unsubscribe a session from a channel within a swarm. Routes the channel
    /// index updates through the swarm service (Tier 3).
    pub(crate) async fn unsubscribe_session_from_channel(
        &self,
        session_id: &str,
        swarm_id: &str,
        channel: &str,
    ) {
        super::super::swarm_channels::unsubscribe_session_from_channel(
            session_id,
            swarm_id,
            channel,
            &self.channel_subscriptions,
            &self.channel_subscriptions_by_session,
        )
        .await;
    }

    /// Borrow the channel-subscription forward index for read-only consumers
    /// (channel list / member resolution, debug snapshots). Callers must not
    /// mutate through this handle; all writes route through
    /// `subscribe_session_to_channel` / `unsubscribe_session_from_channel` /
    /// `remove_session_channel_subscriptions`.
    pub(crate) fn channel_subscriptions_map(&self) -> &ChannelSubscriptions {
        &self.channel_subscriptions
    }

    /// Borrow the channel-subscription reverse index (session_id -> swarm_id ->
    /// channels) for read-only consumers. Callers must not mutate through this
    /// handle; all writes route through the subscribe/unsubscribe/remove
    /// methods.
    pub(crate) fn channel_subscriptions_by_session_map(&self) -> &ChannelSubscriptions {
        &self.channel_subscriptions_by_session
    }

    /// Rebroadcast the current membership of a swarm to its channel
    /// subscribers/builds. Routes through the swarm service so callers do not
    /// reach into the raw membership and swarm maps. Deferred to
    /// `swarm::broadcast_swarm_status`.
    pub(crate) async fn broadcast_swarm_status(&self, swarm_id: &str) {
        super::super::swarm::broadcast_swarm_status(
            swarm_id,
            &self.swarm_state.members,
            &self.swarm_state.swarms_by_id,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_handle() -> SwarmServiceHandle {
        SwarmServiceHandle::test_with_state(
            SwarmState {
                members: Arc::new(RwLock::new(HashMap::new())),
                swarms_by_id: Arc::new(RwLock::new(HashMap::new())),
                plans: Arc::new(RwLock::new(HashMap::new())),
                coordinators: Arc::new(RwLock::new(HashMap::new())),
            },
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(HashMap::new())),
            SwarmMutationRuntime::default(),
        )
    }

    #[tokio::test]
    async fn shared_context_upsert_and_preserve_created_at() {
        let handle = base_handle();
        handle
            .set_shared_context("swarm-1", "k", "v1".to_string(), "sess", None, false)
            .await;

        let first = handle.get_shared_context("swarm-1", "k").await;
        let created = first.as_ref().expect("entry").created_at;
        assert_eq!(first.as_ref().expect("entry").value, "v1");

        // Re-insert preserves the original created_at (Tier 3 unification).
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        handle
            .set_shared_context("swarm-1", "k", "v2".to_string(), "sess", None, false)
            .await;
        let second = handle.get_shared_context("swarm-1", "k").await;
        assert_eq!(second.as_ref().expect("entry").value, "v2");
        assert_eq!(
            second.as_ref().expect("entry").created_at, created,
            "created_at should survive a plain re-insert"
        );
    }

    #[tokio::test]
    async fn shared_context_append_semantics() {
        let handle = base_handle();
        handle
            .set_shared_context("swarm-1", "k", "head".to_string(), "sess", None, false)
            .await;

        // Append joins with a newline.
        handle
            .set_shared_context("swarm-1", "k", "tail".to_string(), "sess", None, true)
            .await;
        let entry = handle.get_shared_context("swarm-1", "k").await;
        assert_eq!(entry.as_ref().expect("entry").value, "head\ntail");

        // Append onto an empty existing value uses the new value directly.
        handle
            .set_shared_context("swarm-1", "empty", "".to_string(), "sess", None, false)
            .await;
        handle
            .set_shared_context("swarm-1", "empty", "solo".to_string(), "sess", None, true)
            .await;
        assert_eq!(
            handle.get_shared_context("swarm-1", "empty").await.unwrap().value,
            "solo"
        );
    }

    #[tokio::test]
    async fn shared_context_remove_and_entries() {
        let handle = base_handle();
        handle
            .set_shared_context("swarm-1", "a", "1".to_string(), "sess", None, false)
            .await;
        handle
            .set_shared_context("swarm-1", "b", "2".to_string(), "sess", None, false)
            .await;

        let mut entries = handle.shared_context_entries("swarm-1").await;
        let mut keys: Vec<String> = entries.drain(..).map(|e| e.key).collect();
        keys.sort();
        assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);

        handle.remove_shared_context("swarm-1", "a").await;
        assert!(handle.get_shared_context("swarm-1", "a").await.is_none());
        assert!(handle.get_shared_context("swarm-1", "b").await.is_some());

        // Removing from a swarm with no context map is a no-op.
        handle.remove_shared_context("swarm-nope", "a").await;
    }

    #[tokio::test]
    async fn subscribe_unsubscribe_channel_updates_both_indexes() {
        let handle = base_handle();
        handle
            .subscribe_session_to_channel("sess-1", "swarm-1", "chan-a")
            .await;

        // Forward index: swarm-1 -> chan-a -> {sess-1}
        let fwd = handle.channel_subscriptions_map().read().await;
        assert!(
            fwd.get("swarm-1")
                .and_then(|ch| ch.get("chan-a"))
                .is_some_and(|s| s.contains("sess-1"))
        );
        // Reverse index: sess-1 -> swarm-1 -> {chan-a}
        let rev = handle
            .channel_subscriptions_by_session_map()
            .read()
            .await;
        assert!(
            rev.get("sess-1")
                .and_then(|sw| sw.get("swarm-1"))
                .is_some_and(|ch| ch.contains("chan-a"))
        );
        drop(fwd);
        drop(rev);

        handle
            .unsubscribe_session_from_channel("sess-1", "swarm-1", "chan-a")
            .await;
        let fwd = handle.channel_subscriptions_map().read().await;
        assert!(
            fwd.get("swarm-1")
                .and_then(|ch| ch.get("chan-a"))
                .is_none_or(|s| !s.contains("sess-1"))
        );
    }

    #[tokio::test]
    async fn event_sources_expose_the_seeded_sinks() {
        let history = Arc::new(RwLock::new(VecDeque::from([SwarmEvent {
            id: 1,
            session_id: "s".to_string(),
            session_name: None,
            swarm_id: Some("sw".to_string()),
            event: SwarmEventType::MemberChange {
                action: "joined".to_string(),
            },
            timestamp: std::time::Instant::now(),
            absolute_time: std::time::SystemTime::now(),
        }])));
        let counter = Arc::new(AtomicU64::new(7));
        let (tx, _rx) = broadcast::channel(16);
        let handle = base_handle().with_event_sources(history, counter, tx.clone());

        let (h, c, t) = handle.read_event_sources();
        assert_eq!(h.read().await.len(), 1);
        assert_eq!(c.load(std::sync::atomic::Ordering::SeqCst), 7);
        assert!(
            t.same_channel(&tx),
            "read_event_sources should expose the seeded broadcast sender"
        );
    }

    // Smoke: a default handle builds and mutations on it are inert, not panics.
    #[tokio::test]
    async fn default_handle_accepts_method_calls() {
        let handle = SwarmServiceHandle::test_with_state(
            SwarmState {
                members: Arc::new(RwLock::new(HashMap::new())),
                swarms_by_id: Arc::new(RwLock::new(HashMap::new())),
                plans: Arc::new(RwLock::new(HashMap::new())),
                coordinators: Arc::new(RwLock::new(HashMap::new())),
            },
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(HashMap::new())),
            SwarmMutationRuntime::default(),
        );
        handle.remove_shared_context("swarm-1", "k").await;
        handle
            .unsubscribe_session_from_channel("s", "sw", "c")
            .await;
        let _ = handle.read_event_sources();
    }
}
