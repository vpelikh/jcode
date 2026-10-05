// End-to-end shape of the user-visible failure: a coordinator starts an async
// `await_members` with a short timeout while its worker is still mid-turn, then
// re-issues the same wait with a longer timeout before the first expires. The
// live watcher must honor the extension instead of firing "Timed out" at the
// original deadline, and must still deliver the completion when the worker
// finally reports.

#[tokio::test]
async fn async_await_extension_keeps_live_watcher_watching() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-extend";
    let requester = "req-extend";
    let peer = "peer-1";
    let await_runtime = AwaitMembersRuntime::default();

    let (client_tx, _client_rx) = mpsc::unbounded_channel();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), member(requester, swarm_id, "ready")),
        (peer.to_string(), member(peer, swarm_id, "running")),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([requester.to_string(), peer.to_string()]),
    )])));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);
    let swarm = crate::server::test_util::TestSwarmBuilder::default()
        .members(Arc::clone(&swarm_members))
        .swarms_by_id(Arc::clone(&swarms_by_id))
        .swarm_event_tx(swarm_event_tx.clone())
        .build();

    let mut bus_rx = crate::bus::Bus::global().subscribe();

    let target = vec![
        "ready".to_string(),
        "completed".to_string(),
        "stopped".to_string(),
    ];
    let key = crate::server::await_members_state::request_key(
        requester,
        swarm_id,
        &[],
        &target,
        None,
    );

    // Very short async wait (1s) while the worker is still running.
    handle_comm_await_members(
        1,
        requester.to_string(),
        target.clone(),
        vec![],
        None,
        Some(1),
        true,
        true,
        true,
        CommAwaitMembersContext {
            client_event_tx: &client_tx,
            swarm: &swarm,
            await_members_runtime: &await_runtime,
        },
    )
    .await;

    // Re-issue the same wait with a long timeout before the original expires.
    // With the fix this refreshes the persisted deadline; without it the live
    // watcher stays bound to the 1s deadline.
    handle_comm_await_members(
        2,
        requester.to_string(),
        target,
        vec![],
        None,
        Some(600),
        true,
        true,
        true,
        CommAwaitMembersContext {
            client_event_tx: &client_tx,
            swarm: &swarm,
            await_members_runtime: &await_runtime,
        },
    )
    .await;

    // Sleep past the *original* 1s deadline. Without the extension the watcher
    // would have finalized as timed out here.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let still_pending = crate::server::await_members_state::load_state(&key)
        .expect("persisted state remains")
        .final_response
        .is_none();
    assert!(
        still_pending,
        "extended live watcher must not finalize at the original deadline"
    );

    // Worker finishes after the original deadline: the extended watcher must
    // still be watching and deliver the completion.
    {
        let mut members = swarm_members.write().await;
        members.get_mut(peer).expect("peer exists").status = "completed".to_string();
    }
    let _ = swarm_event_tx.send(swarm_event(
        peer,
        swarm_id,
        SwarmEventType::StatusChange {
            old_status: "running".to_string(),
            new_status: "completed".to_string(),
        },
    ));

    let event = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match bus_rx.recv().await {
                Ok(crate::bus::BusEvent::SwarmAwaitCompleted(event))
                    if event.session_id == requester =>
                {
                    return event;
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("bus closed before SwarmAwaitCompleted arrived")
                }
            }
        }
    })
    .await
    .expect("extended live watcher should still deliver completion");

    assert!(
        event.completed,
        "extension should keep the wait alive until the worker reports"
    );
}
// Direct assertion on the persisted deadline: a re-issue while the wait is
// still active must move the deadline forward. Regression guard for the
// request-key reuse path that previously pinned every retry to the first
// call's deadline.
#[tokio::test]
async fn async_await_reissue_refreshes_persisted_deadline() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-reissue";
    let requester = "req-reissue";
    let peer = "peer-1";
    let await_runtime = AwaitMembersRuntime::default();

    let (client_tx, _client_rx) = mpsc::unbounded_channel();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), member(requester, swarm_id, "ready")),
        (peer.to_string(), member(peer, swarm_id, "running")),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([requester.to_string(), peer.to_string()]),
    )])));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);
    let swarm = crate::server::test_util::TestSwarmBuilder::default()
        .members(Arc::clone(&swarm_members))
        .swarms_by_id(Arc::clone(&swarms_by_id))
        .swarm_event_tx(swarm_event_tx.clone())
        .build();

    let target = vec![
        "ready".to_string(),
        "completed".to_string(),
        "stopped".to_string(),
    ];
    let key = crate::server::await_members_state::request_key(
        requester,
        swarm_id,
        &[],
        &target,
        None,
    );

    handle_comm_await_members(
        1,
        requester.to_string(),
        target.clone(),
        vec![],
        None,
        Some(60),
        true,
        false,
        false,
        CommAwaitMembersContext {
            client_event_tx: &client_tx,
            swarm: &swarm,
            await_members_runtime: &await_runtime,
        },
    )
    .await;
    let first = crate::server::await_members_state::load_state(&key)
        .expect("first await persists pending state");

    handle_comm_await_members(
        2,
        requester.to_string(),
        target,
        vec![],
        None,
        Some(600),
        true,
        false,
        false,
        CommAwaitMembersContext {
            client_event_tx: &client_tx,
            swarm: &swarm,
            await_members_runtime: &await_runtime,
        },
    )
    .await;
    let second = crate::server::await_members_state::load_state(&key)
        .expect("re-issued await keeps pending state");

    assert!(
        second.deadline_unix_ms > first.deadline_unix_ms + 60_000,
        "re-issued async await must extend the persisted deadline (first={}, second={})",
        first.deadline_unix_ms,
        second.deadline_unix_ms,
    );
}
