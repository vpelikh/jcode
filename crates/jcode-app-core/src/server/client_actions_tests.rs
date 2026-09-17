#![cfg_attr(test, allow(clippy::await_holding_lock))]

use super::{
    NotifySessionContext, clone_split_session, handle_notify_session, handle_rename_session,
    handle_resume_all_sessions, handle_set_feature, handle_set_handoff_resume,
    handle_set_working_dir, handle_split,
};
use crate::agent::Agent;
use crate::message::{ContentBlock, Message, Role, StreamEvent, ToolDefinition};
use crate::protocol::{FeatureToggle, ServerEvent};
use crate::provider::{EventStream, Provider};
use crate::server::{ClientConnectionInfo, SwarmMember};
use crate::tool::Registry;
use anyhow::Result;
use async_stream::stream;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::time::{Duration, timeout};

#[allow(clippy::type_complexity)]
fn empty_swarm_status_state() -> (
    Arc<RwLock<HashMap<String, std::collections::HashSet<String>>>>,
    Arc<RwLock<std::collections::VecDeque<crate::server::SwarmEvent>>>,
    Arc<std::sync::atomic::AtomicU64>,
    tokio::sync::broadcast::Sender<crate::server::SwarmEvent>,
) {
    let (swarm_event_tx, _) = tokio::sync::broadcast::channel(16);
    (
        Arc::new(RwLock::new(HashMap::new())),
        Arc::new(RwLock::new(std::collections::VecDeque::new())),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        swarm_event_tx,
    )
}

/// A minimal session service handle for tests that only exercise the soft-interrupt
/// delivery path. The other handle fields are inert defaults.
fn session_handle_for_test(
    sessions: crate::server::SessionAgents,
    soft_interrupt_queues: crate::server::SessionInterruptQueues,
) -> crate::server::services::SessionServiceHandle {
    crate::server::services::SessionServiceHandle {
        sessions,
        session_id: Arc::new(RwLock::new(String::new())),
        is_processing: Arc::new(RwLock::new(false)),
        shutdown_signals: Arc::new(RwLock::new(HashMap::new())),
        soft_interrupt_queues,
    }
}

struct MockProvider;

#[derive(Clone, Default)]
struct StreamingMockProvider {
    responses: Arc<StdMutex<VecDeque<Vec<StreamEvent>>>>,
}

impl StreamingMockProvider {
    fn queue_response(&self, events: Vec<StreamEvent>) {
        self.responses.lock().unwrap().push_back(events);
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _messages: &[crate::message::Message],
        _tools: &[crate::message::ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Err(anyhow::anyhow!(
            "mock provider complete should not be called in client_actions tests"
        ))
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(MockProvider)
    }
}

#[async_trait]
impl Provider for StreamingMockProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let events = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
        let stream = stream! {
            for event in events {
                yield Ok(event);
            }
        };
        Ok(Box::pin(stream))
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[test]
fn clone_split_session_uses_persisted_session_state() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let mut parent = crate::session::Session::create_with_id(
        "session_parent_split_test".to_string(),
        None,
        None,
    );
    parent.working_dir = Some("/tmp/jcode-split-test".to_string());
    parent.model = Some("gpt-test".to_string());
    parent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "hello from parent".to_string(),
            cache_control: None,
        }],
    );
    parent.compaction = Some(crate::session::StoredCompactionState {
        summary_text: "summary".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 1,
        original_turn_count: 1,
        compacted_count: 1,
        physically_consolidated: false,
    });
    parent.save().expect("save parent");

    let mut unsaved_parent = parent.clone();
    unsaved_parent.model = Some("unsaved-model".into());
    unsaved_parent.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "unfinished turn".into(),
            cache_control: None,
        }],
    );
    let (child_id, _child_name) =
        clone_split_session(&parent.id, Some(&unsaved_parent)).expect("clone split");
    let child = crate::session::Session::load(&child_id).expect("load child");

    assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
    assert_eq!(
        child.messages.len(),
        parent.messages.len() + 1,
        "fork should inherit the transcript plus one fork notice"
    );
    assert_eq!(
        child.messages[0].content_preview(),
        parent.messages[0].content_preview()
    );
    let fork_notice = child.messages.last().expect("fork notice message");
    assert_eq!(
        fork_notice.display_role,
        Some(crate::session::StoredDisplayRole::System),
        "fork notice must be hidden from the visible transcript"
    );
    let fork_notice_text = fork_notice.content_preview();
    assert!(
        fork_notice_text.contains("forked") && fork_notice_text.contains(parent.id.as_str()),
        "fork notice should mention the parent session: {fork_notice_text}"
    );
    assert_eq!(child.compaction, parent.compaction);
    assert_eq!(child.working_dir, parent.working_dir);
    assert_eq!(child.model, parent.model);
    assert_eq!(child.status, crate::session::SessionStatus::Closed);
    assert_ne!(child.id, parent.id);

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

struct SplitTestHome {
    _directory: tempfile::TempDir,
    previous_home: Option<std::ffi::OsString>,
}

impl SplitTestHome {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("split test home");
        let previous_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", directory.path());
        Self {
            _directory: directory,
            previous_home,
        }
    }
}

