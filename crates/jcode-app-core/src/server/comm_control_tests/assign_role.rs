/// A minimal swarm service handle mirroring `test_swarm_status_handle` in the
/// live-turn tests, but seeded with pre-existing coordinator membership so a
/// test can exercise `handle_comm_assign_role`'s authorization and mutation
/// paths end to end. The returned handle shares the provided maps.
#[allow(clippy::too_many_arguments)]
fn assign_role_swarm_handle(
    swarm_members: &Arc<RwLock<HashMap<String, crate::server::SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    coordinators: &Arc<RwLock<HashMap<String, String>>>,
    event_history: &Arc<RwLock<VecDeque<crate::server::SwarmEvent>>>,
    event_counter: &Arc<std::sync::atomic::AtomicU64>,
    swarm_event_tx: &broadcast::Sender<crate::server::SwarmEvent>,
) -> crate::server::services::SwarmServiceHandle {
    crate::server::services::SwarmServiceHandle {
        swarm_state: SwarmState {
            members: Arc::clone(swarm_members),
            swarms_by_id: Arc::clone(swarms_by_id),
            plans: Arc::new(RwLock::new(HashMap::new())),
            coordinators: Arc::clone(coordinators),
        },
        shared_context: Arc::new(RwLock::new(HashMap::new())),
        file_touch: FileTouchService::new(),
        channel_subscriptions: Arc::new(RwLock::new(HashMap::new())),
        channel_subscriptions_by_session: Arc::new(RwLock::new(HashMap::new())),
        event_history: Arc::clone(event_history),
        event_counter: Arc::clone(event_counter),
        swarm_event_tx: swarm_event_tx.clone(),
        await_members_runtime: AwaitMembersRuntime::default(),
        swarm_mutation_runtime: SwarmMutationRuntime::default(),
    }
}

#[tokio::test]
async fn coordinator_can_assign_role_to_worker() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-role";
    let coordinator = "coord";
    let worker = "worker";
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let sessions = Arc::new(RwLock::new(HashMap::new()));

    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (
            coordinator.to_string(),
            {
                let mut member = member(coordinator, swarm_id, "ready");
                member.role = "coordinator".to_string();
                member
            },
        ),
        (worker.to_string(), member(worker, swarm_id, "ready")),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([coordinator.to_string(), worker.to_string()]),
    )])));
    let coordinators = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        coordinator.to_string(),
    )])));
    let history = Arc::new(RwLock::new(VecDeque::new()));
    let event_counter = Arc::new(AtomicU64::new(0));
    let (swarm_event_tx, _ev_rx) = broadcast::channel(16);
    let swarm = assign_role_swarm_handle(
        &swarm_members,
        &swarms_by_id,
        &coordinators,
        &history,
        &event_counter,
        &swarm_event_tx,
    );

    handle_comm_assign_role(
        1,
        coordinator.to_string(),
        worker.to_string(),
        "co_lead".to_string(),
        &client_tx,
        &sessions,
        &swarm,
    )
    .await;

    // The handled mutation rewrote only the target member's role.
    let members = swarm_members.read().await;
    assert_eq!(members.get(worker).unwrap().role, "co_lead");
    assert_eq!(members.get(coordinator).unwrap().role, "coordinator");
    drop(members);

    // The coordinator mapping still points at the coordinator session.
    let coords = coordinators.read().await;
    assert_eq!(coords.get(swarm_id).unwrap(), "coord");
    drop(coords);

    // The requesting coordinator sees a Done for the mutation.
    let mut saw_done = false;
    while let Ok(event) = client_rx.try_recv() {
        if matches!(event, ServerEvent::Done { .. }) {
            saw_done = true;
        }
    }
    assert!(saw_done, "coordinator role assignment should Ack");

    // The role_assignment notification was recorded through the handle.
    let recorded = history.read().await;
    let role_event = recorded.iter().find(|event| {
        event.swarm_id.as_deref() == Some(swarm_id)
            && matches!(
                &event.event,
                crate::server::SwarmEventType::Notification {
                    notification_type,
                    ..
                } if notification_type == "role_assignment"
            )
    });
    assert!(
        role_event.is_some(),
        "expected a role_assignment notification event in the swarm history"
    );
}

#[tokio::test]
async fn non_coordinator_is_rejected() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-role-reject";
    let coordinator = "coord";
    let plain_member = "member";
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let sessions = Arc::new(RwLock::new(HashMap::new()));

    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (
            coordinator.to_string(),
            {
                let mut member = member(coordinator, swarm_id, "ready");
                member.role = "coordinator".to_string();
                member
            },
        ),
        (plain_member.to_string(), member(plain_member, swarm_id, "ready")),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([coordinator.to_string(), plain_member.to_string()]),
    )])));
    let coordinators = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        coordinator.to_string(),
    )])));
    let history = Arc::new(RwLock::new(VecDeque::new()));
    let event_counter = Arc::new(AtomicU64::new(0));
    let (swarm_event_tx, _ev_rx) = broadcast::channel(16);
    let swarm = assign_role_swarm_handle(
        &swarm_members,
        &swarms_by_id,
        &coordinators,
        &history,
        &event_counter,
        &swarm_event_tx,
    );

    handle_comm_assign_role(
        2,
        plain_member.to_string(),
        coordinator.to_string(),
        "coordinator".to_string(),
        &client_tx,
        &sessions,
        &swarm,
    )
    .await;

    // The coordinator's role was preserved because a non-coordinator may not
    // reassign roles.
    let members = swarm_members.read().await;
    assert_eq!(members.get(coordinator).unwrap().role, "coordinator");
    drop(members);

    // The rejected caller sees an Error, not a Done.
    let mut saw_error = false;
    while let Ok(event) = client_rx.try_recv() {
        if matches!(&event, ServerEvent::Error { .. }) {
            saw_error = true;
        }
    }
    assert!(saw_error, "non-coordinator assignment should be rejected");
}