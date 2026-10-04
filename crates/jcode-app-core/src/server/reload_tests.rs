use super::{
    graceful_shutdown_sessions, graceful_shutdown_sessions_with_timeout,
    persist_reload_recovery_intents, receive_reload_signal,
};
use crate::server::{ReloadSignal, SwarmEvent, SwarmEventType, SwarmMember};
use jcode_agent_runtime::InterruptSignal;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{RwLock, broadcast, mpsc, watch};

fn set_member_status(members: &mut HashMap<String, SwarmMember>, session_id: &str, status: &str) {
    assert!(
        members.contains_key(session_id),
        "missing test member {session_id}"
    );
    if let Some(member) = members.get_mut(session_id) {
        member.status = status.to_string();
    }
}

fn member(session_id: &str, status: &str) -> SwarmMember {
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    SwarmMember {
        session_id: session_id.to_string(),
        event_tx,
        event_txs: HashMap::new(),
        working_dir: None,
        swarm_id: None,
        swarm_enabled: false,
        status: status.to_string(),
        detail: None,
        task_label: None,
        friendly_name: None,
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
}

#[tokio::test]
async fn receive_reload_signal_consumes_already_pending_value() {
    let (tx, mut rx) = watch::channel(None::<ReloadSignal>);
    tx.send(Some(ReloadSignal {
        hash: "abc1234".to_string(),
        triggering_session: Some("sess-1".to_string()),
        prefer_selfdev_binary: true,
        request_id: "reload-1".to_string(),
    }))
    .expect("send pending reload signal");

    let signal = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        receive_reload_signal(&mut rx, &mut None),
    )
    .await
    .expect("pending signal should be observed immediately")
    .expect("channel should still be open");

    assert_eq!(signal.hash, "abc1234");
    assert_eq!(signal.triggering_session.as_deref(), Some("sess-1"));
    assert!(signal.prefer_selfdev_binary);
    assert_eq!(signal.request_id, "reload-1");
}

#[tokio::test]
async fn receive_reload_signal_waits_for_future_value_when_initially_empty() {
    let (tx, mut rx) = watch::channel(None::<ReloadSignal>);

    let waiter = tokio::spawn(async move { receive_reload_signal(&mut rx, &mut None).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    tx.send(Some(ReloadSignal {
        hash: "def5678".to_string(),
        triggering_session: Some("sess-2".to_string()),
        prefer_selfdev_binary: false,
        request_id: "reload-2".to_string(),
    }))
    .expect("send future reload signal");

    let signal = tokio::time::timeout(std::time::Duration::from_millis(100), waiter)
        .await
        .expect("future signal should wake waiter")
        .expect("waiter task should succeed")
        .expect("channel should still be open");

    assert_eq!(signal.hash, "def5678");
    assert_eq!(signal.triggering_session.as_deref(), Some("sess-2"));
    assert!(!signal.prefer_selfdev_binary);
    assert_eq!(signal.request_id, "reload-2");
}

#[test]
fn persist_reload_recovery_intents_records_running_peer_recovery() -> anyhow::Result<()> {
    let _guard = crate::storage::lock_test_env();
    let temp_home = tempfile::TempDir::new()?;
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp_home.path());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        ("initiator".to_string(), member("initiator", "running")),
        ("peer".to_string(), member("peer", "running")),
        ("idle".to_string(), member("idle", "ready")),
    ])));

    runtime.block_on(persist_reload_recovery_intents(
        "reload-store-test",
        &swarm_members,
        Some("initiator"),
    ));

    let peer_directive = crate::server::reload_recovery::pending_directive_for_session("peer")
        .expect("claim peer recovery")
        .expect("peer recovery intent should exist");
    assert!(
        peer_directive
            .continuation_message
            .contains("interrupted by a server reload")
    );
    assert!(
        crate::server::reload_recovery::pending_directive_for_session("idle")
            .expect("claim idle recovery")
            .is_none(),
        "idle sessions should not get reload recovery intents"
    );
    assert!(
        crate::server::reload_recovery::pending_directive_for_session("initiator")
            .expect("claim initiator recovery")
            .is_none(),
        "initiator without selfdev reload context should not get a generic interrupted-peer intent"
    );

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    Ok(())
}