impl Drop for SplitTestHome {
    fn drop(&mut self) {
        if let Some(home) = &self.previous_home {
            crate::env::set_var("JCODE_HOME", home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }
}

async fn new_split_test_agent() -> Arc<Mutex<Agent>> {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    Arc::new(Mutex::new(Agent::new_with_initial_working_dir(
        provider,
        registry,
        Some("/project/empty-split"),
    )))
}

fn split_response(
    rx: &mut mpsc::UnboundedReceiver<ServerEvent>,
    request_id: u64,
) -> crate::session::Session {
    let event = rx.try_recv().expect("split must respond");
    let ServerEvent::SplitResponse {
        id,
        new_session_id,
        new_session_name,
    } = event
    else {
        panic!("expected SplitResponse, got {event:?}");
    };
    assert_eq!(id, request_id);
    assert!(!new_session_name.is_empty());
    assert!(rx.try_recv().is_err(), "exactly one split response");
    crate::session::Session::load(&new_session_id).expect("fork must be persisted for attachment")
}

#[tokio::test]
async fn split_busy_session_uses_persisted_state_without_waiting_for_agent() {
    let _guard = crate::storage::lock_test_env();
    let _home = SplitTestHome::new();
    let agent = new_split_test_agent().await;
    let mut busy = agent.lock().await;
    let mut parent = busy.session_for_split().clone();
    parent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "persisted request".into(),
            cache_control: None,
        }],
    );
    parent.save().expect("save pre-turn snapshot");
    busy.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "unsaved streaming output".into(),
            cache_control: None,
        }],
    );
    let (tx, mut rx) = mpsc::unbounded_channel();

    timeout(
        Duration::from_millis(100),
        handle_split(18, &parent.id, &agent, &tx),
    )
    .await
    .expect("split must not wait on the held streaming Agent lock");
    let child = split_response(&mut rx, 18);
    assert_eq!(child.messages.len(), parent.messages.len() + 1);
    assert_eq!(
        child.messages[0].content_preview(),
        parent.messages[0].content_preview()
    );
    assert!(
        !child
            .messages
            .iter()
            .any(|m| m.content_preview().contains("unsaved streaming output"))
    );
    assert!(
        child
            .messages
            .last()
            .unwrap()
            .content_preview()
            .contains("forked")
    );
    assert!(
        agent.try_lock().is_err(),
        "parent lock is still owned by the busy turn"
    );
    drop(busy);
}

#[test]
fn split_missing_parent_never_uses_another_live_session() {
    let _guard = crate::storage::lock_test_env();
    let _home = SplitTestHome::new();
    let other = crate::session::Session::create(None, None);
    assert!(clone_split_session("session_missing_parent", Some(&other)).is_err());
}

#[test]
fn split_corrupt_persisted_parent_is_not_hidden_by_live_fallback() {
    let _guard = crate::storage::lock_test_env();
    let _home = SplitTestHome::new();
    let mut parent = crate::session::Session::create(None, Some("persisted parent".into()));
    parent.save().expect("create snapshot");
    let path = crate::session::session_path(&parent.id).unwrap();
    std::fs::write(&path, b"invalid session JSON").unwrap();
    assert!(clone_split_session(&parent.id, Some(&parent)).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"invalid session JSON");
}

