//! Shared test helpers for the server module.
//!
//! Consolidated here rather than in each test module so every swarm-handle
//! test builds a `SwarmServiceHandle` through one builder. Callers seed
//! whichever maps/sinks their test actually reaches; every field left unseeded
//! is an inert default.

use crate::plan::VersionedPlan;
use crate::server::services::SwarmServiceHandle;
use crate::server::{
    AwaitMembersRuntime, FileTouchService, SharedContext, SwarmEvent, SwarmMember,
    SwarmMutationRuntime, SwarmState,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tokio::sync::{RwLock, broadcast};

type MemberMap = Arc<RwLock<HashMap<String, SwarmMember>>>;
type SwarmGrouping = Arc<RwLock<HashMap<String, HashSet<String>>>>;
type PlanMap = Arc<RwLock<HashMap<String, VersionedPlan>>>;
type CoordinatorMap = Arc<RwLock<HashMap<String, String>>>;
type ChannelSubscriptions = Arc<RwLock<HashMap<String, HashMap<String, HashSet<String>>>>>;
type EventHistory = Arc<RwLock<VecDeque<SwarmEvent>>>;

/// A minimal `SwarmServiceHandle` builder for tests. Seed only the maps/sinks
/// the code under test reaches; the remaining fields fall back to inert
/// defaults. Because a `broadcast::Sender` can only be created from a live
/// channel, the default sender owns a channel nobody subscribes to.
#[derive(Default)]
#[allow(clippy::type_complexity)]
pub(crate) struct TestSwarmBuilder {
    members: Option<MemberMap>,
    swarms_by_id: Option<SwarmGrouping>,
    plans: Option<PlanMap>,
    coordinators: Option<CoordinatorMap>,
    channel_subscriptions: Option<ChannelSubscriptions>,
    channel_subscriptions_by_session: Option<ChannelSubscriptions>,
    event_history: Option<EventHistory>,
    event_counter: Option<Arc<AtomicU64>>,
    swarm_event_tx: Option<broadcast::Sender<SwarmEvent>>,
    swarm_mutation_runtime: Option<SwarmMutationRuntime>,
    shared_context: Option<Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>>,
}

#[allow(clippy::type_complexity)]
impl TestSwarmBuilder {
    pub(crate) fn members(mut self, v: MemberMap) -> Self {
        self.members = Some(v);
        self
    }

    pub(crate) fn swarms_by_id(mut self, v: SwarmGrouping) -> Self {
        self.swarms_by_id = Some(v);
        self
    }

    pub(crate) fn plans(mut self, v: PlanMap) -> Self {
        self.plans = Some(v);
        self
    }

    pub(crate) fn coordinators(mut self, v: CoordinatorMap) -> Self {
        self.coordinators = Some(v);
        self
    }

    pub(crate) fn channel_subscriptions(mut self, v: ChannelSubscriptions) -> Self {
        self.channel_subscriptions = Some(v);
        self
    }

    pub(crate) fn channel_subscriptions_by_session(mut self, v: ChannelSubscriptions) -> Self {
        self.channel_subscriptions_by_session = Some(v);
        self
    }

    pub(crate) fn event_history(mut self, v: EventHistory) -> Self {
        self.event_history = Some(v);
        self
    }

    pub(crate) fn event_counter(mut self, v: Arc<AtomicU64>) -> Self {
        self.event_counter = Some(v);
        self
    }

    pub(crate) fn swarm_event_tx(mut self, v: broadcast::Sender<SwarmEvent>) -> Self {
        self.swarm_event_tx = Some(v);
        self
    }

    pub(crate) fn swarm_mutation_runtime(mut self, v: SwarmMutationRuntime) -> Self {
        self.swarm_mutation_runtime = Some(v);
        self
    }

    pub(crate) fn shared_context(
        mut self,
        v: Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
    ) -> Self {
        self.shared_context = Some(v);
        self
    }

    pub(crate) fn build(self) -> SwarmServiceHandle {
        SwarmServiceHandle {
            swarm_state: SwarmState {
                members: self
                    .members
                    .unwrap_or_else(|| Arc::new(RwLock::new(HashMap::new()))),
                swarms_by_id: self
                    .swarms_by_id
                    .unwrap_or_else(|| Arc::new(RwLock::new(HashMap::new()))),
                plans: self
                    .plans
                    .unwrap_or_else(|| Arc::new(RwLock::new(HashMap::new()))),
                coordinators: self
                    .coordinators
                    .unwrap_or_else(|| Arc::new(RwLock::new(HashMap::new()))),
            },
            shared_context: self
                .shared_context
                .unwrap_or_else(|| Arc::new(RwLock::new(HashMap::new()))),
            file_touch: FileTouchService::new(),
            channel_subscriptions: self
                .channel_subscriptions
                .unwrap_or_else(|| Arc::new(RwLock::new(HashMap::new()))),
            channel_subscriptions_by_session: self
                .channel_subscriptions_by_session
                .unwrap_or_else(|| Arc::new(RwLock::new(HashMap::new()))),
            event_history: self
                .event_history
                .unwrap_or_else(|| Arc::new(RwLock::new(VecDeque::new()))),
            event_counter: self
                .event_counter
                .unwrap_or_else(|| Arc::new(AtomicU64::new(0))),
            swarm_event_tx: self
                .swarm_event_tx
                .unwrap_or_else(|| broadcast::channel(16).0),
            await_members_runtime: AwaitMembersRuntime::default(),
            swarm_mutation_runtime: self.swarm_mutation_runtime.unwrap_or_default(),
        }
    }
}