#[tokio::test]
async fn graceful_shutdown_sessions_signals_all_running_sessions_including_initiator() {
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        ("initiator".to_string(), member("initiator", "running")),
        ("peer".to_string(), member("peer", "running")),
    ])));
    let initiator_signal = InterruptSignal::new();
    let peer_signal = InterruptSignal::new();
    let shutdown_signals = Arc::new(RwLock::new(HashMap::from([
        ("initiator".to_string(), initiator_signal.clone()),
        ("peer".to_string(), peer_signal.clone()),
    ])));
    let (swarm_event_tx, _) = broadcast::channel(8);
    let swarm_members_for_task = swarm_members.clone();
    let swarm_event_tx_for_task = swarm_event_tx.clone();

    let checkpoint_task = tokio::spawn(async move {
        tokio::task::yield_now().await;
        {
            let mut members = swarm_members_for_task.write().await;
            set_member_status(&mut members, "initiator", "ready");
            set_member_status(&mut members, "peer", "ready");
        }
        let _ = swarm_event_tx_for_task.send(SwarmEvent {
            id: 1,
            session_id: "initiator".to_string(),
            session_name: None,
            swarm_id: None,
            event: SwarmEventType::StatusChange {
                old_status: "running".to_string(),
                new_status: "ready".to_string(),
            },
            timestamp: Instant::now(),
            absolute_time: std::time::SystemTime::now(),
        });
        let _ = swarm_event_tx_for_task.send(SwarmEvent {
            id: 2,
            session_id: "peer".to_string(),
            session_name: None,
            swarm_id: None,
            event: SwarmEventType::StatusChange {
                old_status: "running".to_string(),
                new_status: "ready".to_string(),
            },
            timestamp: Instant::now(),
            absolute_time: std::time::SystemTime::now(),
        });
    });

    graceful_shutdown_sessions(
        "test-reload",
        &sessions,
        &swarm_members,
        &shutdown_signals,
        &swarm_event_tx,
        None,
    )
    .await;
    checkpoint_task.await.expect("checkpoint task");

    assert!(
        initiator_signal.is_set(),
        "initiating selfdev session should also be interrupted so reload tool cannot hang"
    );
    assert!(
        peer_signal.is_set(),
        "other running sessions should be interrupted too"
    );
}

#[tokio::test]
async fn graceful_shutdown_sessions_does_not_wait_for_triggering_session_checkpoint() {
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        ("initiator".to_string(), member("initiator", "running")),
        ("peer".to_string(), member("peer", "running")),
    ])));
    let initiator_signal = InterruptSignal::new();
    let peer_signal = InterruptSignal::new();
    let shutdown_signals = Arc::new(RwLock::new(HashMap::from([
        ("initiator".to_string(), initiator_signal.clone()),
        ("peer".to_string(), peer_signal.clone()),
    ])));
    let (swarm_event_tx, _) = broadcast::channel(8);
    let swarm_members_for_task = swarm_members.clone();
    let swarm_event_tx_for_task = swarm_event_tx.clone();

    let checkpoint_task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        {
            let mut members = swarm_members_for_task.write().await;
            set_member_status(&mut members, "peer", "ready");
        }
        let _ = swarm_event_tx_for_task.send(SwarmEvent {
            id: 1,
            session_id: "peer".to_string(),
            session_name: None,
            swarm_id: None,
            event: SwarmEventType::StatusChange {
                old_status: "running".to_string(),
                new_status: "ready".to_string(),
            },
            timestamp: Instant::now(),
            absolute_time: std::time::SystemTime::now(),
        });
    });

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        graceful_shutdown_sessions(
            "test-reload",
            &sessions,
            &swarm_members,
            &shutdown_signals,
            &swarm_event_tx,
            Some("initiator"),
        ),
    )
    .await
    .expect("reload shutdown should not wait for triggering session");
    checkpoint_task.await.expect("checkpoint task");

    assert!(
        initiator_signal.is_set(),
        "triggering session should still receive graceful shutdown signal"
    );
    assert!(
        peer_signal.is_set(),
        "peer session should still receive graceful shutdown signal"
    );
    assert_eq!(
        swarm_members
            .read()
            .await
            .get("initiator")
            .expect("initiator")
            .status,
        "running",
        "initiator may remain running without blocking reload"
    );
}