#[tokio::test]
async fn enabling_swarm_does_not_auto_elect_coordinator() {
    // Agent construction persists metadata, so it must share the test-home lock.
    let _guard = crate::storage::lock_test_env();
    let _home = SplitTestHome::new();
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    let (member_event_tx, _member_event_rx) = mpsc::unbounded_channel();
    let now = Instant::now();
    let session_id = "session_test_swarm_toggle";
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        session_id.to_string(),
        crate::server::SwarmMember {
            session_id: session_id.to_string(),
            event_tx: member_event_tx,
            event_txs: HashMap::new(),
            working_dir: Some(PathBuf::from("/tmp/jcode-passive-swarm")),
            swarm_id: None,
            swarm_enabled: false,
            status: "ready".to_string(),
            detail: None,
            task_label: None,
            friendly_name: Some("duck".to_string()),
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
    )])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::<String, HashSet<String>>::new()));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::<String, String>::new()));
    let swarm_plans = Arc::new(RwLock::new(HashMap::new()));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();
    let mut swarm_enabled = false;

    let (swarm_event_tx, _swarm_event_rx) = tokio::sync::broadcast::channel(16);
    let swarm_handle = crate::server::test_util::TestSwarmBuilder::default()
        .members(Arc::clone(&swarm_members))
        .swarms_by_id(Arc::clone(&swarms_by_id))
        .plans(Arc::clone(&swarm_plans))
        .coordinators(Arc::clone(&swarm_coordinators))
        .swarm_event_tx(swarm_event_tx)
        .build();

    handle_set_feature(
        42,
        FeatureToggle::Swarm,
        true,
        &agent,
        session_id,
        &Some("duck".to_string()),
        &mut swarm_enabled,
        &swarm_handle,
        &client_event_tx,
    )
    .await;

    assert!(swarm_enabled);
    assert!(swarm_coordinators.read().await.is_empty());
    assert_eq!(
        swarm_members
            .read()
            .await
            .get(session_id)
            .and_then(|member| member.swarm_id.clone())
            .as_deref(),
        // `swarm_id_for_session` derives the swarm id from the session id (see
        // `default_swarm_id_for_session`); enabling the feature on a session ids
        // it into the `session:<session_id>` swarm, not its working directory.
        Some("session:session_test_swarm_toggle")
    );
    assert_eq!(
        swarm_members
            .read()
            .await
            .get(session_id)
            .map(|member| member.role.as_str()),
        Some("agent")
    );

    let events: Vec<_> = std::iter::from_fn(|| client_event_rx.try_recv().ok()).collect();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id: 42 }))
    );
    assert!(events.iter().all(|event| {
        !matches!(
            event,
            ServerEvent::Notification { message, .. }
                if message == "You are the coordinator for this swarm."
        )
    }));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn rename_session_event_uses_agent_session_id_even_when_client_id_is_stale() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    let agent_session_id = agent.lock().await.session_id().to_string();
    let stale_client_session_id = "session_stale_client_id";
    let (member_event_tx, mut member_event_rx) = mpsc::unbounded_channel();
    let now = Instant::now();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        stale_client_session_id.to_string(),
        SwarmMember {
            session_id: stale_client_session_id.to_string(),
            event_tx: member_event_tx,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: None,
            swarm_enabled: false,
            status: "ready".to_string(),
            detail: None,
            task_label: None,
            friendly_name: Some("stale".to_string()),
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
    )])));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    handle_rename_session(
        99,
        Some("Release planning".to_string()),
        &agent,
        stale_client_session_id,
        &crate::server::test_util::TestSwarmBuilder::default()
            .members(Arc::clone(&swarm_members))
            .build(),
        &client_event_tx,
    )
    .await;

    let rename_event = timeout(Duration::from_secs(2), member_event_rx.recv())
        .await
        .expect("rename event should arrive")
        .expect("member event channel should stay open");
    match rename_event {
        ServerEvent::SessionRenamed {
            session_id,
            title,
            display_title,
        } => {
            assert_eq!(session_id, agent_session_id);
            assert_eq!(title.as_deref(), Some("Release planning"));
            assert_eq!(display_title, "Release planning");
        }
        other => panic!("expected SessionRenamed, got {other:?}"),
    }

    let client_events: Vec<_> = std::iter::from_fn(|| client_event_rx.try_recv().ok()).collect();
    assert!(
        client_events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id } if *id == 99))
    );
    let loaded = crate::session::Session::load(&agent_session_id).expect("renamed session saved");
    assert_eq!(loaded.custom_title.as_deref(), Some("Release planning"));

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[tokio::test]
async fn notify_session_runs_scheduled_task_immediately_for_idle_live_session() {
    // Agent construction persists metadata, so it must share the test-home lock.
    let _guard = crate::storage::lock_test_env();
    let _home = SplitTestHome::new();
    let provider = Arc::new(StreamingMockProvider::default());
    provider.queue_response(vec![
        StreamEvent::TextDelta("Working on scheduled task.".to_string()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);
    let provider_dyn: Arc<dyn Provider> = provider.clone();
    let registry = Registry::new(provider_dyn.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider_dyn, registry)));
    let session_id = agent.lock().await.session_id().to_string();
    let sessions = Arc::new(RwLock::new(HashMap::<String, Arc<Mutex<Agent>>>::from([(
        session_id.clone(),
        agent.clone(),
    )])));
    let soft_interrupt_queues = Arc::new(RwLock::new(HashMap::new()));
    let client_connections = Arc::new(RwLock::new(HashMap::from([(
        "client-1".to_string(),
        ClientConnectionInfo {
            client_id: "client-1".to_string(),
            session_id: session_id.clone(),
            client_instance_id: None,
            debug_client_id: Some("debug-1".to_string()),
            connected_at: Instant::now(),
            last_seen: Instant::now(),
            is_processing: false,
            current_tool_name: None,
            terminal_env: Vec::new(),
            disconnect_tx: mpsc::unbounded_channel().0,
        },
    )])));
    let (member_event_tx, mut member_event_rx) = mpsc::unbounded_channel();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        session_id.clone(),
        SwarmMember {
            session_id: session_id.clone(),
            event_tx: member_event_tx,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: None,
            swarm_enabled: false,
            status: "ready".to_string(),
            detail: None,
            task_label: None,
            friendly_name: Some("otter".to_string()),
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
    )])));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    let (swarms_by_id, event_history, event_counter, swarm_event_tx) = empty_swarm_status_state();
    let session_service =
        session_handle_for_test(Arc::clone(&sessions), Arc::clone(&soft_interrupt_queues));
    handle_notify_session(
        77,
        session_id.clone(),
        "[Scheduled task]\nTask: Follow up".to_string(),
        NotifySessionContext {
            session: &session_service,
            client_connections: &client_connections,
            swarm: &crate::server::test_util::TestSwarmBuilder::default()
                .members(Arc::clone(&swarm_members))
                .swarms_by_id(Arc::clone(&swarms_by_id))
                .event_history(Arc::clone(&event_history))
                .event_counter(Arc::clone(&event_counter))
                .swarm_event_tx(swarm_event_tx.clone())
                .build(),
            client_event_tx: &client_event_tx,
        },
    )
    .await;

    let streamed_event = timeout(Duration::from_secs(2), async {
        loop {
            match member_event_rx.recv().await {
                Some(ServerEvent::TextDelta { text })
                    if text.contains("Working on scheduled task.") =>
                {
                    return text;
                }
                Some(_) => continue,
                None => panic!("live member stream closed before scheduled task ran"),
            }
        }
    })
    .await
    .expect("scheduled task should start streaming promptly");
    assert!(streamed_event.contains("Working on scheduled task."));

    let client_events: Vec<_> = std::iter::from_fn(|| client_event_rx.try_recv().ok()).collect();
    assert!(
        client_events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id } if *id == 77))
    );

    let guard = agent.lock().await;
    assert!(guard.messages().iter().any(|message| {
        message.role == Role::User
            && message.display_role == Some(crate::session::StoredDisplayRole::System)
            && message
                .content_preview()
                .contains("[Scheduled task] Task: Follow up")
    }));
    assert!(guard.messages().iter().any(|message| {
        message.role == Role::Assistant
            && message
                .content_preview()
                .contains("Working on scheduled task.")
    }));
}

#[tokio::test]
async fn notify_session_queues_soft_interrupt_when_live_session_is_busy() {
    // Agent construction persists metadata, so it must share the test-home lock.
    let _guard = crate::storage::lock_test_env();
    let _home = SplitTestHome::new();
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    let session_id = agent.lock().await.session_id().to_string();
    let queue = agent.lock().await.soft_interrupt_queue();

    let sessions = Arc::new(RwLock::new(HashMap::<String, Arc<Mutex<Agent>>>::from([(
        session_id.clone(),
        agent.clone(),
    )])));
    let soft_interrupt_queues = Arc::new(RwLock::new(HashMap::from([(
        session_id.clone(),
        queue.clone(),
    )])));
    let client_connections = Arc::new(RwLock::new(HashMap::from([(
        "client-1".to_string(),
        ClientConnectionInfo {
            client_id: "client-1".to_string(),
            session_id: session_id.clone(),
            client_instance_id: None,
            debug_client_id: Some("debug-1".to_string()),
            connected_at: Instant::now(),
            last_seen: Instant::now(),
            is_processing: false,
            current_tool_name: None,
            terminal_env: Vec::new(),
            disconnect_tx: mpsc::unbounded_channel().0,
        },
    )])));
    let (member_event_tx, mut member_event_rx) = mpsc::unbounded_channel();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        session_id.clone(),
        SwarmMember {
            session_id: session_id.clone(),
            event_tx: member_event_tx,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: None,
            swarm_enabled: false,
            status: "running".to_string(),
            detail: None,
            task_label: None,
            friendly_name: Some("otter".to_string()),
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
    )])));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    let _busy_guard = agent.lock().await;

    let (swarms_by_id, event_history, event_counter, swarm_event_tx) = empty_swarm_status_state();
    let session_service =
        session_handle_for_test(Arc::clone(&sessions), Arc::clone(&soft_interrupt_queues));
    handle_notify_session(
        88,
        session_id.clone(),
        "[Scheduled task]\nTask: Follow up while busy".to_string(),
        NotifySessionContext {
            session: &session_service,
            client_connections: &client_connections,
            swarm: &crate::server::test_util::TestSwarmBuilder::default()
                .members(Arc::clone(&swarm_members))
                .swarms_by_id(Arc::clone(&swarms_by_id))
                .event_history(Arc::clone(&event_history))
                .event_counter(Arc::clone(&event_counter))
                .swarm_event_tx(swarm_event_tx.clone())
                .build(),
            client_event_tx: &client_event_tx,
        },
    )
    .await;

    let member_event = timeout(Duration::from_secs(2), member_event_rx.recv())
        .await
        .expect("notification should arrive promptly")
        .expect("live member should receive notification");
    match member_event {
        ServerEvent::Notification {
            from_session,
            from_name,
            message,
            ..
        } => {
            assert_eq!(from_session, "schedule");
            assert_eq!(from_name.as_deref(), Some("scheduled task"));
            assert!(message.contains("Task: Follow up while busy"));
        }
        other => panic!("expected notification event, got {other:?}"),
    }

    let queued = queue.lock().unwrap();
    assert_eq!(
        queued.len(),
        1,
        "scheduled task should queue as soft interrupt"
    );
    assert!(queued[0].content.contains("Task: Follow up while busy"));
    drop(queued);

    let client_events: Vec<_> = std::iter::from_fn(|| client_event_rx.try_recv().ok()).collect();
    assert!(
        client_events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id } if *id == 88))
    );
}

