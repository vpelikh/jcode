//! Swarm service handle.

use crate::protocol::{NotificationType, ServerEvent};
use crate::server::{
    AwaitMembersRuntime, FileTouchService, Server, SharedContext, SwarmEvent, SwarmEventType,
    SwarmMember, SwarmMutationRuntime, SwarmState, VersionedPlan,
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

    /// Register a headless session as a swarm member that is always scored
    /// `is_headless: true` (never auto-claims coordinator) and carries its own
    /// runtime describe metadata, then add it to `swarms_by_id`. Distinct from
    /// `ensure_member`, which serves connection-backed TUI publishers
    /// (`is_headless: false`, per-connection `event_txs`). Routes the member
    /// insert + swarm-membership insert through the handle so the caller does
    /// not reach into either raw map.
    ///
    /// `swarm_id` is honored only when `swarm_enabled` is true: a disabled
    /// member never carries a swarm id or a `swarms_by_id` entry, regardless of
    /// what the caller passes. This keeps the member consistent even if a caller
    /// links `Some(swarm_id)` with `swarm_enabled: false`.
    #[expect(
        clippy::too_many_arguments,
        reason = "registering a headless member carries session identity, joined swarm id, and its runtime descriptor"
    )]
    pub(crate) async fn register_headless_member(
        &self,
        session_id: &str,
        working_dir: Option<PathBuf>,
        swarm_id: Option<&str>,
        swarm_enabled: bool,
        friendly_name: String,
        report_back_to_session_id: Option<String>,
        event_tx: mpsc::UnboundedSender<ServerEvent>,
        runtime: crate::protocol::SwarmMemberRuntime,
    ) {
        let now = Instant::now();
        // A non-swarm-enabled member must not be recorded under a swarm id. The
        // caller's `swarm_id` is derived from `swarm_enabled`, but enforce the
        // invariant here so a disabled member is never inconsistently tagged.
        let swarm_id = if swarm_enabled { swarm_id } else { None };
        {
            let mut members = self.swarm_state.members.write().await;
            members.insert(
                session_id.to_string(),
                SwarmMember {
                    session_id: session_id.to_string(),
                    event_tx: event_tx.clone(),
                    event_txs: HashMap::new(),
                    working_dir,
                    swarm_id: swarm_id.map(|id| id.to_string()),
                    swarm_enabled,
                    status: "ready".to_string(),
                    detail: None,
                    task_label: None,
                    friendly_name: Some(friendly_name),
                    report_back_to_session_id,
                    latest_completion_report: None,
                    role: "agent".to_string(),
                    joined_at: now,
                    last_status_change: now,
                    is_headless: true,
                    output_tail: None,
                    todo_progress: None,
                    todo_items: Vec::new(),
                    runtime,
                },
            );
        }

        if let Some(id) = swarm_id {
            let mut swarms = self.swarm_state.swarms_by_id.write().await;
            swarms
                .entry(id.to_string())
                .or_insert_with(HashSet::new)
                .insert(session_id.to_string());
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

    /// Resolve the swarm id a member currently belongs to, if any.
    pub(crate) async fn member_swarm_id(&self, session_id: &str) -> Option<String> {
        let members = self.swarm_state.members.read().await;
        members
            .get(session_id)
            .and_then(|member| member.swarm_id.clone())
    }

    /// Resolve the swarm ids of two members in one read.
    ///
    /// Returns `(req_swarm, target_swarm)`. This is used by the
    /// same-swarm-access guard in the comm read helpers.
    pub(crate) async fn member_swarm_ids(
        &self,
        req_session_id: &str,
        target_session: &str,
    ) -> (Option<String>, Option<String>) {
        let members = self.swarm_state.members.read().await;
        (
            members
                .get(req_session_id)
                .and_then(|member| member.swarm_id.clone()),
            members
                .get(target_session)
                .and_then(|member| member.swarm_id.clone()),
        )
    }

    /// Whether `req_session_id` may read the full context of `target_session`:
    /// always true for the session itself, otherwise requires `req_session_id`
    /// to be a swarm coordinator. Mirrors the swarm-aware context permission
    /// guard used by the comm read helpers.
    pub(crate) async fn can_read_full_context(
        &self,
        req_session_id: &str,
        target_session: &str,
    ) -> bool {
        if req_session_id == target_session {
            return true;
        }
        let members = self.swarm_state.members.read().await;
        members
            .get(req_session_id)
            .map(|member| member.role == "coordinator")
            .unwrap_or(false)
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

        // A coordinator slot may also be held by the renamed session. Update
        // it so resume does not leave the old id pointing at a vanished
        // session. Held separately from the member/swarm locks above so no two
        // swarm maps are ever write-locked together (deadlock avoidance).
        let mut coordinators = self.swarm_state.coordinators.write().await;
        for coordinator in coordinators.values_mut() {
            if *coordinator == old_session_id {
                *coordinator = new_session_id.to_string();
            }
        }
    }

    /// Register a freshly spawned *visible* (client-attached) swarm member as
    /// one whole transaction: insert the member record, add it to `swarms_by_id`,
    /// record a `MemberChange { action: "joined" }` event, and broadcast the
    /// refreshed swarm status. The handle owns all four swarm-maps writes so
    /// comm_session's visible-spawn path never touches the raw maps (Tier 3).
    ///
    /// The sibling `register_headless_member` covers headless spawns; this one
    /// preserves the visible-spawn status/detail/role semantics requested by the
    /// caller, and hands the new member a fresh event channel.
    pub(crate) async fn register_visible_member(
        &self,
        session_id: &str,
        swarm_id: &str,
        working_dir: Option<PathBuf>,
        has_startup_message: bool,
        report_back_to_session_id: Option<&str>,
    ) {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let now = Instant::now();
        let friendly_name = crate::id::extract_session_name(session_id)
            .map(|name| name.to_string())
            .unwrap_or_else(|| session_id.to_string());
        let (status, detail) = if has_startup_message {
            ("running".to_string(), Some("startup queued".to_string()))
        } else {
            ("spawned".to_string(), Some("launching client".to_string()))
        };

        {
            let mut members = self.swarm_state.members.write().await;
            members.insert(
                session_id.to_string(),
                SwarmMember {
                    session_id: session_id.to_string(),
                    event_tx,
                    event_txs: HashMap::new(),
                    working_dir: working_dir.map(PathBuf::from),
                    swarm_id: Some(swarm_id.to_string()),
                    swarm_enabled: true,
                    status,
                    detail,
                    task_label: None,
                    friendly_name: Some(friendly_name),
                    report_back_to_session_id: report_back_to_session_id.map(str::to_string),
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
        }

        {
            let mut swarms = self.swarm_state.swarms_by_id.write().await;
            swarms
                .entry(swarm_id.to_string())
                .or_insert_with(HashSet::new)
                .insert(session_id.to_string());
        }

        super::super::swarm::record_swarm_event_for_session(
            session_id,
            SwarmEventType::MemberChange {
                action: "joined".to_string(),
            },
            &self.swarm_state.members,
            &self.event_history,
            &self.event_counter,
            &self.swarm_event_tx,
        )
        .await;
        super::super::swarm::broadcast_swarm_status(
            swarm_id,
            &self.swarm_state.members,
            &self.swarm_state.swarms_by_id,
        )
        .await;
    }

    /// Add the spawner and a freshly spawned worker to the swarm plan's
    /// participant set (when the plan is non-empty) as one whole transaction,
    /// then broadcast the refreshed plan. Mirrors the spawn path's participant
    /// bookkeeping so comm_session never touches the raw plans map (Tier 3).
    pub(crate) async fn spawn_adds_plan_participants(
        &self,
        swarm_id: &str,
        spawner_session_id: &str,
        spawned_session_id: &str,
    ) {
        {
            let mut plans = self.swarm_state.plans.write().await;
            if let Some(plan) = plans.get_mut(swarm_id)
                && (!plan.items.is_empty() || !plan.participants.is_empty())
            {
                plan.participants.insert(spawner_session_id.to_string());
                plan.participants.insert(spawned_session_id.to_string());
            }
        }
        super::super::swarm::broadcast_swarm_plan(
            swarm_id,
            Some("participant_spawned".to_string()),
            &self.swarm_state.plans,
            &self.swarm_state.members,
            &self.swarm_state.swarms_by_id,
        )
        .await;
    }

    /// Promote a spawn root session to the swarm coordinator slot when (and only
    /// when) the slot is empty or stale, as one whole transaction: claim the
    /// `coordinators` slot, flip the member role, persist, and broadcast the
    /// refreshed swarm status. Returns `true` when this call performed the
    /// promotion (so the caller can show the "You are the coordinator" notice).
    /// The caller precomputes `coordinator_is_stale` from its own read pass.
    pub(crate) async fn promote_spawn_coordinator(
        &self,
        swarm_id: &str,
        session_id: &str,
        coordinator_is_stale: bool,
    ) -> bool {
        let promoted = {
            let mut coordinators = self.swarm_state.coordinators.write().await;
            match coordinators.get(swarm_id) {
                Some(existing) if existing == session_id => false,
                Some(_) if !coordinator_is_stale => false,
                _ => {
                    coordinators.insert(swarm_id.to_string(), session_id.to_string());
                    true
                }
            }
        };

        if promoted {
            {
                let mut members = self.swarm_state.members.write().await;
                if let Some(member) = members.get_mut(session_id) {
                    member.role = "coordinator".to_string();
                }
            }
            let swarm_state = SwarmState {
                members: Arc::clone(&self.swarm_state.members),
                swarms_by_id: Arc::clone(&self.swarm_state.swarms_by_id),
                plans: Arc::clone(&self.swarm_state.plans),
                coordinators: Arc::clone(&self.swarm_state.coordinators),
            };
            super::super::persist_swarm_state_for(swarm_id, &swarm_state).await;
            super::super::swarm::broadcast_swarm_status(
                swarm_id,
                &self.swarm_state.members,
                &self.swarm_state.swarms_by_id,
            )
            .await;
        }
        promoted
    }

    /// Re-key a session's swarm membership to match a freshly subscribed
    /// working directory, as one whole transaction. This is the subscribe /
    /// resume orchestration hotspot the encapsulation plan flags as a "risk
    /// concentration": it updates the member's `working_dir`/`swarm_id`,
    /// removes the member from the old `swarms_by_id` set, adds it to the new
    /// one, resets the member role, re-elects a new coordinator when the member
    /// WAS the old swarm's coordinator, drops the plan participant from the old
    /// swarm, persists, and broadcasts swarm status for both swarms (plus a
    /// "You are now the coordinator" notice). All swarm-map mutation happens
    /// here so `client_session`'s subscribe path never touches the raw maps
    /// (Tier 3).
    ///
    /// Returns `(old_swarm_id, updated_swarm_id)` so the caller can log the
    /// transition. `inserted_swarm_member` mirrors the caller's flag: a freshly
    /// registered root receives the new session-scoped identity, while an
    /// existing (reconnect/restored) member keeps its persisted swarm id.
    pub(crate) async fn move_member_between_swarms(
        &self,
        client_session_id: &str,
        working_dir: PathBuf,
        inserted_swarm_member: bool,
    ) -> (Option<String>, Option<String>) {
        let mut old_swarm_id: Option<String> = None;
        let mut updated_swarm_id: Option<String> = None;
        {
            let mut members = self.swarm_state.members.write().await;
            if let Some(member) = members.get_mut(client_session_id) {
                old_swarm_id = member.swarm_id.clone();
                // Existing members include reconnects and daemon-restored
                // sessions. Keep their persisted swarm id so an intentional
                // resume retains its workers and plan. Only a newly inserted
                // root receives the new session-scoped identity.
                let new_swarm_id = if inserted_swarm_member {
                    crate::server::util::swarm_id_for_session(client_session_id)
                } else {
                    member
                        .swarm_id
                        .clone()
                        .or_else(|| crate::server::util::swarm_id_for_session(client_session_id))
                };
                member.working_dir = Some(working_dir);
                member.swarm_id = if member.swarm_enabled {
                    new_swarm_id.clone()
                } else {
                    None
                };
                updated_swarm_id = member.swarm_id.clone();
            }
        }

        if let Some(ref old_id) = old_swarm_id {
            if updated_swarm_id.as_ref() != Some(old_id) {
                self.remove_session_channel_subscriptions(client_session_id).await;
            }
            let mut swarms = self.swarm_state.swarms_by_id.write().await;
            if let Some(swarm) = swarms.get_mut(old_id) {
                swarm.remove(client_session_id);
                if swarm.is_empty() {
                    swarms.remove(old_id);
                }
            }
        }

        if let Some(ref new_id) = updated_swarm_id {
            let mut swarms = self.swarm_state.swarms_by_id.write().await;
            swarms
                .entry(new_id.clone())
                .or_insert_with(HashSet::new)
                .insert(client_session_id.to_string());
        }

        if updated_swarm_id != old_swarm_id {
            let mut members = self.swarm_state.members.write().await;
            if let Some(member) = members.get_mut(client_session_id) {
                member.role = "agent".to_string();
            }
        }

        if let Some(old_id) = old_swarm_id.clone() {
            let was_coordinator = self
                .swarm_state
                .coordinators
                .read()
                .await
                .get(&old_id)
                .map(|session_id| session_id == client_session_id)
                .unwrap_or(false);
            if was_coordinator {
                let new_coordinator: Option<String> = {
                    let swarms = self.swarm_state.swarms_by_id.read().await;
                    swarms
                        .get(&old_id)
                        .and_then(|swarm| swarm.iter().min().cloned())
                };
                {
                    let mut coordinators = self.swarm_state.coordinators.write().await;
                    coordinators.remove(&old_id);
                    if let Some(new_id) = &new_coordinator {
                        coordinators.insert(old_id.clone(), new_id.clone());
                    }
                }
                if let Some(new_id) = new_coordinator
                    && let Some(member) = self.swarm_state.members.read().await.get(&new_id)
                {
                    let _ = member.event_tx.send(ServerEvent::Notification {
                        from_session: new_id.clone(),
                        from_name: member.friendly_name.clone(),
                        notification_type: NotificationType::Message {
                            scope: Some("swarm".to_string()),
                            channel: None,
                            tldr: None,
                        },
                        message: "You are now the coordinator for this swarm.".to_string(),
                    });
                }
            }
        }

        if let Some(old_id) = old_swarm_id.clone() {
            if updated_swarm_id.as_ref() != Some(&old_id) {
                self.remove_plan_participant(&old_id, client_session_id).await;
                let swarm_state = SwarmState {
                    members: Arc::clone(&self.swarm_state.members),
                    swarms_by_id: Arc::clone(&self.swarm_state.swarms_by_id),
                    plans: Arc::clone(&self.swarm_state.plans),
                    coordinators: Arc::clone(&self.swarm_state.coordinators),
                };
                super::super::persist_swarm_state_for(&old_id, &swarm_state).await;
            }
            super::super::swarm::broadcast_swarm_status(
                &old_id,
                &self.swarm_state.members,
                &self.swarm_state.swarms_by_id,
            )
            .await;
        }
        if let Some(new_id) = updated_swarm_id.clone()
            && old_swarm_id.as_ref() != Some(&new_id)
        {
            super::super::swarm::broadcast_swarm_status(
                &new_id,
                &self.swarm_state.members,
                &self.swarm_state.swarms_by_id,
            )
            .await;
        }

        (old_swarm_id, updated_swarm_id)
    }

    /// Assign a target session a role within a swarm as one whole transaction:
    /// flip the member's role, promote it to the coordinator slot when the role
    /// is `coordinator` (demoting the prior requester), persist, broadcast the
    /// refreshed swarm status, and record a `role_assignment` notification
    /// event. Returns `Err` with a user-facing message when the target session
    /// is not a member. The durable mutation dedup (begin/finish) is owned by
    /// the caller; this method routes the whole map mutation so comm_control
    /// never touches the raw members/coordinators maps (Tier 3).
    pub(crate) async fn assign_member_role(
        &self,
        swarm_id: &str,
        target_session: &str,
        role: &str,
        req_session_id: &str,
    ) -> Result<(), String> {
        {
            let mut members = self.swarm_state.members.write().await;
            if let Some(member) = members.get_mut(target_session) {
                member.role = role.to_string();
            } else {
                return Err(format!("Unknown session '{target_session}'"));
            }
        }

        if role == "coordinator" {
            {
                let mut coordinators = self.swarm_state.coordinators.write().await;
                coordinators.insert(swarm_id.to_string(), target_session.to_string());
            }
            let mut members = self.swarm_state.members.write().await;
            if let Some(member) = members.get_mut(req_session_id)
                && member.session_id != target_session
            {
                member.role = "agent".to_string();
            }
        }

        let swarm_state = SwarmState {
            members: Arc::clone(&self.swarm_state.members),
            swarms_by_id: Arc::clone(&self.swarm_state.swarms_by_id),
            plans: Arc::clone(&self.swarm_state.plans),
            coordinators: Arc::clone(&self.swarm_state.coordinators),
        };
        super::super::persist_swarm_state_for(swarm_id, &swarm_state).await;
        super::super::swarm::broadcast_swarm_status(
            swarm_id,
            &self.swarm_state.members,
            &self.swarm_state.swarms_by_id,
        )
        .await;
        self.record_swarm_event(
            req_session_id.to_string(),
            None,
            Some(swarm_id.to_string()),
            SwarmEventType::Notification {
                notification_type: "role_assignment".to_string(),
                message: format!("{} -> {}", target_session, role),
            },
        )
        .await;
        Ok(())
    }

    /// Record a task assignment in a swarm plan as one whole transaction. Owns
    /// the raw `plans` write lock, runs the caller's synchronous assignment
    /// mutation (which _must_ not touch any other swarm map), and when a task
    /// was actually selected commits the plan: persist, broadcast
    /// `task_assigned`, and record a `PlanUpdate` event. Returns the assignment
    /// fan-out tuple `(selected_task_id, content, participant_ids, item_count,
    /// blocked_reason)` so the caller can notify the assignee. When no task was
    /// selected the plan is left uncommitted and the caller handles the
    /// `blocked_reason` ourselves. This keeps comm_control's assign path
    /// single-homed on the handle (Tier 3).
    pub(crate) async fn record_task_assignment<F>(
        &self,
        swarm_id: &str,
        req_session_id: &str,
        transform: F,
    ) -> (
        Option<String>,
        Option<String>,
        HashSet<String>,
        usize,
        Option<String>,
    )
    where
        F: FnOnce(&mut crate::plan::VersionedPlan) -> (
            Option<String>,
            Option<String>,
            HashSet<String>,
            usize,
            Option<String>,
        ),
    {
        let (selected_task_id, content, participant_ids, item_count, blocked_reason) = {
            let mut plans = self.swarm_state.plans.write().await;
            let plan = plans
                .entry(swarm_id.to_string())
                .or_insert_with(crate::plan::VersionedPlan::new);
            transform(plan)
        };

        if selected_task_id.is_some() {
            let swarm_state = SwarmState {
                members: Arc::clone(&self.swarm_state.members),
                swarms_by_id: Arc::clone(&self.swarm_state.swarms_by_id),
                plans: Arc::clone(&self.swarm_state.plans),
                coordinators: Arc::clone(&self.swarm_state.coordinators),
            };
            super::super::persist_swarm_state_for(swarm_id, &swarm_state).await;
            super::super::swarm::broadcast_swarm_plan(
                swarm_id,
                Some("task_assigned".to_string()),
                &self.swarm_state.plans,
                &self.swarm_state.members,
                &self.swarm_state.swarms_by_id,
            )
            .await;
            self.record_swarm_event(
                req_session_id.to_string(),
                None,
                Some(swarm_id.to_string()),
                SwarmEventType::PlanUpdate {
                    swarm_id: swarm_id.to_string(),
                    item_count,
                },
            )
            .await;
        }

        (
            selected_task_id,
            content,
            participant_ids,
            item_count,
            blocked_reason,
        )
    }

    /// Requeue an existing task assignment in a swarm plan as one whole
    /// transaction: take the raw `plans` write lock, run the caller's
    /// synchronous requeue mutation, and when a task was actually requeued
    /// commit the plan (persist + broadcast `task_<reason>`). Used by
    /// task_control's salvage/resume path so comm_control never touches the raw
    /// plans map (Tier 3). Returns whether a task was requeued.
    pub(crate) async fn requeue_assignment<F>(
        &self,
        swarm_id: &str,
        reason: &str,
        transform: F,
    ) -> bool
    where
        F: FnOnce(&mut crate::plan::VersionedPlan) -> Option<()>,
    {
        let requeued = {
            let mut plans = self.swarm_state.plans.write().await;
            let Some(plan) = plans.get_mut(swarm_id) else {
                return false;
            };
            transform(plan).is_some()
        };

        if requeued {
            let swarm_state = SwarmState {
                members: Arc::clone(&self.swarm_state.members),
                swarms_by_id: Arc::clone(&self.swarm_state.swarms_by_id),
                plans: Arc::clone(&self.swarm_state.plans),
                coordinators: Arc::clone(&self.swarm_state.coordinators),
            };
            super::super::persist_swarm_state_for(swarm_id, &swarm_state).await;
            super::super::swarm::broadcast_swarm_plan(
                swarm_id,
                Some(reason.to_string()),
                &self.swarm_state.plans,
                &self.swarm_state.members,
                &self.swarm_state.swarms_by_id,
            )
            .await;
        }
        requeued
    }

    /// Reclaim a stranded task assignment (dead assignee) for re-dispatch as
    /// one whole transaction: own the raw `plans` write lock and run the
    /// caller's synchronous reclaim closure, returning the reclaimed task id
    /// (or `None`). Commits are left to the caller's subsequent assignment so
    /// the reclaimed run persists/broadcasts as part of that transaction.
    pub(crate) async fn reclaim_stranded_task<F>(
        &self,
        swarm_id: &str,
        transform: F,
    ) -> Option<String>
    where
        F: FnOnce(&mut crate::plan::VersionedPlan) -> Option<String>,
    {
        let mut plans = self.swarm_state.plans.write().await;
        plans.get_mut(swarm_id).and_then(transform)
    }

    /// Transition a task's status/progress in a swarm plan as one whole
    /// transaction: own the raw `plans` write lock, run the caller's
    /// synchronous status mutation, then commit (persist + broadcast
    /// `task_<reason>`). Used by the background task-run lifecycle in
    /// comm_control (running / turn-end disposition / failed transitions) so
    /// those spawned flows never touch the raw plans map (Tier 3).
    pub(crate) async fn mutate_task_status<F>(&self, swarm_id: &str, reason: &str, transform: F)
    where
        F: FnOnce(&mut crate::plan::VersionedPlan),
    {
        {
            let mut plans = self.swarm_state.plans.write().await;
            if let Some(plan) = plans.get_mut(swarm_id) {
                transform(plan);
            }
        }
        let swarm_state = SwarmState {
            members: Arc::clone(&self.swarm_state.members),
            swarms_by_id: Arc::clone(&self.swarm_state.swarms_by_id),
            plans: Arc::clone(&self.swarm_state.plans),
            coordinators: Arc::clone(&self.swarm_state.coordinators),
        };
        super::super::persist_swarm_state_for(swarm_id, &swarm_state).await;
        super::super::swarm::broadcast_swarm_plan(
            swarm_id,
            Some(reason.to_string()),
            &self.swarm_state.plans,
            &self.swarm_state.members,
            &self.swarm_state.swarms_by_id,
        )
        .await;
    }

    /// Apply a turn-end task disposition in a swarm plan as one whole
    /// transaction. Owns the raw `plans` write lock and the commit (persist +
    /// a *with-previous* plan broadcast) so comm_control's background
    /// task-turn handler never touches the raw plans map (Tier 3). The closure
    /// runs the disposition logic and returns the broadcast reason; the
    /// pre-write `previous_items` snapshot is passed through to the diff-aware
    /// broadcast.
    pub(crate) async fn mutate_task_disposition<F>(
        &self,
        swarm_id: &str,
        previous_items: Option<&[crate::plan::PlanItem]>,
        transform: F,
    ) where
        F: FnOnce(&mut crate::plan::VersionedPlan) -> &'static str,
    {
        let reason = {
            let mut plans = self.swarm_state.plans.write().await;
            let Some(plan) = plans.get_mut(swarm_id) else {
                return;
            };
            transform(plan)
        };
        let swarm_state = SwarmState {
            members: Arc::clone(&self.swarm_state.members),
            swarms_by_id: Arc::clone(&self.swarm_state.swarms_by_id),
            plans: Arc::clone(&self.swarm_state.plans),
            coordinators: Arc::clone(&self.swarm_state.coordinators),
        };
        super::super::persist_swarm_state_for(swarm_id, &swarm_state).await;
        super::super::swarm::broadcast_swarm_plan_with_previous(
            swarm_id,
            Some(reason.to_string()),
            previous_items,
            &self.swarm_state.plans,
            &self.swarm_state.members,
            &self.swarm_state.swarms_by_id,
        )
        .await;
    }

    /// Remove a session's member record, returning the removed member's identity
    /// (swarm id, swarm-enabled flag, friendly name). Leaner than
    /// `take_session_membership`: it only touches `members` and does not clear
    /// file-touch or channel subscriptions, so teardown paths that want to keep
    /// those until the caller has finished their own swarm teardown
    /// (`remove_session_from_swarm`) can fold their member removal through the
    /// handle instead of the raw map. No-op (empty identity) when no member was
    /// present.
    pub(crate) async fn remove_session_member(&self, session_id: &str) -> MemberIdentity {
        let mut members = self.swarm_state.members.write().await;
        match members.remove(session_id) {
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

    /// Toggle whether a session participates in its swarm (`set feature:swarm`).
    /// Runs the whole coordination transaction: flip `swarm_enabled`, on disable
    /// drop the member from its swarm and clear channel subscriptions; on enable
    /// re-derive the swarm id, register the member, broadcast status and
    /// persist. Also owns the client `Done`/`SwarmStatus` responses so session
    /// code never touches the raw maps (Tier 3).
    pub(crate) async fn toggle_swarm_membership(
        &self,
        request_id: u64,
        client_session_id: &str,
        enabled: bool,
        client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    ) {
        let (old_swarm_id, _working_dir) = {
            let mut members = self.swarm_state.members.write().await;
            if let Some(member) = members.get_mut(client_session_id) {
                let old = member.swarm_id.clone();
                let wd = member.working_dir.clone();
                member.swarm_enabled = enabled;
                if !enabled {
                    member.swarm_id = None;
                    member.role = "agent".to_string();
                }
                (old, wd)
            } else {
                (None, None)
            }
        };

        if let Some(ref old_id) = old_swarm_id {
            self.remove_session_from_swarm(client_session_id, old_id).await;
            self.remove_session_channel_subscriptions(client_session_id).await;
        }

        if enabled {
            let new_swarm_id = super::super::swarm_id_for_session(client_session_id);
            if let Some(ref id) = new_swarm_id {
                {
                    let mut swarms = self.swarm_state.swarms_by_id.write().await;
                    swarms
                        .entry(id.clone())
                        .or_insert_with(HashSet::new)
                        .insert(client_session_id.to_string());
                }

                {
                    let mut members = self.swarm_state.members.write().await;
                    if let Some(member) = members.get_mut(client_session_id) {
                        member.swarm_id = Some(id.clone());
                        member.role = "agent".to_string();
                    }
                }

                self.broadcast_swarm_status(id).await;
                let swarm_state = SwarmState {
                    members: Arc::clone(&self.swarm_state.members),
                    swarms_by_id: Arc::clone(&self.swarm_state.swarms_by_id),
                    plans: Arc::clone(&self.swarm_state.plans),
                    coordinators: Arc::clone(&self.swarm_state.coordinators),
                };
                super::super::persist_swarm_state_for(id, &swarm_state).await;
            } else {
                let _ = client_event_tx.send(ServerEvent::SwarmStatus {
                    members: Vec::new(),
                });
            }
        } else {
            let _ = client_event_tx.send(ServerEvent::SwarmStatus {
                members: Vec::new(),
            });
        }

        let _ = client_event_tx.send(ServerEvent::Done { id: request_id });
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

    /// Apply a coordinator's direct `/plan` update: upsert the plan, add the
    /// proposer and assignee participants, replace the item set, bump the
    /// version, then persist, broadcast, and record a `PlanUpdate` event.
    /// Returns `(version, participant_ids, item_count)` so the caller can fan
    /// out notifications. The whole coordination transaction runs here so the
    /// caller never touches the raw `plans` map (Tier 3).
    pub(crate) async fn propose_coordinator_plan_update(
        &self,
        swarm_id: &str,
        req_session_id: &str,
        items: Vec<crate::plan::PlanItem>,
        from_name: Option<String>,
    ) -> (u64, HashSet<String>, usize) {
        let (version, participant_ids) = {
            let mut plans = self.swarm_state.plans.write().await;
            let plan = plans
                .entry(swarm_id.to_string())
                .or_insert_with(crate::plan::VersionedPlan::new);
            plan.participants.insert(req_session_id.to_string());
            for item in &items {
                if let Some(owner) = &item.assigned_to {
                    plan.participants.insert(owner.clone());
                }
            }
            plan.replace_items(items.clone());
            plan.version += 1;
            (plan.version, plan.participants.clone())
        };
        let item_count = items.len();

        let swarm_state = SwarmState {
            members: Arc::clone(&self.swarm_state.members),
            swarms_by_id: Arc::clone(&self.swarm_state.swarms_by_id),
            plans: Arc::clone(&self.swarm_state.plans),
            coordinators: Arc::clone(&self.swarm_state.coordinators),
        };
        super::super::persist_swarm_state_for(swarm_id, &swarm_state).await;

        super::super::swarm::broadcast_swarm_plan(
            swarm_id,
            Some("coordinator_direct_update".to_string()),
            &self.swarm_state.plans,
            &self.swarm_state.members,
            &self.swarm_state.swarms_by_id,
        )
        .await;

        self.record_swarm_event(
            req_session_id.to_string(),
            from_name,
            Some(swarm_id.to_string()),
            SwarmEventType::PlanUpdate {
                swarm_id: swarm_id.to_string(),
                item_count,
            },
        )
        .await;

        (version, participant_ids, item_count)
    }

    /// Approve a coordinator's merge of proposed plan items: append the items,
    /// bump the version, add the approver/proposer/assignee participants, then
    /// remove the proposal shared-context, persist, broadcast, and record a
    /// `PlanUpdate` event. Returns the participant fan-out set so the caller
    /// can notify. The whole coordination transaction runs here so the caller
    /// never touches the raw `plans` map (Tier 3).
    pub(crate) async fn approve_plan_merge(
        &self,
        swarm_id: &str,
        req_session_id: &str,
        proposer_session: &str,
        items: Vec<crate::plan::PlanItem>,
        proposal_key: &str,
    ) -> (HashSet<String>, usize) {
        let participant_ids = {
            let mut plans = self.swarm_state.plans.write().await;
            let plan = plans
                .entry(swarm_id.to_string())
                .or_insert_with(crate::plan::VersionedPlan::new);
            plan.items.extend(items.clone());
            plan.version += 1;
            plan.participants.insert(req_session_id.to_string());
            plan.participants.insert(proposer_session.to_string());
            for item in &items {
                if let Some(owner) = &item.assigned_to {
                    plan.participants.insert(owner.clone());
                }
            }
            plan.participants.clone()
        };
        let item_count = items.len();

        self.remove_shared_context(swarm_id, proposal_key).await;

        super::super::swarm::broadcast_swarm_plan(
            swarm_id,
            Some("proposal_approved".to_string()),
            &self.swarm_state.plans,
            &self.swarm_state.members,
            &self.swarm_state.swarms_by_id,
        )
        .await;

        self.record_swarm_event(
            req_session_id.to_string(),
            None,
            Some(swarm_id.to_string()),
            SwarmEventType::PlanUpdate {
                swarm_id: swarm_id.to_string(),
                item_count,
            },
        )
        .await;

        let swarm_state = SwarmState {
            members: Arc::clone(&self.swarm_state.members),
            swarms_by_id: Arc::clone(&self.swarm_state.swarms_by_id),
            plans: Arc::clone(&self.swarm_state.plans),
            coordinators: Arc::clone(&self.swarm_state.coordinators),
        };
        super::super::persist_swarm_state_for(swarm_id, &swarm_state).await;

        (participant_ids, item_count)
    }

    /// Elect a seeding session as coordinator when the swarm has no live
    /// coordinator, mirroring the self-promote rule used by `assign_role`.
    /// Owns the whole `coordinators` + `members`-role write transaction so
    /// comm_graph's seed path never touches the raw maps (Tier 3). Returns
    /// whether the seeder is the coordinator afterwards.
    ///
    /// A live, non-headless coordinator is left untouched so a real coordinator
    /// is never displaced by a worker that happens to seed.
    pub(crate) async fn elect_seeder_coordinator(
        &self,
        swarm_id: &str,
        seeder_session_id: &str,
    ) -> bool {
        // 1. Read the current coordinator id without holding the lock across the
        //    liveness check (matches the non-nested lock pattern used elsewhere).
        let current = self
            .swarm_state
            .coordinators
            .read()
            .await
            .get(swarm_id)
            .cloned();
        if current.as_deref() == Some(seeder_session_id) {
            return true;
        }

        // 2. Decide whether the existing coordinator is still a live driver.
        let coordinator_is_live = match &current {
            Some(coord) => self
                .swarm_state
                .members
                .read()
                .await
                .get(coord)
                .map(|member| !member.event_tx.is_closed() && !member.is_headless)
                .unwrap_or(false),
            None => false,
        };
        if coordinator_is_live {
            return false;
        }

        // 3. Promote the seeder; demote any prior (stale) coordinator member. Re-check
        //    under the write lock that the coordinator is still the one we inspected
        //    (compare-and-swap): two concurrent seeders race here, and the loser must
        //    not silently displace the winner it never liveness-checked.
        let prior = {
            let mut coordinators = self.swarm_state.coordinators.write().await;
            if coordinators.get(swarm_id) != current.as_ref() {
                // Someone else changed the coordinator between our read and write.
                return coordinators.get(swarm_id).map(String::as_str) == Some(seeder_session_id);
            }
            coordinators.insert(swarm_id.to_string(), seeder_session_id.to_string())
        };
        {
            let mut members = self.swarm_state.members.write().await;
            if let Some(member) = members.get_mut(seeder_session_id) {
                member.role = "coordinator".to_string();
            }
            if let Some(prior) = prior
                && prior != seeder_session_id
                && let Some(member) = members.get_mut(&prior)
            {
                member.role = "agent".to_string();
            }
        }
        true
    }

    /// Apply a task-DAG mutation (seed/expand/complete/inject) to a swarm plan
    /// as one whole transaction. Takes the raw `plans` write lock, runs the
    /// caller's synchronous transformation (which _must_ not touch any other
    /// swarm map), and on success commits: persist, broadcast, record a
    /// `PlanUpdate` event, and emit a `Done` (or `Error`) response. The caller
    /// supplies the per-op rejection logic so engine error messages are
    /// preserved, and is responsible for bumping `plan.version` (seed only
    /// bumps when the graph actually changed, expand/complete/inject always
    /// do). Set `create_if_missing` for ops that may seed a brand-new plan
    /// (seed); ops that require an existing plan will otherwise error out.
    ///
    /// This keeps every graph-write site single-homed on the handle so
    /// comm_graph never holds the raw plans lock (Tier 3).
    #[expect(
        clippy::too_many_arguments,
        reason = "mutate_task_dag threads request ids, swarm identity, commit reasons, client responses, and the transform closure"
    )]
    pub(crate) async fn mutate_task_dag<F>(
        &self,
        id: u64,
        swarm_id: &str,
        req_session_id: &str,
        reason: &str,
        item_count: usize,
        client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
        create_if_missing: bool,
        transform: F,
    ) where
        F: FnOnce(&mut crate::plan::VersionedPlan) -> Result<(), String>,
    {
        let result = {
            let mut plans = self.swarm_state.plans.write().await;
            let plan = if create_if_missing {
                plans
                    .entry(swarm_id.to_string())
                    .or_insert_with(crate::plan::VersionedPlan::new)
            } else {
                match plans.get_mut(swarm_id) {
                    Some(plan) => plan,
                    None => {
                        let _ = client_event_tx.send(ServerEvent::Error {
                            id,
                            message: "No plan for this swarm.".to_string(),
                            retry_after_secs: None,
                        });
                        return;
                    }
                }
            };
            transform(plan).map(|()| plan.version)
        };

        match result {
            Ok(_version) => {
                let from_name = self
                    .swarm_state
                    .members
                    .read()
                    .await
                    .get(req_session_id)
                    .and_then(|member| member.friendly_name.clone());

                let swarm_state = SwarmState {
                    members: Arc::clone(&self.swarm_state.members),
                    swarms_by_id: Arc::clone(&self.swarm_state.swarms_by_id),
                    plans: Arc::clone(&self.swarm_state.plans),
                    coordinators: Arc::clone(&self.swarm_state.coordinators),
                };
                super::super::persist_swarm_state_for(swarm_id, &swarm_state).await;
                super::super::swarm::broadcast_swarm_plan(
                    swarm_id,
                    Some(reason.to_string()),
                    &self.swarm_state.plans,
                    &self.swarm_state.members,
                    &self.swarm_state.swarms_by_id,
                )
                .await;
                self.record_swarm_event(
                    req_session_id.to_string(),
                    from_name,
                    Some(swarm_id.to_string()),
                    SwarmEventType::PlanUpdate {
                        swarm_id: swarm_id.to_string(),
                        item_count,
                    },
                )
                .await;
                let _ = client_event_tx.send(ServerEvent::Done { id });
            }
            Err(message) => {
                let _ = client_event_tx.send(ServerEvent::Error {
                    id,
                    message,
                    retry_after_secs: None,
                });
            }
        }
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

    /// Resolve a session's swarm membership and whether it is that swarm's
    /// coordinator. Returns `(swarm_id, is_coordinator)`. Shared by the handle's
    /// own `require_coordinator_swarm` and by callers (e.g. the debug plan
    /// commands) that need the raw identity without emitting an error event.
    pub(crate) async fn coordinator_identity(&self, session_id: &str) -> (Option<String>, bool) {
        // Resolve the member's swarm id without retaining the members lock while
        // reading coordinators (independent locks, never a path back to members).
        let swarm_id = {
            let members = self.swarm_state.members.read().await;
            members
                .get(session_id)
                .and_then(|member| member.swarm_id.clone())
        };
        let is_coordinator = if let Some(ref swarm_id) = swarm_id {
            let coordinators = self.swarm_state.coordinators.read().await;
            coordinators
                .get(swarm_id)
                .map(|coordinator| coordinator == session_id)
                .unwrap_or(false)
        } else {
            false
        };
        (swarm_id, is_coordinator)
    }

    /// Require that `req_session_id` is the coordinator of its own swarm.
    ///
    /// Resolves the requesting session's swarm id, verifies the session is its
    /// coordinator, and returns `Some(swarm_id)` on success. On failure sends
    /// the appropriate `ServerEvent::Error` and returns `None`. Mirrors the
    /// `require_coordinator_swarm` permission guard used by the plan-decision
    /// handlers.
    pub(crate) async fn require_coordinator_swarm(
        &self,
        id: u64,
        req_session_id: &str,
        permission_error: &str,
        client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    ) -> Option<String> {
        let (swarm_id, is_coordinator) = self.coordinator_identity(req_session_id).await;

        if !is_coordinator {
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: permission_error.to_string(),
                retry_after_secs: None,
            });
            return None;
        }

        match swarm_id {
            Some(swarm_id) => Some(swarm_id),
            None => {
                let _ = client_event_tx.send(ServerEvent::Error {
                    id,
                    message: "Not in a swarm.".to_string(),
                    retry_after_secs: None,
                });
                None
            }
        }
    }

    /// Guard that `req_session_id` and `target_session` belong to the same
    /// swarm. Sends an error and returns `false` when they do not. Mirrors the
    /// `ensure_same_swarm_access` permission guard used by the comm read
    /// helpers.
    pub(crate) async fn ensure_same_swarm_access(
        &self,
        id: u64,
        req_session_id: &str,
        target_session: &str,
        client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    ) -> bool {
        let (req_swarm, target_swarm) = self.member_swarm_ids(req_session_id, target_session).await;

        if req_swarm.is_some() && req_swarm == target_swarm {
            true
        } else {
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: format!(
                    "Session '{}' is not in the same swarm as requester '{}'",
                    target_session, req_session_id
                ),
                retry_after_secs: None,
            });
            false
        }
    }

    /// Require that `req_session_id` may drive the plan for its swarm: either it
    /// is the coordinator, or the plan runs in deep mode and the session is a
    /// participant. Returns `Some(swarm_id)` on success, else sends the
    /// permission error and returns `None`. Mirrors the `require_plan_driver_swarm`
    /// guard used by the assign / task-control handlers.
    ///
    /// Light mode keeps the single-coordinator rule (matching the cheap fan-out
    /// preset): only the coordinator is a driver. Deep mode follows the task-DAG
    /// ownership model (see `docs/SWARM_TASK_GRAPH.md` section 2): the plan is a
    /// tree of ownership, so the agent that seeded / participates in the graph
    /// may dispatch it even when another session holds the swarm-level
    /// coordinator slot — otherwise a deep-mode agent that joins a shared swarm
    /// could seed a graph but then be blocked from spawning/assigning any of it.
    pub(crate) async fn require_plan_driver_swarm(
        &self,
        id: u64,
        req_session_id: &str,
        permission_error: &str,
        client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    ) -> Option<String> {
        let swarm_id = self.member_swarm_id(req_session_id).await;
        let Some(swarm_id) = swarm_id else {
            let _ = client_event_tx.send(ServerEvent::Error {
                id,
                message: "Not in a swarm.".to_string(),
                retry_after_secs: None,
            });
            return None;
        };

        let is_coordinator = {
            let coordinators = self.swarm_state.coordinators.read().await;
            coordinators
                .get(&swarm_id)
                .map(|coordinator| coordinator == req_session_id)
                .unwrap_or(false)
        };
        if is_coordinator {
            return Some(swarm_id);
        }

        // Deep mode: any participant of the plan may drive its own task graph.
        let is_deep_participant = {
            let plans = self.swarm_state.plans.read().await;
            plans
                .get(&swarm_id)
                .map(|plan| {
                    jcode_plan::bridge::parse_mode(&plan.mode) == jcode_plan::dag::Mode::Deep
                        && plan.participants.contains(req_session_id)
                })
                .unwrap_or(false)
        };
        if is_deep_participant {
            return Some(swarm_id);
        }

        let _ = client_event_tx.send(ServerEvent::Error {
            id,
            message: permission_error.to_string(),
            retry_after_secs: None,
        });
        None
    }

    /// Clear `swarm_id`'s coordinator, demoting any swarm members whose role is
    /// `coordinator` back to `agent`, then persist the change. Returns whether
    /// a coordinator was actually removed. This is the debug
    /// `swarm:clear_coordinator` operation.
    pub(crate) async fn clear_coordinator(&self, swarm_id: &str) -> bool {
        // Never nest these independent locks: persistence re-reads
        // coordinators, so retaining a write guard here self-deadlocks.
        let removed = {
            let mut coordinators = self.swarm_state.coordinators.write().await;
            coordinators.remove(swarm_id).is_some()
        };
        if removed {
            {
                let mut members = self.swarm_state.members.write().await;
                for member in members.values_mut() {
                    if member.swarm_id.as_deref() == Some(swarm_id) && member.role == "coordinator"
                    {
                        member.role = "agent".to_string();
                    }
                }
            }
            let swarm_state = SwarmState {
                members: self.swarm_state.members.clone(),
                swarms_by_id: self.swarm_state.swarms_by_id.clone(),
                plans: self.swarm_state.plans.clone(),
                coordinators: self.swarm_state.coordinators.clone(),
            };
            super::super::persist_swarm_state_for(swarm_id, &swarm_state).await;
        }
        removed
    }

    /// Clear `swarm_id`'s plan: remove it from the plans map, re-persist so the
    /// on-disk state drops it too (otherwise the next restart resurrects the
    /// stale plan graph), and broadcast a `plan_cleared` `ServerEvent::SwarmPlan`
    /// to every attached session so their TUIs drop the resident item graph.
    /// Returns the removed plan, or `None` if no plan existed. This is the
    /// debug `swarm:clear_plan` operation.
    pub(crate) async fn clear_plan(&self, swarm_id: &str) -> Option<VersionedPlan> {
        let removed = {
            let mut plans = self.swarm_state.plans.write().await;
            plans.remove(swarm_id)
        };
        let removed = removed?;

        let swarm_state = SwarmState {
            members: self.swarm_state.members.clone(),
            swarms_by_id: self.swarm_state.swarms_by_id.clone(),
            plans: self.swarm_state.plans.clone(),
            coordinators: self.swarm_state.coordinators.clone(),
        };
        super::super::persist_swarm_state_for(swarm_id, &swarm_state).await;

        let clear_event = ServerEvent::SwarmPlan {
            swarm_id: swarm_id.to_string(),
            version: removed.version.saturating_add(1),
            items: Vec::new(),
            participants: Vec::new(),
            reason: Some("plan_cleared".to_string()),
            summary: None,
        };
        let session_ids: Vec<String> = {
            let swarms = self.swarm_state.swarms_by_id.read().await;
            swarms
                .get(swarm_id)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default()
        };
        {
            let members = self.swarm_state.members.read().await;
            for sid in session_ids {
                if let Some(member) = members.get(&sid) {
                    let _ = member.event_tx.send(clear_event.clone());
                    for tx in member.event_txs.values() {
                        let _ = tx.send(clear_event.clone());
                    }
                }
            }
        }
        Some(removed)
    }

    /// Attach a session to its swarm's plan participant set on a resync
    /// (`/plan-resync`) and run the whole transaction: add the participant,
    /// persist, notify the session, broadcast the refreshed plan, and record a
    /// `PlanUpdate` event. Also owns the client `Done`/`Error` responses.
    ///
    /// The entire coordination transaction runs here on the handle so session
    /// code never touches the raw `plans` map in place.
    pub(crate) async fn resync_plan_participant(
        &self,
        request_id: u64,
        req_session_id: &str,
        client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
    ) {
        let swarm_id = {
            let members = self.swarm_state.members.read().await;
            members
                .get(req_session_id)
                .and_then(|member| member.swarm_id.clone())
        };

        let Some(swarm_id) = swarm_id else {
            let _ = client_event_tx.send(ServerEvent::Error {
                id: request_id,
                message: "Not in a swarm.".to_string(),
                retry_after_secs: None,
            });
            return;
        };

        let plan_state = {
            let mut plans = self.swarm_state.plans.write().await;
            plans.get_mut(&swarm_id).map(|plan| {
                plan.participants.insert(req_session_id.to_string());
                (plan.version, plan.items.len())
            })
        };
        let Some((version, item_count)) = plan_state else {
            let _ = client_event_tx.send(ServerEvent::Error {
                id: request_id,
                message: "No swarm plan exists for this swarm.".to_string(),
                retry_after_secs: None,
            });
            return;
        };

        // Snapshot the swarm maps so persistence sees the just-mutated plan.
        let swarm_state = SwarmState {
            members: Arc::clone(&self.swarm_state.members),
            swarms_by_id: Arc::clone(&self.swarm_state.swarms_by_id),
            plans: Arc::clone(&self.swarm_state.plans),
            coordinators: Arc::clone(&self.swarm_state.coordinators),
        };
        super::super::persist_swarm_state_for(&swarm_id, &swarm_state).await;

        if let Some(member) = self.swarm_state.members.read().await.get(req_session_id) {
            let _ = member.event_tx.send(ServerEvent::Notification {
                from_session: req_session_id.to_string(),
                from_name: member.friendly_name.clone(),
                notification_type: NotificationType::Message {
                    scope: Some("plan".to_string()),
                    channel: None,
                    tldr: None,
                },
                message: format!(
                    "Plan attached to this session (v{}, {} items).",
                    version, item_count
                ),
            });
        }

        super::super::swarm::broadcast_swarm_plan(
            &swarm_id,
            Some("resync".to_string()),
            &self.swarm_state.plans,
            &self.swarm_state.members,
            &self.swarm_state.swarms_by_id,
        )
        .await;

        super::super::swarm::record_swarm_event(
            &self.event_history,
            &self.event_counter,
            &self.swarm_event_tx,
            req_session_id.to_string(),
            None,
            Some(swarm_id.clone()),
            SwarmEventType::PlanUpdate {
                swarm_id: swarm_id.clone(),
                item_count,
            },
        )
        .await;

        let _ = client_event_tx.send(ServerEvent::Done { id: request_id });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::VersionedPlan;

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
            second.as_ref().expect("entry").created_at,
            created,
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
            handle
                .get_shared_context("swarm-1", "empty")
                .await
                .unwrap()
                .value,
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
        let rev = handle.channel_subscriptions_by_session_map().read().await;
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

    #[tokio::test]
    async fn rename_member_session_rewrites_coordinator_slot() {
        let coord = |id: &str| {
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
            SwarmMember {
                session_id: id.to_string(),
                event_tx,
                event_txs: HashMap::new(),
                working_dir: Some(id.to_string().into()),
                swarm_id: Some("swarm-test".to_string()),
                swarm_enabled: true,
                status: "ready".to_string(),
                detail: None,
                task_label: None,
                friendly_name: Some(id.to_string()),
                report_back_to_session_id: None,
                latest_completion_report: None,
                role: "coordinator".to_string(),
                joined_at: Instant::now(),
                last_status_change: Instant::now(),
                is_headless: false,
                output_tail: None,
                todo_progress: None,
                todo_items: Vec::new(),
                runtime: crate::protocol::SwarmMemberRuntime::default(),
            }
        };
        let handle = SwarmServiceHandle::test_with_state(
            SwarmState {
                members: Arc::new(RwLock::new(HashMap::from([
                    ("old".to_string(), coord("old")),
                    ("child".to_string(), coord("child")),
                ]))),
                swarms_by_id: Arc::new(RwLock::new(HashMap::from([(
                    "swarm-test".to_string(),
                    HashSet::from(["old".to_string(), "child".to_string()]),
                )]))),
                plans: Arc::new(RwLock::new(HashMap::new())),
                coordinators: Arc::new(RwLock::new(HashMap::from([(
                    "swarm-test".to_string(),
                    "old".to_string(),
                )]))),
            },
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(HashMap::new())),
            SwarmMutationRuntime::default(),
        );

        handle.rename_member_session("old", "new").await;

        // The member map now keys by the new id.
        assert!(
            handle
                .swarm_state()
                .members
                .read()
                .await
                .contains_key("new")
        );
        // The coordinator slot follows the renamed session.
        let coordinators = handle.swarm_state().coordinators.read().await;
        assert_eq!(
            coordinators.get("swarm-test").map(String::as_str),
            Some("new"),
            "routing a coordinator through a rename must point at the new session id"
        );
    }

    async fn insert_member(
        handle: &SwarmServiceHandle,
        session_id: &str,
        swarm_id: &str,
        role: &str,
    ) {
        let member = SwarmMember {
            session_id: session_id.to_string(),
            event_tx: mpsc::unbounded_channel().0,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: Some(swarm_id.to_string()),
            swarm_enabled: true,
            status: "ready".to_string(),
            detail: None,
            task_label: None,
            friendly_name: None,
            report_back_to_session_id: None,
            latest_completion_report: None,
            role: role.to_string(),
            joined_at: Instant::now(),
            last_status_change: Instant::now(),
            is_headless: false,
            output_tail: None,
            todo_progress: None,
            todo_items: Vec::new(),
            runtime: Default::default(),
        };
        handle
            .swarm_state()
            .members
            .write()
            .await
            .insert(session_id.to_string(), member);
    }

    #[tokio::test]
    async fn member_swarm_id_and_ids_resolve_membership() {
        let handle = base_handle();
        assert_eq!(handle.member_swarm_id("sess-1").await, None);

        insert_member(&handle, "sess-1", "swarm-A", "agent").await;
        insert_member(&handle, "sess-2", "swarm-A", "coordinator").await;
        insert_member(&handle, "sess-3", "swarm-B", "agent").await;
        assert_eq!(
            handle.member_swarm_id("sess-1").await,
            Some("swarm-A".into())
        );
        assert_eq!(handle.member_swarm_id("sess-missing").await, None);
        assert_eq!(
            handle.member_swarm_ids("sess-1", "sess-2").await,
            (Some("swarm-A".into()), Some("swarm-A".into()))
        );
        assert_eq!(
            handle.member_swarm_ids("sess-1", "sess-3").await,
            (Some("swarm-A".into()), Some("swarm-B".into()))
        );
    }

    #[tokio::test]
    async fn remove_session_member_drops_only_the_member_and_returns_swarm_id() {
        let handle = SwarmServiceHandle::test_with_state(
            SwarmState {
                members: Arc::new(RwLock::new(HashMap::from([("sess".to_string(), {
                    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
                    SwarmMember {
                        session_id: "sess".to_string(),
                        event_tx,
                        event_txs: HashMap::new(),
                        working_dir: None,
                        swarm_id: Some("swarm-1".to_string()),
                        swarm_enabled: true,
                        status: "ready".to_string(),
                        detail: None,
                        task_label: None,
                        friendly_name: Some("sess".to_string()),
                        report_back_to_session_id: None,
                        latest_completion_report: None,
                        role: "agent".to_string(),
                        joined_at: Instant::now(),
                        last_status_change: Instant::now(),
                        is_headless: false,
                        output_tail: None,
                        todo_progress: None,
                        todo_items: Vec::new(),
                        runtime: crate::protocol::SwarmMemberRuntime::default(),
                    }
                })]))),
                swarms_by_id: Arc::new(RwLock::new(HashMap::from([(
                    "swarm-1".to_string(),
                    HashSet::from(["sess".to_string()]),
                )]))),
                plans: Arc::new(RwLock::new(HashMap::new())),
                coordinators: Arc::new(RwLock::new(HashMap::new())),
            },
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(HashMap::new())),
            SwarmMutationRuntime::default(),
        );

        let removed = handle.remove_session_member("sess").await;
        assert_eq!(removed.swarm_id.as_deref(), Some("swarm-1"));
        assert_eq!(removed.friendly_name.as_deref(), Some("sess"));
        // The member record is gone, but swarms_by_id (swarm-level membership)
        // is untouched: the caller decides whether to also run
        // remove_session_from_swarm.
        assert!(
            !handle
                .swarm_state()
                .members
                .read()
                .await
                .contains_key("sess")
        );
        assert!(
            handle
                .swarm_state()
                .swarms_by_id
                .read()
                .await
                .contains_key("swarm-1")
        );

        // A second removal is a clean no-op (empty identity).
        let again = handle.remove_session_member("sess").await;
        assert!(again.swarm_id.is_none() && again.friendly_name.is_none());
    }

    #[tokio::test]
    async fn register_headless_member_inserts_member_and_swarm_membership() {
        let handle = base_handle();
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        handle
            .register_headless_member(
                "headless-1",
                None,
                Some("swarm-h"),
                true,
                "headless-1".to_string(),
                None,
                event_tx,
                crate::protocol::SwarmMemberRuntime::default(),
            )
            .await;

        let members = handle.swarm_state().members.read().await;
        let member = members.get("headless-1").expect("member inserted");
        assert!(
            member.is_headless,
            "headless member carries is_headless=true"
        );
        assert_eq!(member.swarm_id.as_deref(), Some("swarm-h"));
        assert!(member.event_txs.is_empty(), "no per-connection senders");

        let swarms = handle.swarm_state().swarms_by_id.read().await;
        let swarm = swarms.get("swarm-h").expect("swarm created");
        assert!(swarm.contains("headless-1"));

        // No swarm id: no swarms_by_id entry is created.
        let (event_tx2, _event_rx2) = tokio::sync::mpsc::unbounded_channel();
        let h2 = base_handle();
        h2.register_headless_member(
            "solo",
            None,
            None,
            false,
            "solo".to_string(),
            None,
            event_tx2,
            crate::protocol::SwarmMemberRuntime::default(),
        )
        .await;
        assert!(h2.swarm_state().swarms_by_id.read().await.is_empty());

        // Invariant: an inconsistent caller passing Some(swarm_id) with
        // swarm_enabled=false must not tag the member or create a swarms_by_id
        // entry.
        let (event_tx3, _event_rx3) = tokio::sync::mpsc::unbounded_channel();
        let h3 = base_handle();
        h3.register_headless_member(
            "disabled",
            None,
            Some("swarm-x"),
            false,
            "disabled".to_string(),
            None,
            event_tx3,
            crate::protocol::SwarmMemberRuntime::default(),
        )
        .await;
        assert!(
            h3.swarm_state()
                .members
                .read()
                .await
                .get("disabled")
                .unwrap()
                .swarm_id
                .is_none(),
            "a swarm-disabled member must carry no swarm id"
        );
        assert!(
            !h3.swarm_state()
                .swarms_by_id
                .read()
                .await
                .contains_key("swarm-x"),
            "a swarm-disabled member must not create a swarms_by_id entry"
        );
    }

    #[tokio::test]
    async fn rename_completes_while_coordinator_write_is_held() {
        // Regression guard for the coordinator rewrite folded into
        // rename_member_session: the member/swarms writes must be released
        // before the method acquires the coordinators write lock. If instead it
        // held the member (or swarms) write while waiting on coordinators, a
        // concurrent path that already holds coordinators would deadlock.
        let handle = {
            let s = SwarmState {
                members: Arc::new(RwLock::new(HashMap::from([("old".to_string(), {
                    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
                    SwarmMember {
                        session_id: "old".to_string(),
                        event_tx,
                        event_txs: HashMap::new(),
                        working_dir: None,
                        swarm_id: Some("swarm-test".to_string()),
                        swarm_enabled: true,
                        status: "ready".to_string(),
                        detail: None,
                        task_label: None,
                        friendly_name: Some("old".to_string()),
                        report_back_to_session_id: None,
                        latest_completion_report: None,
                        role: "agent".to_string(),
                        joined_at: Instant::now(),
                        last_status_change: Instant::now(),
                        is_headless: false,
                        output_tail: None,
                        todo_progress: None,
                        todo_items: Vec::new(),
                        runtime: crate::protocol::SwarmMemberRuntime::default(),
                    }
                })]))),
                swarms_by_id: Arc::new(RwLock::new(HashMap::from([(
                    "swarm-test".to_string(),
                    HashSet::from(["old".to_string()]),
                )]))),
                plans: Arc::new(RwLock::new(HashMap::new())),
                coordinators: Arc::new(RwLock::new(HashMap::from([(
                    "swarm-test".to_string(),
                    "old".to_string(),
                )]))),
            };
            SwarmServiceHandle::test_with_state(
                s,
                Arc::new(RwLock::new(HashMap::new())),
                Arc::new(RwLock::new(HashMap::new())),
                Arc::new(RwLock::new(HashMap::new())),
                SwarmMutationRuntime::default(),
            )
        };
        let coordinators = handle.swarm_state().coordinators.clone();

        // A concurrent owner holds the coordinators write lock. rename_member_session
        // must NOT block before completing the members rename: it acquires
        // members, then swarms, and only last tries coordinators. If it held a
        // member (or swarm) write while waiting on the coordinator lock, the
        // members rename would not be visible until the external lock drops,
        // and with the guard held forever it would deadlock.
        let guard = coordinators.write().await;
        let rename_task = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle.rename_member_session("old", "new").await;
            }
        });

        // The members rename must complete while the coordinator write is still
        // held by us, proving the method reaches and releases the members write
        // before waiting on coordinators.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if handle
                    .swarm_state()
                    .members
                    .read()
                    .await
                    .contains_key("new")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("members rename must progress while the coordinator write lock is held");

        // Release the coordinator lock; the rename completes its coordinator
        // rewrite and the task finishes.
        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(2), rename_task)
            .await
            .expect("rename task must finish once the coordinator lock is released")
            .expect("rename task must not panic");

        let coordinators = coordinators.read().await;
        assert_eq!(
            coordinators.get("swarm-test").map(String::as_str),
            Some("new"),
            "coordinator follows the renamed session once the external lock is released"
        );
    }

    #[tokio::test]
    async fn ensure_same_swarm_access_accepts_same_swarm_and_rejects_foreign() {
        let handle = base_handle();
        insert_member(&handle, "req", "swarm-A", "agent").await;
        insert_member(&handle, "same", "swarm-A", "agent").await;
        insert_member(&handle, "other", "swarm-B", "agent").await;
        let (tx, mut rx) = mpsc::unbounded_channel();

        assert!(
            handle.ensure_same_swarm_access(1, "req", "same", &tx).await,
            "same-swarm members pass"
        );
        let recipient_unmatched = handle
            .ensure_same_swarm_access(2, "req", "other", &tx)
            .await;
        assert!(!recipient_unmatched, "different swarm is rejected");
        match rx.try_recv() {
            Ok(ServerEvent::Error { id, .. }) => assert_eq!(id, 2),
            other => panic!("expected error event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resync_plan_participant_adds_member_and_emits_done() {
        let handle = base_handle();
        let (client_tx, mut client_rx) = tokio::sync::mpsc::unbounded_channel();

        let member = {
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
            SwarmMember {
                session_id: "sess-1".to_string(),
                event_tx,
                event_txs: HashMap::new(),
                working_dir: None,
                swarm_id: Some("swarm-1".to_string()),
                swarm_enabled: true,
                status: "ready".to_string(),
                detail: None,
                task_label: None,
                friendly_name: Some("worker".to_string()),
                report_back_to_session_id: None,
                latest_completion_report: None,
                role: "agent".to_string(),
                joined_at: Instant::now(),
                last_status_change: Instant::now(),
                is_headless: false,
                output_tail: None,
                todo_progress: None,
                todo_items: Vec::new(),
                runtime: crate::protocol::SwarmMemberRuntime::default(),
            }
        };

        let mut plan = crate::plan::VersionedPlan::new();
        plan.version = 3;
        plan.participants.insert("other".to_string());
        plan.items = vec![crate::plan::PlanItem {
            content: "task".to_string(),
            status: "todo".to_string(),
            priority: "normal".to_string(),
            id: "t-1".to_string(),
            subsystem: None,
            file_scope: Vec::new(),
            blocked_by: Vec::new(),
            assigned_to: None,
        }];

        {
            let mut members = handle.swarm_state().members.write().await;
            members.insert("sess-1".to_string(), member);
        }
        {
            let mut plans = handle.swarm_state().plans.write().await;
            plans.insert("swarm-1".to_string(), plan);
        }

        handle
            .resync_plan_participant(99, "sess-1", &client_tx)
            .await;

        // Participant added.
        {
            let plans = handle.swarm_state().plans.read().await;
            let plan = plans.get("swarm-1").expect("plan exists");
            assert!(plan.participants.contains("sess-1"));
            assert!(plan.participants.contains("other"));
        }

        // Client gets a Done (not an Error).
        let got = client_rx.try_recv().expect("a client event");
        match got {
            ServerEvent::Done { id } => assert_eq!(id, 99),
            other => panic!("expected Done, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn require_coordinator_swarm_grants_only_the_coordinator() {
        let handle = base_handle();
        insert_member(&handle, "sess-1", "swarm-A", "coordinator").await;
        insert_member(&handle, "sess-2", "swarm-A", "agent").await;
        handle
            .swarm_state()
            .coordinators
            .write()
            .await
            .insert("swarm-A".to_string(), "sess-1".to_string());
        let (tx, _rx) = mpsc::unbounded_channel();

        assert_eq!(
            handle
                .require_coordinator_swarm(1, "sess-1", "denied", &tx)
                .await,
            Some("swarm-A".into()),
            "coordinator is granted"
        );
        assert_eq!(
            handle
                .require_coordinator_swarm(2, "sess-2", "denied", &tx)
                .await,
            None,
            "plain member is denied"
        );
        assert_eq!(
            handle
                .require_coordinator_swarm(3, "ghost", "denied", &tx)
                .await,
            None,
            "unknown session is denied"
        );
    }

    #[tokio::test]
    async fn can_read_full_context_allows_self_and_coordinator() {
        let handle = base_handle();
        insert_member(&handle, "coord", "swarm-A", "coordinator").await;
        insert_member(&handle, "agent-1", "swarm-A", "agent").await;
        insert_member(&handle, "agent-2", "swarm-A", "agent").await;
        // Self always allowed.
        assert!(
            handle.can_read_full_context("agent-1", "agent-1").await,
            "self is always readable"
        );
        // Coordinator reading any target in the swarm.
        assert!(
            handle.can_read_full_context("coord", "agent-2").await,
            "coordinator may read any member"
        );
        assert!(
            !handle.can_read_full_context("agent-1", "agent-2").await,
            "non-coordinator may not read other members"
        );
    }

    #[tokio::test]
    async fn require_plan_driver_swarm_grants_coordinator_and_deep_participant() {
        let handle = base_handle();
        insert_member(&handle, "coord", "swarm-A", "coordinator").await;
        insert_member(&handle, "member", "swarm-A", "agent").await;
        handle
            .swarm_state()
            .coordinators
            .write()
            .await
            .insert("swarm-A".to_string(), "coord".to_string());
        // Deep-mode plan where `member` participates.
        handle.swarm_state().plans.write().await.insert(
            "swarm-A".to_string(),
            VersionedPlan {
                items: Vec::new(),
                version: 1,
                participants: HashSet::from(["member".to_string()]),
                task_progress: HashMap::new(),
                mode: "deep".to_string(),
                node_meta: HashMap::new(),
            },
        );
        let (tx, _rx) = mpsc::unbounded_channel();

        assert_eq!(
            handle
                .require_plan_driver_swarm(1, "coord", "denied", &tx)
                .await,
            Some("swarm-A".into()),
            "coordinator may drive the plan"
        );
        assert_eq!(
            handle
                .require_plan_driver_swarm(2, "member", "denied", &tx)
                .await,
            Some("swarm-A".into()),
            "deep-mode participant may drive the plan"
        );
        assert_eq!(
            handle
                .require_plan_driver_swarm(3, "ghost", "denied", &tx)
                .await,
            None,
            "unknown session is denied"
        );
    }

    #[tokio::test]
    async fn require_plan_driver_swarm_denies_non_coordinator_in_light_mode() {
        let handle = base_handle();
        insert_member(&handle, "coord", "swarm-A", "coordinator").await;
        insert_member(&handle, "member", "swarm-A", "agent").await;
        handle
            .swarm_state()
            .coordinators
            .write()
            .await
            .insert("swarm-A".to_string(), "coord".to_string());
        // Light-mode plan: only the single coordinator may drive.
        handle.swarm_state().plans.write().await.insert(
            "swarm-A".to_string(),
            VersionedPlan {
                items: Vec::new(),
                version: 1,
                participants: HashSet::from(["member".to_string()]),
                task_progress: HashMap::new(),
                mode: "light".to_string(),
                node_meta: HashMap::new(),
            },
        );
        let (tx, _rx) = mpsc::unbounded_channel();

        assert_eq!(
            handle
                .require_plan_driver_swarm(1, "member", "denied", &tx)
                .await,
            None,
            "light-mode non-coordinator cannot drive"
        );
    }

    #[tokio::test]
    async fn clear_coordinator_demotes_members_and_reports_removal() {
        let handle = base_handle();
        insert_member(&handle, "coord", "swarm-A", "coordinator").await;
        insert_member(&handle, "agent", "swarm-A", "agent").await;
        handle
            .swarm_state()
            .coordinators
            .write()
            .await
            .insert("swarm-A".to_string(), "coord".to_string());

        assert!(
            handle.clear_coordinator("swarm-A").await,
            "removes coordinator"
        );

        let coordinators = handle.swarm_state().coordinators.read().await;
        assert!(!coordinators.contains_key("swarm-A"));
        drop(coordinators);
        let members = handle.swarm_state().members.read().await;
        assert_eq!(
            members.get("coord").map(|m| m.role.as_str()),
            Some("agent"),
            "former coordinator is demoted to agent"
        );
        assert_eq!(members.get("agent").map(|m| m.role.as_str()), Some("agent"));

        // Clearing a swarm with no coordinator reports no removal.
        assert!(!handle.clear_coordinator("swarm-A").await);
    }

    #[tokio::test]
    async fn clear_plan_removes_persisted_plan() {
        let handle = base_handle();
        handle.swarm_state().plans.write().await.insert(
            "swarm-A".to_string(),
            VersionedPlan {
                items: Vec::new(),
                version: 3,
                participants: HashSet::new(),
                task_progress: HashMap::new(),
                mode: "deep".to_string(),
                node_meta: HashMap::new(),
            },
        );

        let removed = handle.clear_plan("swarm-A").await;
        assert_eq!(
            removed.map(|p| p.version),
            Some(3),
            "returns the removed plan"
        );
        assert!(
            handle.swarm_state().plans.read().await.is_empty(),
            "plan is dropped from the map"
        );

        // Clearing a missing plan is a no-op returning None.
        assert!(handle.clear_plan("swarm-nope").await.is_none());
    }

    #[tokio::test]
    async fn coordinator_identity_reports_swarm_and_coordinator_role() {
        let handle = base_handle();
        insert_member(&handle, "coord", "swarm-A", "coordinator").await;
        insert_member(&handle, "agent", "swarm-A", "agent").await;
        handle
            .swarm_state()
            .coordinators
            .write()
            .await
            .insert("swarm-A".to_string(), "coord".to_string());

        assert_eq!(
            handle.coordinator_identity("coord").await,
            (Some("swarm-A".into()), true),
            "coordinator reports its swarm and role"
        );
        assert_eq!(
            handle.coordinator_identity("agent").await,
            (Some("swarm-A".into()), false),
            "plain member reports its swarm but not coordinator role"
        );
        assert_eq!(
            handle.coordinator_identity("ghost").await,
            (None, false),
            "unknown session has no swarm and is not coordinator"
        );

        // Confirm swarm_id durability follows the members map, independent of
        // the coordinator map (e.g. after clear_coordinator).
        handle.clear_coordinator("swarm-A").await;
        assert_eq!(
            handle.coordinator_identity("coord").await,
            (Some("swarm-A".into()), false),
            "after clear_coordinator the member keeps its swarm but loses the role"
        );
    }

    #[tokio::test]
    async fn resync_plan_participant_errors_when_not_in_swarm() {
        let handle = base_handle();
        let (client_tx, mut client_rx) = tokio::sync::mpsc::unbounded_channel();
        // No member registered: not in a swarm.
        handle
            .resync_plan_participant(7, "unknown", &client_tx)
            .await;
        let got = client_rx.try_recv().expect("a client event");
        match got {
            ServerEvent::Error { id, message, .. } => {
                assert_eq!(id, 7);
                assert_eq!(message, "Not in a swarm.");
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn toggle_swarm_membership_enables_and_registers_member() {
        let handle = base_handle();
        let (client_tx, mut client_rx) = tokio::sync::mpsc::unbounded_channel();
        let member = {
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
            SwarmMember {
                session_id: "sess".to_string(),
                event_tx,
                event_txs: HashMap::new(),
                working_dir: None,
                swarm_id: None,
                swarm_enabled: false,
                status: "ready".to_string(),
                detail: None,
                task_label: None,
                friendly_name: Some("w".to_string()),
                report_back_to_session_id: None,
                latest_completion_report: None,
                role: "agent".to_string(),
                joined_at: Instant::now(),
                last_status_change: Instant::now(),
                is_headless: false,
                output_tail: None,
                todo_progress: None,
                todo_items: Vec::new(),
                runtime: crate::protocol::SwarmMemberRuntime::default(),
            }
        };
        {
            let mut members = handle.swarm_state().members.write().await;
            members.insert("sess".to_string(), member);
        }

        handle
            .toggle_swarm_membership(5, "sess", true, &client_tx)
            .await;

        // Member is swarm-enabled and has a derived swarm id.
        {
            let members = handle.swarm_state().members.read().await;
            let m = members.get("sess").expect("member");
            assert!(m.swarm_enabled);
            assert!(m.swarm_id.is_some());
        }
        // Client got a Done (the SwarmStatus for the empty-changed-dir branch,
        // or at minimum the trailing Done).
        let mut saw_done = false;
        while let Ok(ev) = client_rx.try_recv() {
            if matches!(ev, ServerEvent::Done { id: 5 }) {
                saw_done = true;
            }
        }
        assert!(saw_done, "expected a trailing Done for the toggle");
    }

    #[tokio::test]
    async fn toggle_swarm_membership_disable_clears_member_state() {
        let handle = base_handle();
        let (client_tx, _client_rx) = tokio::sync::mpsc::unbounded_channel();
        let member = {
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
            SwarmMember {
                session_id: "sess".to_string(),
                event_tx,
                event_txs: HashMap::new(),
                working_dir: None,
                swarm_id: Some("swarm-1".to_string()),
                swarm_enabled: true,
                status: "ready".to_string(),
                detail: None,
                task_label: None,
                friendly_name: Some("w".to_string()),
                report_back_to_session_id: None,
                latest_completion_report: None,
                role: "agent".to_string(),
                joined_at: Instant::now(),
                last_status_change: Instant::now(),
                is_headless: false,
                output_tail: None,
                todo_progress: None,
                todo_items: Vec::new(),
                runtime: crate::protocol::SwarmMemberRuntime::default(),
            }
        };
        {
            let mut members = handle.swarm_state().members.write().await;
            members.insert("sess".to_string(), member);
        }
        {
            let mut swarms = handle.swarm_state().swarms_by_id.write().await;
            swarms.insert("swarm-1".to_string(), ["sess".to_string()].into_iter().collect());
        }

        handle
            .toggle_swarm_membership(6, "sess", false, &client_tx)
            .await;

        // Member is disabled: not swarm-enabled, no swarm id, role reset.
        {
            let members = handle.swarm_state().members.read().await;
            let m = members.get("sess").expect("member");
            assert!(!m.swarm_enabled);
            assert!(m.swarm_id.is_none());
            assert_eq!(m.role, "agent");
        }
    }
    #[tokio::test]
    async fn propose_coordinator_plan_update_adds_participants() {
        let handle = base_handle();

        let items = vec![crate::plan::PlanItem {
            content: "task".to_string(),
            status: "todo".to_string(),
            priority: "normal".to_string(),
            id: "t-1".to_string(),
            subsystem: None,
            file_scope: Vec::new(),
            blocked_by: Vec::new(),
            assigned_to: Some("owner".to_string()),
        }];

        let (version, participants, _item_count) = handle
            .propose_coordinator_plan_update("swarm-1", "sess", items, None)
            .await;

        assert!(version >= 1);
        assert!(participants.contains("sess"));
        assert!(participants.contains("owner"));

        let plans = handle.swarm_state().plans.read().await;
        let plan = plans.get("swarm-1").expect("plan upserted");
        assert_eq!(plan.version, version);
        assert_eq!(plan.items.len(), 1);
    }

    #[tokio::test]
    async fn approve_plan_merge_appends_items_and_participants() {
        let handle = base_handle();

        let items = vec![crate::plan::PlanItem {
            content: "task".to_string(),
            status: "todo".to_string(),
            priority: "normal".to_string(),
            id: "t-2".to_string(),
            subsystem: None,
            file_scope: Vec::new(),
            blocked_by: Vec::new(),
            assigned_to: Some("owner".to_string()),
        }];

        // Seed an existing plan so the merge appends.
        {
            let mut plan = crate::plan::VersionedPlan::new();
            plan.version = 2;
            plan.items = vec![crate::plan::PlanItem {
                content: "existing".to_string(),
                status: "todo".to_string(),
                priority: "normal".to_string(),
                id: "t-0".to_string(),
                subsystem: None,
                file_scope: Vec::new(),
                blocked_by: Vec::new(),
                assigned_to: None,
            }];
            let mut plans = handle.swarm_state().plans.write().await;
            plans.insert("swarm-1".to_string(), plan);
        }

        let (participants, _item_count) = handle
            .approve_plan_merge("swarm-1", "coord", "proposer", items, "plan_proposal:proposer")
            .await;

        assert!(participants.contains("coord"));
        assert!(participants.contains("proposer"));
        assert!(participants.contains("owner"));

        let plans = handle.swarm_state().plans.read().await;
        let plan = plans.get("swarm-1").expect("plan exists");
        assert_eq!(plan.items.len(), 2);
        assert_eq!(plan.version, 3);
    }

    #[tokio::test]
    async fn mutate_task_dag_commits_done_and_bumps_version() {
        let handle = base_handle();
        let (client_tx, mut client_rx) = tokio::sync::mpsc::unbounded_channel();

        {
            let mut plan = crate::plan::VersionedPlan::new();
            plan.version = 2;
            let mut plans = handle.swarm_state().plans.write().await;
            plans.insert("swarm-1".to_string(), plan);
        }

        handle
            .mutate_task_dag(
                9,
                "swarm-1",
                "sess",
                "task_graph_seed",
                2,
                &client_tx,
                false, // existing plan
                |plan| {
                    plan.items.push(crate::plan::PlanItem {
                        content: "task".to_string(),
                        status: "todo".to_string(),
                        priority: "normal".to_string(),
                        id: "t-1".to_string(),
                        subsystem: None,
                        file_scope: Vec::new(),
                        blocked_by: Vec::new(),
                        assigned_to: None,
                    });
                    plan.version += 1;
                    Ok(())
                },
            )
            .await;

        // Done response emitted and version bumped by the transform.
        assert!(matches!(client_rx.try_recv(), Ok(ServerEvent::Done { id: 9 })));

        let plans = handle.swarm_state().plans.read().await;
        let plan = plans.get("swarm-1").expect("plan exists");
        assert_eq!(plan.version, 3);
        assert_eq!(plan.items.len(), 1);
    }

    #[tokio::test]
    async fn mutate_task_dag_error_emits_error_without_version_bump() {
        let handle = base_handle();
        let (client_tx, mut client_rx) = tokio::sync::mpsc::unbounded_channel();

        {
            let mut plan = crate::plan::VersionedPlan::new();
            plan.version = 1;
            let mut plans = handle.swarm_state().plans.write().await;
            plans.insert("swarm-1".to_string(), plan);
        }

        handle
            .mutate_task_dag(
                7,
                "swarm-1",
                "sess",
                "task_graph_complete",
                0,
                &client_tx,
                false,
                |_plan| Err("Complete rejected: actor does not own node".to_string()),
            )
            .await;

        assert!(matches!(
            client_rx.try_recv(),
            Ok(ServerEvent::Error { id: 7, .. })
        ));

        let plans = handle.swarm_state().plans.read().await;
        let plan = plans.get("swarm-1").expect("plan exists");
        assert_eq!(plan.version, 1, "failed transform must not bump the version");
    }

    #[tokio::test]
    async fn mutate_task_dag_create_if_missing_seeds_new_plan() {
        let handle = base_handle();
        let (client_tx, mut client_rx) = tokio::sync::mpsc::unbounded_channel();

        // No plan seeded; create_if_missing should insert one.
        handle
            .mutate_task_dag(
                11,
                "swarm-1",
                "sess",
                "task_graph_seed",
                1,
                &client_tx,
                true,
                |plan| {
                    plan.participants.insert("sess".to_string());
                    plan.version += 1;
                    Ok(())
                },
            )
            .await;

        assert!(matches!(
            client_rx.try_recv(),
            Ok(ServerEvent::Done { id: 11 })
        ));

        let plans = handle.swarm_state().plans.read().await;
        let plan = plans.get("swarm-1").expect("plan exists");
        assert_eq!(plan.version, 1);
        assert!(plan.participants.contains("sess"));
    }

    #[tokio::test]
    async fn elect_seeder_coordinator_promotes_seeder_and_demotes_prior() {
        let handle = base_handle();
        let mk = |id: &str| {
            let (event_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            SwarmMember {
                session_id: id.to_string(),
                event_tx,
                event_txs: HashMap::new(),
                working_dir: None,
                swarm_id: Some("swarm-1".to_string()),
                swarm_enabled: true,
                status: "ready".to_string(),
                detail: None,
                task_label: None,
                friendly_name: Some(id.to_string()),
                report_back_to_session_id: None,
                latest_completion_report: None,
                role: "agent".to_string(),
                joined_at: Instant::now(),
                last_status_change: Instant::now(),
                is_headless: false,
                output_tail: None,
                todo_progress: None,
                todo_items: Vec::new(),
                runtime: crate::protocol::SwarmMemberRuntime::default(),
            }
        };
        {
            let mut members = handle.swarm_state().members.write().await;
            members.insert("prior".to_string(), mk("prior"));
            members.insert("seed".to_string(), mk("seed"));
        }

        // A stale coordinator (prior) is present.
        {
            let mut coordinators = handle.swarm_state().coordinators.write().await;
            coordinators.insert("swarm-1".to_string(), "prior".to_string());
        }

        let became = handle.elect_seeder_coordinator("swarm-1", "seed").await;
        assert!(became);

        let coordinators = handle.swarm_state().coordinators.read().await;
        assert_eq!(coordinators.get("swarm-1").map(String::as_str), Some("seed"));
        drop(coordinators);

        let members = handle.swarm_state().members.read().await;
        assert_eq!(
            members.get("seed").map(|m| m.role.as_str()),
            Some("coordinator")
        );
        assert_eq!(
            members.get("prior").map(|m| m.role.as_str()),
            Some("agent"),
            "prior coordinator should be demoted"
        );
    }

    #[tokio::test]
    async fn elect_seeder_coordinator_leaves_live_coordinator_untouched() {
        let handle = base_handle();
        let (event_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let live = SwarmMember {
            session_id: "coord".to_string(),
            event_tx,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: Some("swarm-1".to_string()),
            swarm_enabled: true,
            status: "ready".to_string(),
            detail: None,
            task_label: None,
            friendly_name: Some("coord".to_string()),
            report_back_to_session_id: None,
            latest_completion_report: None,
            role: "coordinator".to_string(),
            joined_at: Instant::now(),
            last_status_change: Instant::now(),
            is_headless: false,
            output_tail: None,
            todo_progress: None,
            todo_items: Vec::new(),
            runtime: crate::protocol::SwarmMemberRuntime::default(),
        };
        {
            let mut members = handle.swarm_state().members.write().await;
            members.insert("coord".to_string(), live);
            members.insert("seed".to_string(), {
                let (event_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                SwarmMember {
                    session_id: "seed".to_string(),
                    event_tx,
                    event_txs: HashMap::new(),
                    working_dir: None,
                    swarm_id: Some("swarm-1".to_string()),
                    swarm_enabled: true,
                    status: "ready".to_string(),
                    detail: None,
                    task_label: None,
                    friendly_name: Some("seed".to_string()),
                    report_back_to_session_id: None,
                    latest_completion_report: None,
                    role: "agent".to_string(),
                    joined_at: Instant::now(),
                    last_status_change: Instant::now(),
                    is_headless: false,
                    output_tail: None,
                    todo_progress: None,
                    todo_items: Vec::new(),
                    runtime: crate::protocol::SwarmMemberRuntime::default(),
                }
            });
        }
        {
            let mut coordinators = handle.swarm_state().coordinators.write().await;
            coordinators.insert("swarm-1".to_string(), "coord".to_string());
        }

        // Live coordinator (open event channel, not headless) is left in place.
        let became = handle.elect_seeder_coordinator("swarm-1", "seed").await;
        assert!(!became);

        let coordinators = handle.swarm_state().coordinators.read().await;
        assert_eq!(coordinators.get("swarm-1").map(String::as_str), Some("coord"));
    }

    #[tokio::test]
    async fn register_visible_member_records_member_and_swarm_index() {
        let handle = base_handle();
        handle
            .register_visible_member("child-1", "swarm-1", None, false, Some("root"))
            .await;

        {
            let members = handle.swarm_state().members.read().await;
            let member = members.get("child-1").expect("member registered");
            assert!(!member.is_headless);
            assert_eq!(member.swarm_id.as_deref(), Some("swarm-1"));
            assert_eq!(member.status, "spawned");
            assert_eq!(
                member.report_back_to_session_id.as_deref(),
                Some("root")
            );
        }

        let swarms = handle.swarm_state().swarms_by_id.read().await;
        assert!(swarms.get("swarm-1").expect("swarm index").contains("child-1"));
    }

    #[tokio::test]
    async fn spawn_adds_plan_participants_adds_both_when_plan_nonempty() {
        let handle = base_handle();
        {
            let mut plan = crate::plan::VersionedPlan::new();
            plan.items.push(crate::plan::PlanItem {
                content: "task".to_string(),
                status: "todo".to_string(),
                priority: "normal".to_string(),
                id: "t-1".to_string(),
                subsystem: None,
                file_scope: Vec::new(),
                blocked_by: Vec::new(),
                assigned_to: None,
            });
            let mut plans = handle.swarm_state().plans.write().await;
            plans.insert("swarm-1".to_string(), plan);
        }

        handle
            .spawn_adds_plan_participants("swarm-1", "root", "child-1")
            .await;

        let plans = handle.swarm_state().plans.read().await;
        let plan = plans.get("swarm-1").expect("plan exists");
        assert!(plan.participants.contains("root"));
        assert!(plan.participants.contains("child-1"));
    }

    #[tokio::test]
    async fn spawn_adds_plan_participants_is_noop_for_empty_plan() {
        let handle = base_handle();
        // No plan exists: the handle must not create an empty participant set.
        handle
            .spawn_adds_plan_participants("swarm-1", "root", "child-1")
            .await;

        let plans = handle.swarm_state().plans.read().await;
        assert!(plans.get("swarm-1").is_none());
    }

    #[tokio::test]
    async fn promote_spawn_coordinator_claims_empty_slot() {
        let handle = base_handle();
        {
            let (event_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let mut members = handle.swarm_state().members.write().await;
            members.insert(
                "root".to_string(),
                SwarmMember {
                    session_id: "root".to_string(),
                    event_tx,
                    event_txs: HashMap::new(),
                    working_dir: None,
                    swarm_id: Some("swarm-1".to_string()),
                    swarm_enabled: true,
                    status: "ready".to_string(),
                    detail: None,
                    task_label: None,
                    friendly_name: Some("root".to_string()),
                    report_back_to_session_id: None,
                    latest_completion_report: None,
                    role: "agent".to_string(),
                    joined_at: Instant::now(),
                    last_status_change: Instant::now(),
                    is_headless: false,
                    output_tail: None,
                    todo_progress: None,
                    todo_items: Vec::new(),
                    runtime: crate::protocol::SwarmMemberRuntime::default(),
                },
            );
        }

        let promoted = handle
            .promote_spawn_coordinator("swarm-1", "root", false)
            .await;
        assert!(promoted);

        let coordinators = handle.swarm_state().coordinators.read().await;
        assert_eq!(coordinators.get("swarm-1").map(String::as_str), Some("root"));
        drop(coordinators);
        let members = handle.swarm_state().members.read().await;
        assert_eq!(
            members.get("root").map(|m| m.role.as_str()),
            Some("coordinator")
        );
    }

    #[tokio::test]
    async fn promote_spawn_coordinator_skips_live_coordinator() {
        let handle = base_handle();
        {
            let (event_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let mut members = handle.swarm_state().members.write().await;
            members.insert(
                "root".to_string(),
                SwarmMember {
                    session_id: "root".to_string(),
                    event_tx,
                    event_txs: HashMap::new(),
                    working_dir: None,
                    swarm_id: Some("swarm-1".to_string()),
                    swarm_enabled: true,
                    status: "ready".to_string(),
                    detail: None,
                    task_label: None,
                    friendly_name: Some("root".to_string()),
                    report_back_to_session_id: None,
                    latest_completion_report: None,
                    role: "agent".to_string(),
                    joined_at: Instant::now(),
                    last_status_change: Instant::now(),
                    is_headless: false,
                    output_tail: None,
                    todo_progress: None,
                    todo_items: Vec::new(),
                    runtime: crate::protocol::SwarmMemberRuntime::default(),
                },
            );
        }
        {
            let mut coordinators = handle.swarm_state().coordinators.write().await;
            coordinators.insert("swarm-1".to_string(), "other".to_string());
        }

        // A live (non-stale) coordinator is left in place.
        let promoted = handle
            .promote_spawn_coordinator("swarm-1", "root", false)
            .await;
        assert!(!promoted);
        let coordinators = handle.swarm_state().coordinators.read().await;
        assert_eq!(coordinators.get("swarm-1").map(String::as_str), Some("other"));
    }

    #[tokio::test]
    async fn move_member_between_swarms_rekeys_subscribed_member() {
        let handle = base_handle();
        // Seed a member in "swarm-a" plus a swarms_by_id index entry.
        {
            let (event_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let mut members = handle.swarm_state().members.write().await;
            members.insert(
                "sess".to_string(),
                SwarmMember {
                    session_id: "sess".to_string(),
                    event_tx,
                    event_txs: HashMap::new(),
                    working_dir: None,
                    swarm_id: Some("swarm-a".to_string()),
                    swarm_enabled: true,
                    status: "ready".to_string(),
                    detail: None,
                    task_label: None,
                    friendly_name: Some("sess".to_string()),
                    report_back_to_session_id: None,
                    latest_completion_report: None,
                    role: "agent".to_string(),
                    joined_at: Instant::now(),
                    last_status_change: Instant::now(),
                    is_headless: false,
                    output_tail: None,
                    todo_progress: None,
                    todo_items: Vec::new(),
                    runtime: crate::protocol::SwarmMemberRuntime::default(),
                },
            );
        }
        {
            let mut swarms = handle.swarm_state().swarms_by_id.write().await;
            swarms.insert(
                "swarm-a".to_string(),
                HashSet::from(["sess".to_string()]),
            );
        }

        let (old, new) = handle
            .move_member_between_swarms("sess", PathBuf::from("/work"), true)
            .await;
        assert_eq!(old.as_deref(), Some("swarm-a"));
        // A fresh (inserted) member re-keys to its session-scoped swarm id.
        assert_eq!(new.as_deref(), Some("session:sess"));

        let members = handle.swarm_state().members.read().await;
        let member = members.get("sess").expect("member");
        assert_eq!(member.swarm_id.as_deref(), Some("session:sess"));
        assert_eq!(
            member.working_dir.as_deref(),
            Some(std::path::Path::new("/work"))
        );
        assert_eq!(member.role, "agent");
        drop(members);

        let swarms = handle.swarm_state().swarms_by_id.read().await;
        assert!(swarms.get("swarm-a").is_none(), "old swarm pruned");
        assert!(swarms.get("session:sess").is_some_and(|s| s.contains("sess")));
    }

    #[tokio::test]
    async fn move_member_between_swarms_reelects_coordinator() {
        let handle = base_handle();
        // Two members in "swarm-a"; "coord" is both the coordinator and moving.
        for id in ["coord", "other"] {
            let (event_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let mut members = handle.swarm_state().members.write().await;
            members.insert(
                id.to_string(),
                SwarmMember {
                    session_id: id.to_string(),
                    event_tx,
                    event_txs: HashMap::new(),
                    working_dir: None,
                    swarm_id: Some("swarm-a".to_string()),
                    swarm_enabled: true,
                    status: "ready".to_string(),
                    detail: None,
                    task_label: None,
                    friendly_name: Some(id.to_string()),
                    report_back_to_session_id: None,
                    latest_completion_report: None,
                    role: "agent".to_string(),
                    joined_at: Instant::now(),
                    last_status_change: Instant::now(),
                    is_headless: false,
                    output_tail: None,
                    todo_progress: None,
                    todo_items: Vec::new(),
                    runtime: crate::protocol::SwarmMemberRuntime::default(),
                },
            );
        }
        {
            let mut swarms = handle.swarm_state().swarms_by_id.write().await;
            swarms.insert(
                "swarm-a".to_string(),
                HashSet::from(["coord".to_string(), "other".to_string()]),
            );
        }
        {
            let mut coordinators = handle.swarm_state().coordinators.write().await;
            coordinators.insert("swarm-a".to_string(), "coord".to_string());
        }

        let (old, _new) = handle
            .move_member_between_swarms("coord", PathBuf::from("/work"), true)
            .await;
        assert_eq!(old.as_deref(), Some("swarm-a"));

        // The coordinator slot was re-pointed away from the moved member.
        let coordinators = handle.swarm_state().coordinators.read().await;
        let coord = coordinators.get("swarm-a").expect("re-elected");
        assert_ne!(coord, "coord", "moved member must not keep the slot");
    }

    #[tokio::test]
    async fn assign_member_role_promotes_and_reports_requester() {
        let handle = base_handle();
        for id in ["root", "target"] {
            let (event_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let mut members = handle.swarm_state().members.write().await;
            members.insert(
                id.to_string(),
                SwarmMember {
                    session_id: id.to_string(),
                    event_tx,
                    event_txs: HashMap::new(),
                    working_dir: None,
                    swarm_id: Some("swarm-1".to_string()),
                    swarm_enabled: true,
                    status: "ready".to_string(),
                    detail: None,
                    task_label: None,
                    friendly_name: Some(id.to_string()),
                    report_back_to_session_id: None,
                    latest_completion_report: None,
                    role: "coordinator".to_string(),
                    joined_at: Instant::now(),
                    last_status_change: Instant::now(),
                    is_headless: false,
                    output_tail: None,
                    todo_progress: None,
                    todo_items: Vec::new(),
                    runtime: crate::protocol::SwarmMemberRuntime::default(),
                },
            );
        }
        let result = handle
            .assign_member_role("swarm-1", "target", "coordinator", "root")
            .await;
        assert!(result.is_ok());

        let coordinators = handle.swarm_state().coordinators.read().await;
        assert_eq!(coordinators.get("swarm-1").map(String::as_str), Some("target"));
        drop(coordinators);
        let members = handle.swarm_state().members.read().await;
        assert_eq!(
            members.get("target").map(|m| m.role.as_str()),
            Some("coordinator")
        );
        assert_eq!(
            members.get("root").map(|m| m.role.as_str()),
            Some("agent")
        );
    }

    #[tokio::test]
    async fn assign_member_role_errors_on_unknown_session() {
        let handle = base_handle();
        let result = handle
            .assign_member_role("swarm-1", "ghost", "coordinator", "root")
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn record_task_assignment_assigns_and_commits() {
        let handle = base_handle();
        {
            let mut plan = crate::plan::VersionedPlan::new();
            plan.items.push(crate::plan::PlanItem {
                id: "t-1".to_string(),
                content: "task".to_string(),
                status: "todo".to_string(),
                priority: "normal".to_string(),
                subsystem: None,
                file_scope: Vec::new(),
                blocked_by: Vec::new(),
                assigned_to: None,
            });
            let mut plans = handle.swarm_state().plans.write().await;
            plans.insert("swarm-1".to_string(), plan);
        }

        let (selected, content, participants, _count, _blocked) = handle
            .record_task_assignment("swarm-1", "root", |plan| {
                let idx = plan.items.iter().position(|i| i.id == "t-1").unwrap();
                plan.items[idx].assigned_to = Some("worker".to_string());
                plan.items[idx].status = "queued".to_string();
                plan.version += 1;
                plan.participants.insert("root".to_string());
                plan.participants.insert("worker".to_string());
                let c = plan.items[idx].content.clone();
                (
                    Some("t-1".to_string()),
                    Some(c),
                    plan.participants.clone(),
                    plan.items.len(),
                    None,
                )
            })
            .await;

        assert_eq!(selected.as_deref(), Some("t-1"));
        assert_eq!(content.as_deref(), Some("task"));
        assert!(participants.contains("worker"));

        let plans = handle.swarm_state().plans.read().await;
        assert_eq!(plans.get("swarm-1").unwrap().items[0].status, "queued");
    }

    #[tokio::test]
    async fn requeue_assignment_requeues_and_commits() {
        let handle = base_handle();
        {
            let mut plan = crate::plan::VersionedPlan::new();
            plan.items.push(crate::plan::PlanItem {
                id: "t-1".to_string(),
                content: "task".to_string(),
                status: "failed".to_string(),
                priority: "normal".to_string(),
                subsystem: None,
                file_scope: Vec::new(),
                blocked_by: Vec::new(),
                assigned_to: Some("worker".to_string()),
            });
            let mut plans = handle.swarm_state().plans.write().await;
            plans.insert("swarm-1".to_string(), plan);
        }

        let requeued = handle
            .requeue_assignment("swarm-1", "task_retry", |plan| {
                let item = plan.items.iter_mut().find(|i| i.id == "t-1")?;
                item.status = "queued".to_string();
                plan.version += 1;
                Some(())
            })
            .await;
        assert!(requeued);

        let plans = handle.swarm_state().plans.read().await;
        assert_eq!(plans.get("swarm-1").unwrap().items[0].status, "queued");
    }

    #[tokio::test]
    async fn mutate_task_status_transitions_and_commits() {
        let handle = base_handle();
        {
            let mut plan = crate::plan::VersionedPlan::new();
            plan.items.push(crate::plan::PlanItem {
                id: "t-1".to_string(),
                content: "task".to_string(),
                status: "queued".to_string(),
                priority: "normal".to_string(),
                subsystem: None,
                file_scope: Vec::new(),
                blocked_by: Vec::new(),
                assigned_to: Some("worker".to_string()),
            });
            let mut plans = handle.swarm_state().plans.write().await;
            plans.insert("swarm-1".to_string(), plan);
        }

        handle
            .mutate_task_status("swarm-1", "task_running", |plan| {
                let item = plan.items.iter_mut().find(|i| i.id == "t-1").unwrap();
                item.status = "running".to_string();
            })
            .await;

        let plans = handle.swarm_state().plans.read().await;
        assert_eq!(plans.get("swarm-1").unwrap().items[0].status, "running");
    }

    #[tokio::test]
    async fn mutate_task_disposition_autocompletes_and_commits() {
        let handle = base_handle();
        {
            let mut plan = crate::plan::VersionedPlan::new();
            plan.items.push(crate::plan::PlanItem {
                id: "t-1".to_string(),
                content: "task".to_string(),
                status: "running".to_string(),
                priority: "normal".to_string(),
                subsystem: None,
                file_scope: Vec::new(),
                blocked_by: Vec::new(),
                assigned_to: Some("worker".to_string()),
            });
            let mut plans = handle.swarm_state().plans.write().await;
            plans.insert("swarm-1".to_string(), plan);
        }

        handle
            .mutate_task_disposition("swarm-1", None, |plan| {
                let item = plan.items.iter_mut().find(|i| i.id == "t-1").unwrap();
                item.status = "done".to_string();
                "task_completed"
            })
            .await;

        let plans = handle.swarm_state().plans.read().await;
        assert_eq!(plans.get("swarm-1").unwrap().items[0].status, "done");
    }
}