#[tokio::test]
async fn graceful_shutdown_sessions_skips_idle_sessions() {
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        "idle".to_string(),
        member("idle", "ready"),
    )])));
    let idle_signal = InterruptSignal::new();
    let shutdown_signals = Arc::new(RwLock::new(HashMap::from([(
        "idle".to_string(),
        idle_signal.clone(),
    )])));
    let (swarm_event_tx, _) = broadcast::channel(8);

    graceful_shutdown_sessions(
        "test-reload",
        &sessions,
        &swarm_members,
        &shutdown_signals,
        &swarm_event_tx,
        None,
    )
    .await;

    assert!(
        !idle_signal.is_set(),
        "idle sessions should not be interrupted during reload"
    );
}

#[tokio::test]
async fn graceful_shutdown_sessions_does_not_wait_on_running_sessions_without_signal() {
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        "orphan_running".to_string(),
        member("orphan_running", "running"),
    )])));
    let shutdown_signals = Arc::new(RwLock::new(HashMap::new()));
    let (swarm_event_tx, _) = broadcast::channel(8);

    let started = Instant::now();
    graceful_shutdown_sessions(
        "test-reload",
        &sessions,
        &swarm_members,
        &shutdown_signals,
        &swarm_event_tx,
        None,
    )
    .await;

    assert!(
        started.elapsed() < std::time::Duration::from_millis(100),
        "running sessions without shutdown signals should not consume the reload grace period"
    );
}

#[tokio::test]
async fn graceful_shutdown_sessions_waits_until_target_status_change_arrives() {
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        "target".to_string(),
        member("target", "running"),
    )])));
    let signal = InterruptSignal::new();
    let shutdown_signals = Arc::new(RwLock::new(HashMap::from([(
        "target".to_string(),
        signal.clone(),
    )])));
    let (swarm_event_tx, _) = broadcast::channel(8);

    let mut waiter = tokio::spawn({
        let sessions = sessions.clone();
        let swarm_members = swarm_members.clone();
        let shutdown_signals = shutdown_signals.clone();
        let swarm_event_tx = swarm_event_tx.clone();
        async move {
            graceful_shutdown_sessions(
                "test-reload",
                &sessions,
                &swarm_members,
                &shutdown_signals,
                &swarm_event_tx,
                None,
            )
            .await;
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        signal.is_set(),
        "running target should be interrupted promptly"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut waiter)
            .await
            .is_err(),
        "reload shutdown should stay pending until target leaves running"
    );

    {
        let mut members = swarm_members.write().await;
        set_member_status(&mut members, "target", "ready");
    }
    let _ = swarm_event_tx.send(SwarmEvent {
        id: 1,
        session_id: "target".to_string(),
        session_name: None,
        swarm_id: None,
        event: SwarmEventType::StatusChange {
            old_status: "running".to_string(),
            new_status: "ready".to_string(),
        },
        timestamp: Instant::now(),
        absolute_time: std::time::SystemTime::now(),
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
        .await
        .expect("waiter should complete after target checkpoint")
        .expect("waiter task should succeed");
}

#[tokio::test]
async fn graceful_shutdown_sessions_ignores_unrelated_events_until_target_leaves() {
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        ("target".to_string(), member("target", "running")),
        ("other".to_string(), member("other", "running")),
    ])));
    let signal = InterruptSignal::new();
    let shutdown_signals = Arc::new(RwLock::new(HashMap::from([("target".to_string(), signal)])));
    let (swarm_event_tx, _) = broadcast::channel(8);

    let mut waiter = tokio::spawn({
        let sessions = sessions.clone();
        let swarm_members = swarm_members.clone();
        let shutdown_signals = shutdown_signals.clone();
        let swarm_event_tx = swarm_event_tx.clone();
        async move {
            graceful_shutdown_sessions(
                "test-reload",
                &sessions,
                &swarm_members,
                &shutdown_signals,
                &swarm_event_tx,
                None,
            )
            .await;
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    {
        let mut members = swarm_members.write().await;
        set_member_status(&mut members, "other", "ready");
    }
    let _ = swarm_event_tx.send(SwarmEvent {
        id: 1,
        session_id: "other".to_string(),
        session_name: None,
        swarm_id: None,
        event: SwarmEventType::StatusChange {
            old_status: "running".to_string(),
            new_status: "ready".to_string(),
        },
        timestamp: Instant::now(),
        absolute_time: std::time::SystemTime::now(),
    });

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut waiter)
            .await
            .is_err(),
        "unrelated status changes should not unblock reload shutdown"
    );

    {
        let mut members = swarm_members.write().await;
        set_member_status(&mut members, "target", "stopped");
    }
    let _ = swarm_event_tx.send(SwarmEvent {
        id: 2,
        session_id: "target".to_string(),
        session_name: None,
        swarm_id: None,
        event: SwarmEventType::StatusChange {
            old_status: "running".to_string(),
            new_status: "stopped".to_string(),
        },
        timestamp: Instant::now(),
        absolute_time: std::time::SystemTime::now(),
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
        .await
        .expect("waiter should complete after target transition")
        .expect("waiter task should succeed");
}

#[tokio::test]
async fn graceful_shutdown_sessions_treats_member_left_as_unblocked() {
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        "target".to_string(),
        member("target", "running"),
    )])));
    let signal = InterruptSignal::new();
    let shutdown_signals = Arc::new(RwLock::new(HashMap::from([("target".to_string(), signal)])));
    let (swarm_event_tx, _) = broadcast::channel(8);

    let waiter = tokio::spawn({
        let sessions = sessions.clone();
        let swarm_members = swarm_members.clone();
        let shutdown_signals = shutdown_signals.clone();
        let swarm_event_tx = swarm_event_tx.clone();
        async move {
            graceful_shutdown_sessions(
                "test-reload",
                &sessions,
                &swarm_members,
                &shutdown_signals,
                &swarm_event_tx,
                None,
            )
            .await;
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    {
        let mut members = swarm_members.write().await;
        members.remove("target");
    }
    let _ = swarm_event_tx.send(SwarmEvent {
        id: 1,
        session_id: "target".to_string(),
        session_name: None,
        swarm_id: None,
        event: SwarmEventType::MemberChange {
            action: "left".to_string(),
        },
        timestamp: Instant::now(),
        absolute_time: std::time::SystemTime::now(),
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
        .await
        .expect("waiter should complete after member leaves")
        .expect("waiter task should succeed");
}

#[tokio::test]
async fn graceful_shutdown_sessions_times_out_and_proceeds() {
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        "target".to_string(),
        member("target", "running"),
    )])));
    let signal = InterruptSignal::new();
    let shutdown_signals = Arc::new(RwLock::new(HashMap::from([(
        "target".to_string(),
        signal.clone(),
    )])));
    let (swarm_event_tx, _) = broadcast::channel(8);

    let started = Instant::now();
    graceful_shutdown_sessions_with_timeout(
        "test-reload",
        &sessions,
        &swarm_members,
        &shutdown_signals,
        &swarm_event_tx,
        std::time::Duration::from_millis(50),
        None,
    )
    .await;

    assert!(
        signal.is_set(),
        "running target should still be signaled promptly"
    );
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(50)
            && started.elapsed() < std::time::Duration::from_millis(250),
        "graceful shutdown should honor the timeout instead of waiting indefinitely"
    );
}