/// Build a live SwarmMember with a real client attachment so the resume-all
/// sweep treats it as live. Returns the member and the receiver for events
/// fanned out to that attachment.
fn live_member(session_id: &str) -> (SwarmMember, mpsc::UnboundedReceiver<ServerEvent>) {
    let (attach_tx, attach_rx) = mpsc::unbounded_channel();
    let member = SwarmMember {
        session_id: session_id.to_string(),
        event_tx: mpsc::unbounded_channel().0,
        event_txs: HashMap::from([("client-1".to_string(), attach_tx)]),
        working_dir: None,
        swarm_id: None,
        swarm_enabled: false,
        status: "ready".to_string(),
        detail: None,
        task_label: None,
        friendly_name: Some("otter".to_string()),
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
    };
    (member, attach_rx)
}

#[tokio::test]
async fn resume_all_continues_interrupted_idle_live_session() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let provider = Arc::new(StreamingMockProvider::default());
    provider.queue_response(vec![
        StreamEvent::TextDelta("Continuing where I left off.".to_string()),
        StreamEvent::MessageEnd { stop_reason: None },
    ]);
    let provider_dyn: Arc<dyn Provider> = provider.clone();
    let registry = Registry::new(provider_dyn.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider_dyn, registry)));
    let session_id = {
        let mut guard = agent.lock().await;
        // Leave the session with a pending user turn the assistant never answered
        // (simulating a turn that errored / was interrupted mid-generation).
        guard.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "please keep going on the refactor".to_string(),
                cache_control: None,
            }],
        );
        guard.session_id().to_string()
    };

    let sessions = Arc::new(RwLock::new(HashMap::<String, Arc<Mutex<Agent>>>::from([(
        session_id.clone(),
        agent.clone(),
    )])));
    let (member, mut attach_rx) = live_member(&session_id);
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(session_id.clone(), member)])));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    let (swarms_by_id, event_history, event_counter, swarm_event_tx) = empty_swarm_status_state();
    handle_resume_all_sessions(
        91,
        &sessions,
        &crate::server::test_util::TestSwarmBuilder::default()
            .members(Arc::clone(&swarm_members))
            .swarms_by_id(Arc::clone(&swarms_by_id))
            .event_history(Arc::clone(&event_history))
            .event_counter(Arc::clone(&event_counter))
            .swarm_event_tx(swarm_event_tx.clone())
            .build(),
        &client_event_tx,
    )
    .await;

    // The session should resume and stream the continuation.
    let streamed = timeout(Duration::from_secs(2), async {
        loop {
            match attach_rx.recv().await {
                Some(ServerEvent::TextDelta { text })
                    if text.contains("Continuing where I left off.") =>
                {
                    return text;
                }
                Some(_) => continue,
                None => panic!("live attachment closed before continuation streamed"),
            }
        }
    })
    .await
    .expect("interrupted session should resume promptly");
    assert!(streamed.contains("Continuing where I left off."));

    // The requesting client receives a summary describing one resumed session.
    let result = timeout(Duration::from_secs(2), async {
        loop {
            match client_event_rx.recv().await {
                Some(event @ ServerEvent::ResumeAllResult { .. }) => return event,
                Some(_) => continue,
                None => panic!("client channel closed before resume-all result"),
            }
        }
    })
    .await
    .expect("resume-all result should be emitted");
    match result {
        ServerEvent::ResumeAllResult {
            id,
            resumed,
            skipped,
            ..
        } => {
            assert_eq!(id, 91);
            assert_eq!(resumed, 1);
            assert_eq!(skipped, 0);
        }
        other => panic!("expected ResumeAllResult, got {other:?}"),
    }

    if let Some(home) = prev_home {
        crate::env::set_var("JCODE_HOME", home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[tokio::test]
async fn resume_all_skips_session_with_completed_turn() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    let session_id = {
        let mut guard = agent.lock().await;
        // A completed turn: last visible message is from the assistant.
        guard.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: "do the thing".to_string(),
                cache_control: None,
            }],
        );
        guard.add_message(
            Role::Assistant,
            vec![ContentBlock::Text {
                text: "done".to_string(),
                cache_control: None,
            }],
        );
        guard.session_id().to_string()
    };

    let sessions = Arc::new(RwLock::new(HashMap::<String, Arc<Mutex<Agent>>>::from([(
        session_id.clone(),
        agent.clone(),
    )])));
    let (member, _attach_rx) = live_member(&session_id);
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(session_id.clone(), member)])));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    let (swarms_by_id, event_history, event_counter, swarm_event_tx) = empty_swarm_status_state();
    handle_resume_all_sessions(
        92,
        &sessions,
        &crate::server::test_util::TestSwarmBuilder::default()
            .members(Arc::clone(&swarm_members))
            .swarms_by_id(Arc::clone(&swarms_by_id))
            .event_history(Arc::clone(&event_history))
            .event_counter(Arc::clone(&event_counter))
            .swarm_event_tx(swarm_event_tx.clone())
            .build(),
        &client_event_tx,
    )
    .await;

    let result = timeout(Duration::from_secs(2), async {
        loop {
            match client_event_rx.recv().await {
                Some(event @ ServerEvent::ResumeAllResult { .. }) => return event,
                Some(_) => continue,
                None => panic!("client channel closed before resume-all result"),
            }
        }
    })
    .await
    .expect("resume-all result should be emitted");
    match result {
        ServerEvent::ResumeAllResult {
            id,
            resumed,
            skipped,
            ..
        } => {
            assert_eq!(id, 92);
            assert_eq!(resumed, 0);
            assert_eq!(skipped, 1);
        }
        other => panic!("expected ResumeAllResult, got {other:?}"),
    }

    if let Some(home) = prev_home {
        crate::env::set_var("JCODE_HOME", home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[tokio::test]
async fn set_working_dir_updates_agent_and_fans_out_event() -> Result<()> {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let old_dir = tempfile::tempdir().expect("old dir").keep();
    let new_dir = tempfile::tempdir().expect("new dir").keep();

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    {
        let mut guard = agent.lock().await;
        guard.set_working_dir(old_dir.to_str().expect("utf8"));
    }
    let agent_session_id = agent.lock().await.session_id().to_string();
    let (member_event_tx, mut member_event_rx) = mpsc::unbounded_channel();
    let now = Instant::now();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        agent_session_id.clone(),
        SwarmMember {
            session_id: agent_session_id.clone(),
            event_tx: member_event_tx,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: None,
            swarm_enabled: false,
            status: "ready".to_string(),
            detail: None,
            task_label: None,
            friendly_name: None,
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
    )])));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    handle_set_working_dir(
        88,
        new_dir.to_str().expect("utf8").to_string(),
        &agent,
        &agent_session_id,
        &crate::server::test_util::TestSwarmBuilder::default()
            .members(Arc::clone(&swarm_members))
            .build(),
        &client_event_tx,
    )
    .await;

    let changed_event = timeout(Duration::from_secs(2), member_event_rx.recv())
        .await
        .expect("working dir change event should arrive")
        .expect("member event channel should stay open");
    match changed_event {
        ServerEvent::SessionWorkingDirChanged {
            session_id,
            working_dir,
        } => {
            assert_eq!(session_id, agent_session_id);
            assert_eq!(
                PathBuf::from(working_dir),
                new_dir.canonicalize().expect("canonical"),
                "event must carry the resolved (canonical) new working dir"
            );
        }
        other => panic!("expected SessionWorkingDirChanged, got {other:?}"),
    }

    let client_events: Vec<_> = std::iter::from_fn(|| client_event_rx.try_recv().ok()).collect();
    assert!(
        client_events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id } if *id == 88))
    );

    let guard = agent.lock().await;
    assert_eq!(
        guard.working_dir().map(PathBuf::from),
        Some(new_dir.canonicalize().expect("canonical")),
        "agent working dir must be updated"
    );
    drop(guard);

    // The swarm member record must be kept coherent with the new bound dir.
    let member_dir = swarm_members.read().await;
    let member = member_dir
        .get(&agent_session_id)
        .expect("swarm member exists");
    assert_eq!(
        member.working_dir.as_ref(),
        Some(&new_dir.canonicalize().expect("canonical")),
        "swarm member working_dir must be synced to the /cd target"
    );

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    Ok(())
}

