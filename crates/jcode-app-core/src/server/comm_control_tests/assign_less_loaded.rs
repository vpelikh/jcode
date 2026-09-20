#[tokio::test]
async fn assign_task_without_target_prefers_less_loaded_ready_agent() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-auto-target-load";
    let requester = "coord";
    let less_loaded = "worker-light";
    let more_loaded = "worker-busy";
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let soft_interrupt_queues = Arc::new(RwLock::new(HashMap::new()));
    let client_connections = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), {
            let mut member = member(requester, swarm_id, "ready");
            member.role = "coordinator".to_string();
            member
        }),
        (
            less_loaded.to_string(),
            owned_member(less_loaded, swarm_id, "ready", requester),
        ),
        (
            more_loaded.to_string(),
            owned_member(more_loaded, swarm_id, "ready", requester),
        ),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([
            requester.to_string(),
            less_loaded.to_string(),
            more_loaded.to_string(),
        ]),
    )])));
    let mut busy_existing = plan_item("busy-existing", "running", "high", &[]);
    busy_existing.assigned_to = Some(more_loaded.to_string());
    let swarm_plans = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        VersionedPlan {
            items: vec![
                plan_item("setup", "completed", "high", &[]),
                busy_existing,
                plan_item("next", "queued", "high", &["setup"]),
            ],
            version: 1,
            participants: HashSet::from([
                requester.to_string(),
                less_loaded.to_string(),
                more_loaded.to_string(),
            ]),
            task_progress: HashMap::new(),
            mode: "light".to_string(),
            node_meta: HashMap::new(),
        },
    )])));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        requester.to_string(),
    )])));
    let event_history = Arc::new(RwLock::new(VecDeque::new()));
    let event_counter = Arc::new(AtomicU64::new(1));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);
    let mutation_runtime = SwarmMutationRuntime::default();

    let session_h = session_handle(Arc::clone(&sessions), Arc::clone(&soft_interrupt_queues));
    let swarm = crate::server::test_util::TestSwarmBuilder::default()
        .members(Arc::clone(&swarm_members))
        .swarms_by_id(Arc::clone(&swarms_by_id))
        .plans(Arc::clone(&swarm_plans))
        .coordinators(Arc::clone(&swarm_coordinators))
        .event_history(Arc::clone(&event_history))
        .event_counter(Arc::clone(&event_counter))
        .swarm_event_tx(swarm_event_tx.clone())
        .swarm_mutation_runtime(mutation_runtime.clone())
        .build();
    handle_comm_assign_task(
        100,
        requester.to_string(),
        None,
        None,
        Some("Pick the least-loaded worker".to_string()),
        &client_tx,
        &session_h,
        &client_connections,
        &swarm,
    )
    .await;

    match client_rx.recv().await.expect("response") {
        ServerEvent::CommAssignTaskResponse {
            id,
            task_id,
            target_session,
        } => {
            assert_eq!(id, 100);
            assert_eq!(task_id, "next");
            assert_eq!(target_session, less_loaded);
        }
        other => panic!("expected CommAssignTaskResponse, got {other:?}"),
    }
}
