#[tokio::test]
async fn coordinator_can_assign_role_to_worker() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-role";
    let coordinator = "coord";
    let worker = "worker";
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let sessions = Arc::new(RwLock::new(HashMap::new()));

    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (coordinator.to_string(), {
            let mut member = member(coordinator, swarm_id, "ready");
            member.role = "coordinator".to_string();
            member
        }),
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
    let swarm = crate::server::test_util::TestSwarmBuilder::default()
        .members(Arc::clone(&swarm_members))
        .swarms_by_id(Arc::clone(&swarms_by_id))
        .coordinators(Arc::clone(&coordinators))
        .event_history(Arc::clone(&history))
        .event_counter(Arc::clone(&event_counter))
        .swarm_event_tx(swarm_event_tx.clone())
        .build();

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
        (coordinator.to_string(), {
            let mut member = member(coordinator, swarm_id, "ready");
            member.role = "coordinator".to_string();
            member
        }),
        (
            plain_member.to_string(),
            member(plain_member, swarm_id, "ready"),
        ),
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
    let swarm = crate::server::test_util::TestSwarmBuilder::default()
        .members(Arc::clone(&swarm_members))
        .swarms_by_id(Arc::clone(&swarms_by_id))
        .coordinators(Arc::clone(&coordinators))
        .event_history(Arc::clone(&history))
        .event_counter(Arc::clone(&event_counter))
        .swarm_event_tx(swarm_event_tx.clone())
        .build();

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