#[tokio::test]
async fn set_working_dir_to_current_dir_is_noop() -> Result<()> {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let current = tempfile::tempdir().expect("dir").keep();
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    {
        let mut guard = agent.lock().await;
        guard.set_working_dir(current.to_str().expect("utf8"));
    }
    let agent_session_id = agent.lock().await.session_id().to_string();
    let swarm_members = Arc::new(RwLock::new(HashMap::<String, SwarmMember>::new()));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    // Same directory (canonicalizes to the current working dir): no-op, so
    // neither a change event nor a Done is emitted.
    handle_set_working_dir(
        77,
        current.to_str().expect("utf8").to_string(),
        &agent,
        &agent_session_id,
        &crate::server::test_util::TestSwarmBuilder::default()
            .members(Arc::clone(&swarm_members))
            .build(),
        &client_event_tx,
    )
    .await;

    tokio::time::sleep(Duration::from_millis(50)).await;
    let events: Vec<_> = std::iter::from_fn(|| client_event_rx.try_recv().ok()).collect();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id } if *id == 77)),
        "a no-op /cd must still resolve the request with a Done, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ServerEvent::SessionWorkingDirChanged { .. })),
        "a no-op /cd to the current dir must not emit a change event, got {events:?}"
    );
    let guard = agent.lock().await;
    assert_eq!(guard.working_dir().map(PathBuf::from), Some(current));

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    Ok(())
}

#[tokio::test]
async fn set_working_dir_noop_detects_canonically_equivalent_dir() -> Result<()> {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    // Store a *non-canonical* working dir (a `sub/..` round trip) so the
    // stored string differs lexically from the canonical target, exercising the
    // canonicalization fallback in the no-op comparison rather than the trivial
    // exact-string match.
    let root = tempfile::tempdir().expect("root dir");
    let sub = root.path().join("sub");
    std::fs::create_dir_all(&sub).expect("create sub");
    let stored_non_canonical = root.path().join("sub/..");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    {
        let mut guard = agent.lock().await;
        guard.set_working_dir(stored_non_canonical.to_str().expect("utf8"));
    }
    let agent_session_id = agent.lock().await.session_id().to_string();
    let swarm_members = Arc::new(RwLock::new(HashMap::<String, SwarmMember>::new()));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    // /cd to the canonical form of the *same* directory: a no-op because the
    // stored form canonicalizes to the same tree, so no event is emitted.
    let canonical = root.path().canonicalize().expect("canonical");
    handle_set_working_dir(
        78,
        canonical.to_str().expect("utf8").to_string(),
        &agent,
        &agent_session_id,
        &crate::server::test_util::TestSwarmBuilder::default()
            .members(Arc::clone(&swarm_members))
            .build(),
        &client_event_tx,
    )
    .await;

    tokio::time::sleep(Duration::from_millis(50)).await;
    let events: Vec<_> = std::iter::from_fn(|| client_event_rx.try_recv().ok()).collect();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id } if *id == 78)),
        "a /cd to a canonically-equivalent dir must still resolve the request with a Done, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ServerEvent::SessionWorkingDirChanged { .. })),
        "a /cd to a canonically-equivalent dir must not emit a change event, got {events:?}"
    );

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    Ok(())
}