#[tokio::test]
async fn graceful_shutdown_sessions_times_out_on_partial_checkpoint() {
    // One watched session checkpoints, the other never does. The wait must
    // still terminate at the timeout instead of blocking on the laggard.
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        ("fast".to_string(), member("fast", "running")),
        ("slow".to_string(), member("slow", "running")),
    ])));
    let fast_signal = InterruptSignal::new();
    let slow_signal = InterruptSignal::new();
    let shutdown_signals = Arc::new(RwLock::new(HashMap::from([
        ("fast".to_string(), fast_signal.clone()),
        ("slow".to_string(), slow_signal.clone()),
    ])));
    let (swarm_event_tx, _) = broadcast::channel(8);

    let swarm_members_for_task = swarm_members.clone();
    let swarm_event_tx_for_task = swarm_event_tx.clone();
    let checkpoint_task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        {
            let mut members = swarm_members_for_task.write().await;
            set_member_status(&mut members, "fast", "ready");
        }
        let _ = swarm_event_tx_for_task.send(SwarmEvent {
            id: 1,
            session_id: "fast".to_string(),
            session_name: None,
            swarm_id: None,
            event: SwarmEventType::StatusChange {
                old_status: "running".to_string(),
                new_status: "ready".to_string(),
            },
            timestamp: Instant::now(),
            absolute_time: std::time::SystemTime::now(),
        });
        // "slow" intentionally never leaves running.
    });

    let started = Instant::now();
    graceful_shutdown_sessions_with_timeout(
        "test-reload",
        &sessions,
        &swarm_members,
        &shutdown_signals,
        &swarm_event_tx,
        std::time::Duration::from_millis(120),
        None,
    )
    .await;
    checkpoint_task.await.expect("checkpoint task");

    assert!(fast_signal.is_set() && slow_signal.is_set());
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(120)
            && started.elapsed() < std::time::Duration::from_millis(600),
        "partial checkpoint must still honor the timeout, elapsed={:?}",
        started.elapsed()
    );
    assert_eq!(
        swarm_members.read().await.get("slow").expect("slow").status,
        "running",
        "the laggard session may remain running without blocking reload past the deadline"
    );
}

