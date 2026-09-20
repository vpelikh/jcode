#[tokio::test]
async fn assign_next_prefers_worker_with_dependency_context() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-context-score";
    let requester = "coord";
    let context_worker = "worker-context";
    let other_worker = "worker-other";
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
            context_worker.to_string(),
            owned_member(context_worker, swarm_id, "ready", requester),
        ),
        (
            other_worker.to_string(),
            owned_member(other_worker, swarm_id, "ready", requester),
        ),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([
            requester.to_string(),
            context_worker.to_string(),
            other_worker.to_string(),
        ]),
    )])));
    let mut dependency = plan_item("dep", "completed", "high", &[]);
    dependency.assigned_to = Some(context_worker.to_string());
    let swarm_plans = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        VersionedPlan {
            items: vec![dependency, plan_item("next", "queued", "high", &["dep"])],
            version: 1,
            participants: HashSet::from([
                requester.to_string(),
                context_worker.to_string(),
                other_worker.to_string(),
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
    let provider: Arc<dyn Provider> = Arc::new(TestProvider);
    let global_session_id = Arc::new(RwLock::new(String::new()));
    let mcp_pool = Arc::new(crate::mcp::SharedMcpPool::from_default_config());

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
    handle_comm_assign_next(
        102,
        requester.to_string(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        &client_tx,
        &session_h,
        &global_session_id,
        &provider,
        &client_connections,
        &swarm,
        &mcp_pool,
    )
    .await;

    match client_rx.recv().await.expect("response") {
        ServerEvent::CommAssignTaskResponse {
            id,
            task_id,
            target_session,
        } => {
            assert_eq!(id, 102);
            assert_eq!(task_id, "next");
            assert_eq!(target_session, context_worker);
        }
        other => panic!("expected CommAssignTaskResponse, got {other:?}"),
    }
}
