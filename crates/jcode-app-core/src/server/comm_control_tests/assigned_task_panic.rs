// A worker whose assigned turn panics must still publish a terminal member
// status. `spawn_assigned_task_run` wraps the turn in
// `AssertUnwindSafe(..).catch_unwind()`, so a panic is converted into an
// ordinary turn error and handled like any other turn failure (member ->
// `failed`, task -> `task_failed`) instead of the task aborting and leaving the
// member stuck at `running` forever.

#[tokio::test]
async fn spawn_assigned_task_panic_publishes_failed_member_status() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-panic";
    let requester = "coord";
    let worker = "worker";
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
        (worker.to_string(), member(worker, swarm_id, "ready")),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([requester.to_string(), worker.to_string()]),
    )])));
    let swarm_plans = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        VersionedPlan {
            items: vec![plan_item("next", "queued", "high", &[])],
            version: 1,
            participants: HashSet::from([requester.to_string(), worker.to_string()]),
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

    // Register the worker's session with a provider that panics on `complete`.
    {
        let mut sessions_guard = sessions.write().await;
        let agent = test_agent_with_provider(Arc::new(PanicProvider)).await;
        sessions_guard.insert(worker.to_string(), agent);
    }

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
        1,
        requester.to_string(),
        Some(worker.to_string()),
        None,
        Some("Pick the next task".to_string()),
        &client_tx,
        &session_h,
        &client_connections,
        &swarm,
    )
    .await;
    let _ = client_rx.recv().await; // drop the assign-task response

    // Wait for the spawned turn task to resolve. The panicking provider makes
    // the turn abort; the guard must publish `failed` on the member promptly.
    let worker_member = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let members = swarm_members.read().await;
            let status = members
                .get(worker)
                .map(|member| member.status.clone())
                .unwrap_or_default();
            drop(members);
            if status == "failed" {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("panicking assigned turn must publish a terminal failed member status");

    assert_eq!(worker_member, "failed");
}