/// Integration: the exact user-facing outcome of the reload path. A blocking
/// session that owns a still-running background task must, after
/// `persist_reload_recovery_intents`, produce a recovery directive whose
/// continuation message names that task. This drives the note end to end through
/// the same public seam the client reads (`pending_directive_for_session`) and
/// exercises the scan-once snapshot against real files on disk.
#[test]
fn persist_reload_recovery_intents_names_a_persisted_running_task() -> anyhow::Result<()> {
    let _guard = crate::storage::lock_test_env();
    let temp_home = tempfile::TempDir::new()?;
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp_home.path());

    // A detached, still-running background task owned by the "peer" session.
    // The pid is this test process so `is_process_running` reports true.
    let self_pid = std::process::id();
    let status = crate::background::TaskStatusFile {
        task_id: "reload-bg-1".to_string(),
        tool_name: "bash".to_string(),
        display_name: None,
        session_id: "peer".to_string(),
        status: crate::bus::BackgroundTaskStatus::Running,
        exit_code: None,
        error: None,
        started_at: chrono::Utc::now().to_rfc3339(),
        completed_at: None,
        duration_secs: None,
        pid: Some(self_pid),
        owner_pid: Some(self_pid),
        owner_instance: None,
        detached: true,
        notify: true,
        wake: false,
        progress: None,
        event_history: Vec::new(),
        stall_wake_seconds: None,
    };
    std::fs::write(
        crate::background::global().status_path_for("reload-bg-1"),
        serde_json::to_string_pretty(&status)?,
    )?;

    // A reload context is required for the peer to receive a continuation
    // message that carries the background note. Save it before persisting.
    crate::tool::selfdev::ReloadContext {
        task_context: None,
        version_before: "v0".to_string(),
        version_after: "v1".to_string(),
        session_id: "peer".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    }
    .save()?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        ("initiator".to_string(), member("initiator", "running")),
        ("peer".to_string(), member("peer", "running")),
    ])));
    runtime.block_on(persist_reload_recovery_intents(
        "reload-bg-scan-once",
        &swarm_members,
        Some("initiator"),
    ));

    let directive = crate::server::reload_recovery::pending_directive_for_session("peer")
        .expect("claim peer recovery")
        .expect("peer recovery intent should exist");
    assert!(
        directive.continuation_message.contains("reload-bg-1"),
        "the continuation message must name the still-running background task: {}",
        directive.continuation_message
    );

    let _ = std::fs::remove_file(crate::background::global().status_path_for("reload-bg-1"));
    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    Ok(())
}