#[tokio::test]
async fn set_working_dir_persists_resolved_dir_for_fresh_session() -> Result<()> {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let target = tempfile::tempdir().expect("target dir").keep();

    // Fresh agent/session (no visible conversation yet). A /cd here must be
    // persisted to disk: the agent binds a model/provider route at creation, so
    // the session carries configured state and save() writes it even before the
    // first real message.
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    let agent_session_id = agent.lock().await.session_id().to_string();
    let swarm_members = Arc::new(RwLock::new(HashMap::<String, SwarmMember>::new()));
    let (client_event_tx, _client_event_rx) = mpsc::unbounded_channel();

    handle_set_working_dir(
        89,
        target.to_str().expect("utf8").to_string(),
        &agent,
        &agent_session_id,
        &crate::server::test_util::TestSwarmBuilder::default()
            .members(Arc::clone(&swarm_members))
            .build(),
        &client_event_tx,
    )
    .await;

    let persisted = crate::session::Session::load(&agent_session_id)
        .expect("a /cd on a fresh session must be persisted to disk");
    assert_eq!(
        persisted.working_dir.map(PathBuf::from),
        target.canonicalize().ok(),
        "the resolved /cd working dir must survive on disk for a fresh session"
    );

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    Ok(())
}

#[tokio::test]
async fn set_working_dir_to_previously_noncanonical_but_different_dir_is_a_change() -> Result<()> {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    // Store a non-canonical path that resolves to dir A, then /cd to a real,
    // *different* directory B. The canonical-equivalence no-op suppression must
    // NOT swallow this genuine change: dir A canonicalizes to A (not B), so a
    // change event must still be emitted and the agent re-scoped to B.
    let root = tempfile::tempdir().expect("root dir");
    let dir_a = root.path().join("a");
    let dir_b = root.path().join("b");
    std::fs::create_dir_all(&dir_a).expect("create a");
    std::fs::create_dir_all(&dir_b).expect("create b");
    let stored_a_noncanonical = root.path().join("a/.").join("..").join("a");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    {
        let mut guard = agent.lock().await;
        guard.set_working_dir(stored_a_noncanonical.to_str().expect("utf8"));
    }
    let agent_session_id = agent.lock().await.session_id().to_string();
    let (member_event_tx, mut member_event_rx) = mpsc::unbounded_channel();
    let now = Instant::now();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        agent_session_id.clone(),
        SwarmMember {
            session_id: agent_session_id.clone(),
            event_tx: member_event_tx,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: None,
            swarm_enabled: false,
            status: "ready".to_string(),
            detail: None,
            task_label: None,
            friendly_name: None,
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
    )])));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    handle_set_working_dir(
        90,
        dir_b.to_str().expect("utf8").to_string(),
        &agent,
        &agent_session_id,
        &crate::server::test_util::TestSwarmBuilder::default()
            .members(Arc::clone(&swarm_members))
            .build(),
        &client_event_tx,
    )
    .await;

    let changed_event = timeout(Duration::from_secs(2), member_event_rx.recv())
        .await
        .expect("a change to a different dir must emit a change event")
        .expect("member event channel should stay open");
    match changed_event {
        ServerEvent::SessionWorkingDirChanged {
            session_id,
            working_dir,
        } => {
            assert_eq!(session_id, agent_session_id);
            assert_eq!(
                PathBuf::from(working_dir),
                dir_b.canonicalize().expect("canonical b"),
                "the change event must carry the new (different) dir"
            );
        }
        other => panic!("expected SessionWorkingDirChanged, got {other:?}"),
    }

    let client_events: Vec<_> = std::iter::from_fn(|| client_event_rx.try_recv().ok()).collect();
    assert!(
        client_events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id } if *id == 90))
    );

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    Ok(())
}

#[tokio::test]
async fn set_working_dir_to_missing_dir_reports_error_not_change() -> Result<()> {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    // A path that must not exist, so resolve_working_dir rejects it.
    let missing = temp.path().join("../definitely-not-there");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    let agent_session_id = agent.lock().await.session_id().to_string();
    let swarm_members = Arc::new(RwLock::new(HashMap::<String, SwarmMember>::new()));
    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();

    handle_set_working_dir(
        91,
        missing.to_str().expect("utf8").to_string(),
        &agent,
        &agent_session_id,
        &crate::server::test_util::TestSwarmBuilder::default()
            .members(Arc::clone(&swarm_members))
            .build(),
        &client_event_tx,
    )
    .await;

    tokio::time::sleep(Duration::from_millis(50)).await;
    let events: Vec<_> = std::iter::from_fn(|| client_event_rx.try_recv().ok()).collect();
    assert!(
        events.iter().any(|event| matches!(
            event,
            ServerEvent::Error { id, message, .. } if *id == 91 && message.contains("does not exist")
        )),
        "a /cd to a missing dir must surface an Error, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ServerEvent::Done { id } if *id == 91)),
        "a rejected /cd must not emit a Done, got {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ServerEvent::SessionWorkingDirChanged { .. })),
        "a rejected /cd must not emit a change event, got {events:?}"
    );

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    Ok(())
}