/// Scale/edge integration for the scan-once reload note: many running sessions
/// each owning a distinct still-running background task must each get a
/// directive naming *its own* task. This is the regime where the old per-session
/// scan cost O(sessions x files); the snapshot must group by session correctly.
#[test]
fn persist_reload_recovery_intents_groups_many_sessions_by_own_task() -> anyhow::Result<()> {
    let _guard = crate::storage::lock_test_env();
    let temp_home = tempfile::TempDir::new()?;
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp_home.path());

    const N: usize = 25;
    let self_pid = std::process::id();
    let mut members = HashMap::new();
    members.insert("initiator".to_string(), member("initiator", "running"));
    for i in 0..N {
        let sid = format!("peer-{i}");
        members.insert(sid.clone(), member(&sid, "running"));

        let task_id = format!("bg-task-{i}");
        let status = crate::background::TaskStatusFile {
            task_id: task_id.clone(),
            tool_name: "bash".to_string(),
            display_name: None,
            session_id: sid.clone(),
            status: crate::bus::BackgroundTaskStatus::Running,
            exit_code: None,
            error: None,
            started_at: chrono::Utc::now().to_rfc3339(),
            completed_at: None,
            duration_secs: None,
            pid: Some(self_pid),
            owner_pid: Some(self_pid),
            owner_instance: None,
            detached: true,
            notify: true,
            wake: false,
            progress: None,
            event_history: Vec::new(),
            stall_wake_seconds: None,
        };
        std::fs::write(
            crate::background::global().status_path_for(&task_id),
            serde_json::to_string_pretty(&status)?,
        )?;

        // Each peer needs a reload context to receive a continuation message.
        crate::tool::selfdev::ReloadContext {
            task_context: None,
            version_before: "v0".to_string(),
            version_after: "v1".to_string(),
            session_id: sid,
            timestamp: chrono::Utc::now().to_rfc3339(),
        }
        .save()?;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let swarm_members = Arc::new(RwLock::new(members));
    runtime.block_on(persist_reload_recovery_intents(
        "reload-many-sessions",
        &swarm_members,
        Some("initiator"),
    ));

    for i in 0..N {
        let sid = format!("peer-{i}");
        let directive = crate::server::reload_recovery::pending_directive_for_session(&sid)
            .expect("claim peer recovery")
            .unwrap_or_else(|| panic!("peer {i} should have a recovery intent"));
        assert!(
            directive
                .continuation_message
                .contains(&format!("bg-task-{i}")),
            "peer {i} must name its own task, got: {}",
            directive.continuation_message
        );
        // It must not leak another session's task.
        let other = (i + 1) % N;
        assert!(
            !directive
                .continuation_message
                .contains(&format!("bg-task-{other}")),
            "peer {i} must not name peer {other}'s task"
        );
    }

    // Clean up the shared bg-task files this test created.
    for i in 0..N {
        let _ = std::fs::remove_file(
            crate::background::global().status_path_for(&format!("bg-task-{i}")),
        );
    }

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    Ok(())
}

/// An empty reload (no running/triggering sessions) must do no bg-task/await
/// directory IO. The snapshot is now collected lazily on the first candidate, so
/// the scan that prunes stale await state never runs here. A stale await file (a
/// scan would delete it as a side effect) must therefore survive, and the intent
/// store must stay empty.
#[test]
fn persist_reload_recovery_intents_with_no_candidates_performs_no_scan() -> anyhow::Result<()> {
    let _guard = crate::storage::lock_test_env();
    let temp_home = tempfile::TempDir::new()?;
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp_home.path());
    let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
    crate::env::set_var("JCODE_RUNTIME_DIR", temp_home.path().join("run"));

    // A stale await-state file: a directory scan would prune it (its deadline is
    // far past the pending TTL). Its survival is the "no scan happened" signal.
    let stale = crate::server::await_members_state::PersistedAwaitMembersState {
        key: "stale-await".to_string(),
        session_id: "ns-none".to_string(),
        swarm_id: "swarm-x".to_string(),
        target_status: vec!["ready".to_string()],
        requested_ids: vec!["w1".to_string()],
        mode: None,
        created_at_unix_ms: 1,
        // deadline far in the past -> is_stale() is true.
        deadline_unix_ms: 1,
        background: false,
        notify: true,
        wake: true,
        final_response: None,
    };
    crate::server::await_members_state::save_state(&stale);
    let stale_path = crate::server::await_members_state::state_path_for_key("stale-await");
    assert!(
        stale_path.exists(),
        "fixture must have written the stale file"
    );

    // No running members and no triggering session -> candidates is empty.
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        "idle".to_string(),
        member("idle", "ready"),
    )])));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(persist_reload_recovery_intents(
        "reload-empty",
        &swarm_members,
        None,
    ));

    assert!(
        stale_path.exists(),
        "an empty reload must not scan (and prune) the await directory"
    );
    assert!(
        crate::server::reload_recovery::pending_directive_for_session("ns-none")
            .expect("read store")
            .is_none(),
        "an empty reload must persist no recovery intent"
    );

    let _ = std::fs::remove_file(&stale_path);
    if let Some(prev_runtime) = prev_runtime {
        crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
    } else {
        crate::env::remove_var("JCODE_RUNTIME_DIR");
    }
    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    Ok(())
}