#[tokio::test]
async fn set_working_dir_event_carries_resolved_not_raw_input() -> Result<()> {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let root = tempfile::tempdir().expect("root dir");
    let target = root.path().join("target");
    std::fs::create_dir_all(&target).expect("create target");
    // A non-canonical-but-coherent input: `target/.` canonicalizes to `target`.
    let raw_input = target.join(".").to_str().expect("utf8").to_string();

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    let agent_session_id = agent.lock().await.session_id().to_string();
    let (member_event_tx, mut member_event_rx) = mpsc::unbounded_channel();
    let now = Instant::now();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        agent_session_id.clone(),
        SwarmMember {
            session_id: agent_session_id.clone(),
            event_tx: member_event_tx,
            event_txs: HashMap::new(),
            working_dir: None,
            swarm_id: None,
            swarm_enabled: false,
            status: "ready".to_string(),
            detail: None,
            task_label: None,
            friendly_name: None,
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
    )])));
    let (client_event_tx, _client_event_rx) = mpsc::unbounded_channel();

    // The raw input is non-canonical (`target/.`), but the event must carry the
    // resolved canonical directory so the client's session/git-info cache use
    // the same key the server stores and gather_git_info derives.
    handle_set_working_dir(
        92,
        raw_input,
        &agent,
        &agent_session_id,
        &crate::server::test_util::TestSwarmBuilder::default()
            .members(Arc::clone(&swarm_members))
            .build(),
        &client_event_tx,
    )
    .await;

    let changed_event = timeout(Duration::from_secs(2), member_event_rx.recv())
        .await
        .expect("working dir change event should arrive")
        .expect("member event channel should stay open");
    match changed_event {
        ServerEvent::SessionWorkingDirChanged {
            session_id,
            working_dir,
        } => {
            assert_eq!(session_id, agent_session_id);
            assert_eq!(
                PathBuf::from(&working_dir),
                target.canonicalize().expect("canonical"),
                "the change event must carry the canonical resolved dir, not the raw '{working_dir}' input"
            );
        }
        other => panic!("expected SessionWorkingDirChanged, got {other:?}"),
    }

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    Ok(())
}

/// handle_set_handoff_resume sets a one-shot override on the agent that beats
/// the automatic latest-for-project handoff at first-message injection, and
/// replies Done. An unknown id replies Error instead.
#[tokio::test]
async fn handle_set_handoff_resume_overrides_auto_inject_and_errors_on_unknown() -> Result<()> {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());
    let wd = temp.path();
    std::fs::create_dir_all(wd).ok();

    fn seed(session_id: &str, wd: &Path, intent: &str) {
        crate::todo::save_todos(
            session_id,
            &[crate::todo::TodoItem {
                id: "t".into(),
                content: format!("work for {intent}"),
                status: "in_progress".into(),
                priority: "high".into(),
                group: None,
                confidence: None,
                ..Default::default()
            }],
        )
        .unwrap();
        crate::todo::save_plan(
            session_id,
            &crate::todo::TodoPlan {
                user_intention: Some(intent.into()),
                ..Default::default()
            },
        )
        .unwrap();
        crate::handoff::capture(session_id, Some(wd), "closed", None).expect("capture");
    }

    seed("target-handoff", wd, "target intent");
    std::thread::sleep(Duration::from_millis(20));
    seed("auto-handoff", wd, "auto intent");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    {
        let mut guard = agent.lock().await;
        guard.set_working_dir(wd.to_str().expect("utf8"));
    }

    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel();
    handle_set_handoff_resume(
        11,
        Some("target-handoff".to_string()),
        &agent,
        &client_event_tx,
    )
    .await;
    assert!(
        timeout(Duration::from_secs(2), client_event_rx.recv())
            .await
            .expect("Done should arrive")
            .is_some_and(|e| matches!(e, ServerEvent::Done { id } if id == 11)),
        "handler must reply Done for a valid handoff"
    );

    // First user message must boot from the manually selected handoff, not the
    // newer auto-inject target.
    let first_str = {
        let mut guard = agent.lock().await;
        guard
            .append_user_context_message("resume", Vec::new())
            .expect("append first message");
        // Latest user message (messages() is last-in=last-out by index).
        let mut preview = String::new();
        for message in guard.messages().iter().rev() {
            if message.role == Role::User {
                preview = message.content_preview();
                break;
            }
        }
        preview
    };
    assert!(
        first_str.contains("target intent") && !first_str.contains("auto intent"),
        "override must win over auto-inject, got: {first_str}"
    );

    // Unknown handoff id fails fast with an Error, no override set.
    let (client_event_tx2, mut client_event_rx2) = mpsc::unbounded_channel();
    handle_set_handoff_resume(12, Some("no-such".to_string()), &agent, &client_event_tx2).await;
    let error = timeout(Duration::from_secs(2), client_event_rx2.recv())
        .await
        .expect("Error should arrive")
        .expect("channel should stay open");
    match error {
        ServerEvent::Error { id, message, .. } => {
            assert_eq!(id, 12);
            assert!(message.contains("no saved handoff"), "got: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    Ok(())
}

/// handle_set_handoff_resume(None) clears a previously set override: a fresh
/// conversation afterwards uses the automatic latest-for-project handoff again.
#[tokio::test]
async fn handle_set_handoff_resume_none_clears_override() -> Result<()> {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());
    let wd = temp.path();
    std::fs::create_dir_all(wd).ok();

    fn seed(session_id: &str, wd: &Path, intent: &str) {
        crate::todo::save_todos(
            session_id,
            &[crate::todo::TodoItem {
                id: "t".into(),
                content: format!("work for {intent}"),
                status: "in_progress".into(),
                priority: "high".into(),
                group: None,
                confidence: None,
                ..Default::default()
            }],
        )
        .unwrap();
        crate::todo::save_plan(
            session_id,
            &crate::todo::TodoPlan {
                user_intention: Some(intent.into()),
                ..Default::default()
            },
        )
        .unwrap();
        crate::handoff::capture(session_id, Some(wd), "closed", None).expect("capture");
    }

    seed("manual-handoff", wd, "manual intent");
    std::thread::sleep(Duration::from_millis(20));
    seed("auto-handoff", wd, "auto intent");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Arc::new(Mutex::new(Agent::new(provider, registry)));
    {
        let mut guard = agent.lock().await;
        guard.set_working_dir(wd.to_str().expect("utf8"));
    }

    // Set the manual override, then clear it with None.
    let (tx, mut rx) = mpsc::unbounded_channel();
    handle_set_handoff_resume(21, Some("manual-handoff".to_string()), &agent, &tx).await;
    assert!(
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("Done")
            .is_some_and(|e| matches!(e, ServerEvent::Done { id } if id == 21)),
        "setting the override must reply Done"
    );
    handle_set_handoff_resume(22, None, &agent, &tx).await;
    assert!(
        timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("Done")
            .is_some_and(|e| matches!(e, ServerEvent::Done { id } if id == 22)),
        "clearing the override must reply Done"
    );

    // The override lived on `agent`, which is still a fresh conversation (no
    // messages yet). Sending `None` cleared it, so this agent's first message
    // must now use the automatic latest-for-project handoff.
    let first_str = {
        let mut guard = agent.lock().await;
        guard
            .append_user_context_message("resume", Vec::new())
            .expect("first message");
        let mut preview = String::new();
        for message in guard.messages().iter().rev() {
            if message.role == Role::User {
                preview = message.content_preview();
                break;
            }
        }
        preview
    };
    assert!(
        first_str.contains("auto intent") && !first_str.contains("manual intent"),
        "after None the fresh conversation should use auto-inject, got: {first_str}"
    );

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    Ok(())
}

/// Real-socket integration: boot the real server accept loop, connect a real
/// client over the Unix socket, and drive `Request::SetHandoffResume` through
/// the wire so it reaches the real handler. This exercises request framing and
/// server dispatch that in-process handler tests do not. A valid handoff id
/// round-trips to a `Done` reply; an unknown id yields `Error`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_socket_set_handoff_resume_round_trips_done_and_error() -> Result<()> {
    use crate::protocol::Request;
    use crate::server::Server;
    use crate::transport::Stream;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let _guard = crate::storage::lock_test_env();
    let home = tempfile::tempdir().expect("temp home");
    let sock_dir = tempfile::tempdir().expect("temp sock dir");
    let socket_path = sock_dir.path().join("jcode-e2e.sock");

    struct Restore {
        prev: [(&'static str, Option<std::ffi::OsString>); 2],
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            for (key, prev) in &self.prev {
                match prev {
                    Some(v) => crate::env::set_var(key, v.clone()),
                    None => crate::env::remove_var(key),
                }
            }
        }
    }
    let prev_home = std::env::var_os("JCODE_HOME");
    let prev_socket = std::env::var_os("JCODE_SOCKET");
    let _restore = Restore {
        prev: [
            ("JCODE_HOME", prev_home),
            ("JCODE_SOCKET", prev_socket),
        ],
    };
    crate::env::set_var("JCODE_HOME", home.path());
    crate::env::set_var("JCODE_SOCKET", &socket_path);

    // Seed a handoff the server can resolve.
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    crate::todo::save_todos(
        "target-handoff",
        &[crate::todo::TodoItem {
            id: "t".into(),
            content: "e2e work".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            group: None,
            confidence: None,
            ..Default::default()
        }],
    )
    .unwrap();
    crate::todo::save_plan(
        "target-handoff",
        &crate::todo::TodoPlan {
            user_intention: Some("e2e intent".into()),
            ..Default::default()
        },
    )
    .unwrap();
    crate::handoff::capture("target-handoff", Some(&work), "closed", None).expect("seed handoff");

    let provider: Arc<dyn Provider> = Arc::new(StreamingMockProvider::default());
    let server = Server::new(provider);
    let run_task = tokio::spawn(async move {
        let _ = server.run().await;
    });

    // Wait for the real accept loop, then connect a real client.
    let mut stream = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(s) = Stream::connect(&socket_path).await {
                break s;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .expect("server should accept within 10s");

    // The server requires a Subscribe with a working_dir before stateful
    // requests.
    let subscribe = Request::Subscribe {
        id: 20,
        working_dir: Some(work.to_string_lossy().into_owned()),
        selfdev: None,
        target_session_id: None,
        client_instance_id: None,
        client_has_local_history: false,
        allow_session_takeover: false,
        crash_on_disconnect: false,
        continue_on_disconnect: false,
        terminal_env: Vec::new(),
    };
    stream
        .write_all((serde_json::to_string(&subscribe)? + "\n").as_bytes())
        .await?;

    // A valid SetHandoffResume must round-trip to a Done reply.
    let valid = Request::SetHandoffResume {
        id: 21,
        session_id: Some("target-handoff".to_string()),
    };
    stream
        .write_all((serde_json::to_string(&valid)? + "\n").as_bytes())
        .await?;

    let mut reader = tokio::io::BufReader::new(stream);
    let valid_event: crate::protocol::ServerEvent =
        timeout(Duration::from_secs(5), async {
            loop {
                let mut line = String::new();
                let n = reader.read_line(&mut line).await?;
                if n == 0 {
                    anyhow::bail!("connection closed before Done arrived");
                }
                if let Ok(ev) =
                    serde_json::from_str::<crate::protocol::ServerEvent>(&line)
                    && matches!(ev, crate::protocol::ServerEvent::Done { id: 21 })
                {
                    return Ok(ev);
                }
            }
        })
        .await
        .expect("Done reply should arrive")?;
    assert!(
        matches!(
            valid_event,
            crate::protocol::ServerEvent::Done { id: 21 }
        ),
        "valid SetHandoffResume must round-trip to Done, got {valid_event:?}"
    );

    // An unknown id yields Error over the wire.
    let unknown = Request::SetHandoffResume {
        id: 22,
        session_id: Some("no-such-handoff".to_string()),
    };
    reader
        .get_mut()
        .write_all((serde_json::to_string(&unknown)? + "\n").as_bytes())
        .await?;
    let unknown_event: crate::protocol::ServerEvent =
        timeout(Duration::from_secs(5), async {
            loop {
                let mut line = String::new();
                let n = reader.read_line(&mut line).await?;
                if n == 0 {
                    anyhow::bail!("connection closed before Error arrived");
                }
                if let Ok(ev) =
                    serde_json::from_str::<crate::protocol::ServerEvent>(&line)
                    && matches!(
                        ev,
                        crate::protocol::ServerEvent::Error { id: 22, .. }
                    )
                {
                    return Ok(ev);
                }
            }
        })
        .await
        .expect("Error reply should arrive")?;
    match unknown_event {
        crate::protocol::ServerEvent::Error { id, message, .. } => {
            assert_eq!(id, 22);
            assert!(message.contains("no saved handoff"), "got: {message}");
        }
        other => panic!("unknown id should yield Error, got {other:?}"),
    }

    run_task.abort();
    Ok(())
}
