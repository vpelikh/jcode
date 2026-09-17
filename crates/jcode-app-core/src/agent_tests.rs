use super::*;
use crate::agent::environment::EnvSnapshotDetail;
use crate::message::{Message, StreamEvent, ToolDefinition};
use crate::provider::{EventStream, Provider};
use crate::tool::Registry;
use crate::tool::ToolOutput;
use async_trait::async_trait;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_stream::wrappers::ReceiverStream;

#[path = "agent_tests/concurrency.rs"]
mod concurrency;

#[path = "agent_tests/concurrency_construction.rs"]
mod concurrency_construction;

struct DelayedProvider {
    open_delay: Duration,
    first_event_delay: Duration,
}

struct NativeAutoCompactionProvider;

struct HandoffFailureProvider;

#[async_trait]
impl Provider for HandoffFailureProvider {
    async fn complete(
        &self,
        _: &[Message],
        _: &[ToolDefinition],
        _: &str,
        _: Option<&str>,
    ) -> Result<EventStream> {
        anyhow::bail!("stop after persisting input")
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
}

struct NativeCompactionStreamProvider;

#[derive(Clone)]
struct ExplicitPinProvider {
    model: Arc<std::sync::Mutex<String>>,
    pin: Arc<std::sync::Mutex<Option<String>>>,
    set_model_requests: Arc<std::sync::Mutex<Vec<String>>>,
}

impl ExplicitPinProvider {
    fn new(model: &str) -> Self {
        Self {
            model: Arc::new(std::sync::Mutex::new(model.to_string())),
            pin: Arc::new(std::sync::Mutex::new(None)),
            set_model_requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Provider for ExplicitPinProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        unreachable!("ExplicitPinProvider does not complete requests")
    }

    fn name(&self) -> &str {
        "openrouter"
    }

    fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    fn set_model(&self, request: &str) -> Result<()> {
        self.set_model_requests
            .lock()
            .unwrap()
            .push(request.to_string());
        let spec = request.strip_prefix("openrouter:").unwrap_or(request);
        let (model, pin) = spec
            .rsplit_once('@')
            .map(|(model, pin)| (model, Some(pin.to_string())))
            .unwrap_or((spec, None));
        *self.model.lock().unwrap() = model.to_string();
        *self.pin.lock().unwrap() = pin;
        Ok(())
    }

    fn explicit_provider_pin_for_current_model(&self) -> Option<String> {
        self.pin.lock().unwrap().clone()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

fn content_text(content: &[ContentBlock]) -> &str {
    match content.first() {
        Some(ContentBlock::Text { text, .. }) => text,
        _ => "",
    }
}

fn message_text(message: &Message) -> &str {
    content_text(&message.content)
}

#[test]
fn agent_drop_removes_its_configured_session_tool_policy() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let session = Session::create(None, None);
    let session_id = session.id.clone();
    let agent = Agent::new_with_session(
        provider,
        Registry::empty(),
        session,
        Some(HashSet::from(["bash".to_string()])),
    );

    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&session_id, "bash"),
        Some(true)
    );
    drop(agent);
    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&session_id, "bash"),
        None,
        "dropping the Agent must remove its global policy entry"
    );
}

#[test]
fn stale_agent_drop_preserves_successor_session_tool_policy() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let first_session = Session::create(None, None);
    let session_id = first_session.id.clone();
    let first = Agent::new_with_session(
        provider.clone(),
        Registry::empty(),
        first_session,
        Some(HashSet::from(["bash".to_string()])),
    );
    let mut successor_session = Session::create(None, None);
    successor_session.id.clone_from(&session_id);
    let successor = Agent::new_with_session(
        provider,
        Registry::empty(),
        successor_session,
        Some(HashSet::from(["read".to_string()])),
    );

    drop(first);

    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&session_id, "read"),
        Some(true),
        "a stale Agent must not remove its active successor's policy"
    );
    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&session_id, "bash"),
        Some(false),
        "the surviving entry must be the successor's configured policy"
    );
    drop(successor);
    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&session_id, "read"),
        None
    );
}

#[test]
fn agent_clear_moves_tool_policy_registration_to_new_session() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let session = Session::create(None, None);
    let previous_session_id = session.id.clone();
    let mut agent = Agent::new_with_session(
        provider,
        Registry::empty(),
        session,
        Some(HashSet::from(["bash".to_string()])),
    );

    agent.clear();
    let new_session_id = agent.session.id.clone();

    assert_ne!(previous_session_id, new_session_id);
    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&previous_session_id, "bash"),
        None,
        "changing sessions must remove the former ID's policy"
    );
    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&new_session_id, "bash"),
        Some(true),
        "the new session must retain the Agent's configured policy"
    );
    drop(agent);
    assert_eq!(
        crate::tool::session_tool_policy_allows_tool_for_test(&new_session_id, "bash"),
        None
    );
}

#[async_trait]
impl Provider for DelayedProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        tokio::time::sleep(self.open_delay).await;

        let first_event_delay = self.first_event_delay;
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            tokio::time::sleep(first_event_delay).await;
            let _ = tx
                .send(Ok(StreamEvent::TextDelta("hello".to_string())))
                .await;
            let _ = tx
                .send(Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".to_string()),
                }))
                .await;
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "delayed"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            open_delay: self.open_delay,
            first_event_delay: self.first_event_delay,
        })
    }
}

#[async_trait]
impl Provider for NativeAutoCompactionProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let (_tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(1);
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn supports_compaction(&self) -> bool {
        true
    }

    fn uses_jcode_compaction(&self) -> bool {
        false
    }

    fn context_window(&self) -> usize {
        1_000
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }

    async fn complete_simple(&self, _prompt: &str, _system: &str) -> Result<String> {
        Ok("manual summary from native-auto provider".to_string())
    }
}

#[async_trait]
impl Provider for NativeCompactionStreamProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(4);
        tokio::spawn(async move {
            // Response usage is deliberately far below the provider-reported
            // pre-compaction size so a regression that relabels usage as
            // `pre_tokens` is caught (#1178).
            let _ = tx
                .send(Ok(StreamEvent::TokenUsage {
                    input_tokens: Some(24_000),
                    output_tokens: Some(10),
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                }))
                .await;
            let _ = tx
                .send(Ok(StreamEvent::Compaction {
                    trigger: "openai_native".to_string(),
                    pre_tokens: Some(80_000),
                    openai_encrypted_content: Some("enc_native_test".to_string()),
                }))
                .await;
            let _ = tx
                .send(Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".to_string()),
                }))
                .await;
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn supports_compaction(&self) -> bool {
        true
    }

    fn uses_jcode_compaction(&self) -> bool {
        false
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
}

#[test]
fn tool_output_to_content_blocks_preserves_labeled_images() {
    let output = ToolOutput::new("Image ready").with_labeled_image(
        "image/png",
        "ZmFrZQ==",
        "screenshots/example.png",
    );

    let blocks = tool_output_to_content_blocks("call_1".to_string(), output);
    assert_eq!(blocks.len(), 3);

    match &blocks[0] {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            assert_eq!(tool_use_id.as_str(), "call_1");
            assert_eq!(content, "Image ready");
            assert_eq!(*is_error, None);
        }
        other => panic!("expected tool result, got {other:?}"),
    }

    match &blocks[1] {
        ContentBlock::Image { media_type, data } => {
            assert_eq!(media_type, "image/png");
            assert_eq!(data, "ZmFrZQ==");
        }
        other => panic!("expected image block, got {other:?}"),
    }

    match &blocks[2] {
        ContentBlock::Text { text, .. } => {
            assert!(text.contains("screenshots/example.png"));
            assert!(text.contains("preceding tool result"));
        }
        other => panic!("expected trailing label text, got {other:?}"),
    }
}

#[tokio::test]
async fn queued_soft_interrupt_images_are_injected_as_image_blocks() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let _guard = crate::storage::lock_test_env();
    let mut agent = Agent::new(provider, registry);

    agent.queue_soft_interrupt(
        "look at this".to_string(),
        vec![("image/png".to_string(), "ZmFrZQ==".to_string())],
        false,
        SoftInterruptSource::User,
    );
    let injected = agent.inject_soft_interrupts();

    assert_eq!(injected.len(), 1);
    let message = agent
        .session
        .messages
        .last()
        .expect("soft interrupt should append a user message");
    assert!(matches!(
        &message.content[0],
        ContentBlock::Image { media_type, data }
            if media_type == "image/png" && data == "ZmFrZQ=="
    ));
    assert!(matches!(
        &message.content[1],
        ContentBlock::Text { text, .. } if text == "look at this"
    ));
}

#[tokio::test]
async fn run_turn_streaming_mpsc_emits_keepalive_while_provider_is_quiet() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(DelayedProvider {
        open_delay: Duration::from_secs(2),
        first_event_delay: Duration::from_secs(2),
    });
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "test".to_string(),
            cache_control: None,
        }],
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move { agent.run_turn_streaming_mpsc(tx).await });

    let mut saw_keepalive = false;
    let keepalive_deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < keepalive_deadline {
        match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            Ok(Some(ServerEvent::Pong { id, .. })) => {
                assert_eq!(id, STREAM_KEEPALIVE_PONG_ID);
                saw_keepalive = true;
                break;
            }
            Ok(Some(ServerEvent::TextDelta { text })) => {
                panic!("expected keepalive before text delta, got: {text}");
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("channel closed before keepalive"),
            Err(_) => {
                assert!(
                    !task.is_finished(),
                    "streaming task finished before keepalive arrived"
                );
            }
        }
    }
    assert!(saw_keepalive, "expected keepalive before provider response");

    let mut saw_text = false;
    let text_deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < text_deadline {
        match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            Ok(Some(ServerEvent::TextDelta { text })) => {
                assert_eq!(text, "hello");
                saw_text = true;
                break;
            }
            Ok(Some(ServerEvent::Pong { id, .. })) => {
                assert_eq!(id, STREAM_KEEPALIVE_PONG_ID);
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("channel closed before text delta"),
            Err(_) => {
                assert!(
                    !task.is_finished(),
                    "streaming task finished before text delta arrived"
                );
            }
        }
    }

    assert!(saw_text, "expected delayed provider text after keepalive");
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn run_turn_streaming_mpsc_emits_native_compaction_for_client_cache_reset() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeCompactionStreamProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "compact this".to_string(),
            cache_control: None,
        }],
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.run_turn_streaming_mpsc(tx).await.unwrap();

    let mut saw_native_compaction = false;
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::Compaction {
            trigger,
            pre_tokens,
            messages_compacted,
            ..
        } = event
        {
            assert_eq!(trigger, "openai_native");
            assert_eq!(
                pre_tokens,
                Some(80_000),
                "remote compaction must forward the provider's pre-compaction count"
            );
            assert!(
                messages_compacted.is_some_and(|count| count > 0),
                "native compaction should report a non-empty compacted prefix"
            );
            saw_native_compaction = true;
        }
    }
    assert!(
        saw_native_compaction,
        "native provider compaction must reach clients so they clear KV baselines"
    );
}

/// Provider that transparently switches its model mid-stream, mimicking the
/// Anthropic retired-model fallback (`claude-fable-5` -> `claude-opus-4-8`).
struct MidStreamModelSwitchProvider {
    model: std::sync::Mutex<String>,
    switch_to: String,
}

#[async_trait]
impl Provider for MidStreamModelSwitchProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        // Emulate the provider switching its own model state during the request.
        *self.model.lock().unwrap() = self.switch_to.clone();
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(StreamEvent::TextDelta("hello".to_string())))
                .await;
            let _ = tx
                .send(Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".to_string()),
                }))
                .await;
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "claude"
    }

    fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            model: std::sync::Mutex::new(self.model.lock().unwrap().clone()),
            switch_to: self.switch_to.clone(),
        })
    }
}

#[tokio::test]
async fn run_turn_streaming_mpsc_emits_model_changed_on_midstream_switch() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(MidStreamModelSwitchProvider {
        model: std::sync::Mutex::new("claude-fable-5".to_string()),
        switch_to: "claude-opus-4-8".to_string(),
    });
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "test".to_string(),
            cache_control: None,
        }],
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move { agent.run_turn_streaming_mpsc(tx).await });

    let mut switched_model = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            Ok(Some(ServerEvent::ModelChanged { model, error, .. })) => {
                assert!(error.is_none(), "unexpected model-change error: {error:?}");
                switched_model = Some(model);
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                if task.is_finished() {
                    break;
                }
            }
        }
    }

    task.await.unwrap().unwrap();
    assert_eq!(
        switched_model.as_deref(),
        Some("claude-opus-4-8"),
        "expected a ModelChanged event resyncing to the served model"
    );
}

#[tokio::test]
async fn messages_for_provider_replays_persisted_native_compaction_in_auto_mode() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "first".to_string(),
            cache_control: None,
        }],
    );
    agent.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "second".to_string(),
            cache_control: None,
        }],
    );

    agent
        .apply_openai_native_compaction("enc_auto".to_string(), 1)
        .expect("persist native compaction");

    let (messages, event) = agent.messages_for_provider();
    assert!(event.is_none());
    assert!(!messages.is_empty());
    match &messages[0].content[0] {
        ContentBlock::OpenAICompaction { encrypted_content } => {
            assert_eq!(encrypted_content, "enc_auto");
        }
        other => panic!("expected OpenAI compaction block, got {other:?}"),
    }
    assert!(
        messages
            .iter()
            .any(|message| message.role == Role::Assistant)
    );
}

#[tokio::test]
async fn oversized_openai_native_compaction_is_persisted_as_text_fallback() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "first".to_string(),
            cache_control: None,
        }],
    );
    agent.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "second".to_string(),
            cache_control: None,
        }],
    );

    let oversized =
        "x".repeat(crate::provider::openai_request::OPENAI_ENCRYPTED_CONTENT_SAFE_MAX_CHARS + 1);
    agent
        .apply_openai_native_compaction(oversized, 1)
        .expect("persist fallback compaction");

    let state = agent
        .session
        .compaction
        .as_ref()
        .expect("compaction should be persisted");
    assert!(state.openai_encrypted_content.is_none());
    assert!(
        state
            .summary_text
            .contains("OpenAI native compaction state was discarded")
    );

    let (messages, event) = agent.messages_for_provider();
    assert!(event.is_none());
    assert!(!messages.is_empty());
    assert!(messages.iter().all(|message| {
        message
            .content
            .iter()
            .all(|block| !matches!(block, ContentBlock::OpenAICompaction { .. }))
    }));
    match &messages[0].content[0] {
        ContentBlock::Text { text, .. } => {
            assert!(text.contains("Previous Conversation Summary"));
            assert!(text.contains("OpenAI native compaction state was discarded"));
        }
        other => panic!("expected text fallback summary, got {other:?}"),
    }
    assert!(
        messages
            .iter()
            .any(|message| message.role == Role::Assistant)
    );
}

#[tokio::test]
async fn messages_for_provider_applies_manual_compaction_in_native_auto_mode() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    for i in 0..30 {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!("turn {i} {}", "x".repeat(120)),
                cache_control: None,
            }],
        );
    }

    agent.provider_session_id = Some("stale-provider-session".to_string());
    agent.session.provider_session_id = Some("stale-provider-session".to_string());

    let provider_messages = agent.provider_messages();
    let (message, success) = agent.request_manual_compaction();
    assert!(success, "manual compaction should start: {message}");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut event = None;
    let mut compacted_messages = Vec::new();
    while Instant::now() < deadline {
        let (messages, maybe_event) = agent.messages_for_provider();
        if maybe_event.is_some() {
            event = maybe_event;
            compacted_messages = messages;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let event = event.expect("manual compaction event should be applied");
    assert_eq!(event.trigger, "manual");
    assert!(agent.session.compaction.is_some());
    assert!(agent.provider_session_id.is_none());
    assert!(agent.session.provider_session_id.is_none());
    assert!(compacted_messages.len() < provider_messages.len());
    match &compacted_messages[0].content[0] {
        ContentBlock::Text { text, .. } => {
            assert!(text.contains("Previous Conversation Summary"));
            assert!(text.contains("manual summary from native-auto provider"));
        }
        other => panic!("expected text summary block, got {other:?}"),
    }
}

// ── InterruptSignal tests ────────────────────────────────────────────────

/// With `[compaction] physically_consolidate = true`, a completed manual (soft)
/// compaction must PHYSICALLY consolidate the transcript through the
/// log-bracketed seam (deepseek-harness takeaway #5): `session.messages`
/// becomes `[summary_message, recent_tail...]`, the persisted compaction state
/// is flagged `physically_consolidated`, and the compaction manager is marked
/// physically consolidated so the next provider view is NOT double-summarized.
#[tokio::test]
async fn manual_compaction_physically_consolidates_transcript_when_enabled() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let temp_home = tempfile::Builder::new()
        .prefix("jcode-phys-compact-")
        .tempdir()
        .expect("temp home");
    std::fs::write(
        temp_home.path().join("config.toml"),
        "[compaction]\nphysically_consolidate = true\n",
    )
    .expect("write config");
    crate::env::set_var("JCODE_HOME", temp_home.path());
    crate::config::Config::invalidate_cache();

    let provider: Arc<dyn Provider> = Arc::new(PruneAccountingStreamProvider { context: 20_000 });
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // Drive the transcript well past the compaction threshold so the manager
    // will actually compact on request.
    for i in 0..40 {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!("turn {i} {}", "x".repeat(400)),
                cache_control: None,
            }],
        );
    }

    let (started, ok) = agent.request_manual_compaction();
    assert!(ok, "manual compaction should start: {started}");

    // Poll for the compaction completion event.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut seen = false;
    while std::time::Instant::now() < deadline {
        let (_, maybe_event) = agent.messages_for_provider();
        if maybe_event.is_some() {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(seen, "manual compaction event should have been applied");

    let comp = agent
        .session
        .compaction
        .as_ref()
        .expect("compaction state must be set");
    assert!(
        comp.physically_consolidated,
        "compaction state must be physically consolidated"
    );

    // The session transcript must now physically hold [summary, recent_tail...],
    // NOT the full 40-message transcript.
    assert!(
        agent.session.messages.len() < 40,
        "transcript must be physically consolidated (summary + tail), got {} messages",
        agent.session.messages.len()
    );
    let first = &agent.session.messages[0];
    let is_summary = first.content.iter().any(|b| match b {
        ContentBlock::Text { text, .. } => text.contains("Previous Conversation Summary"),
        _ => false,
    });
    assert!(
        is_summary,
        "transcript[0] must be the physically-carried summary message"
    );

    // The log must hold a balanced bracket (each CompactionStart matched by a
    // CompactionEnd), which is what replay uses.
    let log = agent.session.event_log();
    let starts = log
        .iter()
        .filter(|e| matches!(&e.op, crate::session::SessionEventOp::CompactionStart { .. }))
        .count();
    let ends = log
        .iter()
        .filter(|e| matches!(&e.op, crate::session::SessionEventOp::CompactionEnd { .. }))
        .count();
    assert_eq!(
        starts, ends,
        "bracket must be balanced (starts={starts}, ends={ends})"
    );
    assert!(
        starts >= 1,
        "physical consolidation must have recorded a CompactionStart bracket"
    );
    agent
        .session
        .rederive_all_checked()
        .expect("physically consolidated session must be internally consistent");

    // The compaction manager must be marked physically consolidated so a
    // subsequent provider rebuild does not double-prepend the summary.
    let compaction_registry = agent.registry.compaction();
    let manager = compaction_registry.read().await;
    assert!(
        manager.is_physically_consolidated(),
        "compaction manager must be marked physically consolidated"
    );
    assert_eq!(
        manager.compacted_count(),
        0,
        "physical manager must have zero live skip offset"
    );
    drop(manager);

    // A provider view derived after consolidation must NOT carry a duplicate
    // synthetic "Previous Conversation Summary" prefix beyond the physical one.
    let view = agent.provider_messages();
    let summary_blocks = view
        .iter()
        .filter(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text, .. } if text.contains("Previous Conversation Summary")))
        })
        .count();
    assert_eq!(
        summary_blocks, 1,
        "exactly one summary message must be present in the provider view"
    );

    // Restore the previous JCODE_HOME and config cache.
    if let Some(previous) = prev_home {
        crate::env::set_var("JCODE_HOME", previous);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    crate::config::Config::invalidate_cache();
}

#[tokio::test]
async fn degradation_mitigation_triggers_compaction_once_on_compact_rung() {
    // Confirm the degradation tracker's Compact rung drives a single
    // compaction through the agent mitigation checkpoint (Slice 3), and that
    // the action is one-shot per escalation cycle.
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // Give the compaction manager transcript material so a compaction request
    // actually succeeds (mirrors the manual-compaction test setup).
    for i in 0..30 {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!("turn {i} {}", "x".repeat(120)),
                cache_control: None,
            }],
        );
    }

    // Fresh tracker is healthy: no mitigation fires.
    assert!(
        agent.maybe_mitigate_degradation().is_none(),
        "healthy tracker must not mitigate"
    );

    // Push past the Compact rung (default config promotes at 2 stalls).
    agent
        .degradation
        .record_stall(crate::agent::degradation::StallKind::StalledPromise);
    agent
        .degradation
        .record_stall(crate::agent::degradation::StallKind::StalledPromise);
    assert_eq!(agent.degradation.rung(), crate::agent::degradation::Rung::Compact);

    // First mitigation call triggers compaction and returns a notice.
    let notice = agent.maybe_mitigate_degradation();
    assert!(notice.is_some(), "compact rung should trigger a mitigation notice");

    // Second call is a no-op (compact already acknowledged, one-shot). Even
    // though the first compaction may still be applying in the background, the
    // pending flag is consumed, so we must not re-fire on the same cycle.
    assert!(
        agent.maybe_mitigate_degradation().is_none(),
        "compact mitigation must fire only once per cycle"
    );
}

#[tokio::test]
async fn degradation_route_fallback_gating_escalates_or_switches() {
    // Consolidated gating test. The disabled and enabled paths both depend on
    // crate::config::config(). We control them via the DEDICATED degradation
    // env vars (JCODE_DEGRADATION_ROUTE_FALLBACK / JCODE_DEGRADATION_FALLBACK_MODEL),
    // which apply_env_overrides applies on every config load and which no other
    // test sets. Unlike writing config.toml + JCODE_HOME (shared across parallel
    // config tests), env-var control is deterministic even when another test
    // concurrently rewrites JCODE_HOME, because the env override wins over the
    // config file. The scenarios run sequentially so the env is re-set before
    // each.
    let prev_enabled = std::env::var_os("JCODE_DEGRADATION_ROUTE_FALLBACK");
    let prev_model = std::env::var_os("JCODE_DEGRADATION_FALLBACK_MODEL");

    let set_fallback = |enabled: bool, model: Option<&str>| {
        crate::env::set_var("JCODE_DEGRADATION_ROUTE_FALLBACK", if enabled { "1" } else { "0" });
        match model {
            Some(model) => crate::env::set_var("JCODE_DEGRADATION_FALLBACK_MODEL", model),
            None => crate::env::remove_var("JCODE_DEGRADATION_FALLBACK_MODEL"),
        }
        crate::config::Config::invalidate_cache();
    };

    // Scenario A: disabled (explicit). Reaching RouteFallback must escalate and
    // NOT switch the model.
    set_fallback(false, None);
    let disabled_provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let mut disabled_agent = Agent::new(
        Arc::clone(&disabled_provider),
        Registry::new(disabled_provider.clone()).await,
    );
    for _ in 0..2 {
        disabled_agent
            .degradation
            .record_stall(crate::agent::degradation::StallKind::StalledPromise);
    }
    disabled_agent.degradation.acknowledge_compact();
    disabled_agent
        .degradation
        .record_stall(crate::agent::degradation::StallKind::StalledPromise);
    assert_eq!(
        disabled_agent.degradation.rung(),
        crate::agent::degradation::Rung::RouteFallback
    );
    let notice = disabled_agent.maybe_mitigate_degradation();
    assert!(notice.is_some(), "disabled fallback should surface a notice");
    assert!(
        notice.as_deref().unwrap().contains("disabled"),
        "disabled notice should say fallback is disabled: {:?}",
        notice
    );
    assert_eq!(
        disabled_agent.degradation.rung(),
        crate::agent::degradation::Rung::Escalated,
        "disabled fallback must escalate, not switch the model"
    );

    // Scenario B: enabled with a fallback model. Reaching RouteFallback
    // switches the provider onto the fallback and resets the cycle.
    set_fallback(true, Some("deepseek/deepseek-v3@deepseek"));
    let enabled_provider = Arc::new(ExplicitPinProvider::new("deepseek/deepseek-v4-flash@deepseek"));
    let enabled_provider_dyn: Arc<dyn Provider> = enabled_provider.clone();
    let mut enabled_agent = Agent::new(
        Arc::clone(&enabled_provider_dyn),
        Registry::new(enabled_provider_dyn).await,
    );
    for _ in 0..2 {
        enabled_agent
            .degradation
            .record_stall(crate::agent::degradation::StallKind::StalledPromise);
    }
    enabled_agent.degradation.acknowledge_compact();
    enabled_agent
        .degradation
        .record_stall(crate::agent::degradation::StallKind::StalledPromise);
    assert_eq!(
        enabled_agent.degradation.rung(),
        crate::agent::degradation::Rung::RouteFallback
    );
    let gen_before = enabled_agent.provider_model_selection_generation();
    let notice = enabled_agent.maybe_mitigate_degradation();
    assert!(notice.is_some(), "configured fallback should produce a notice");
    assert!(
        notice.as_deref().unwrap().contains("switched route"),
        "notice should confirm the route switch: {:?}",
        notice
    );
    assert_eq!(
        enabled_provider.model(),
        "deepseek/deepseek-v3",
        "provider model should switch to the configured fallback"
    );
    assert!(
        !enabled_agent.user_selected_provider_model_after(gen_before),
        "an automatic route fallback must NOT be recorded as a user model choice"
    );
    assert_eq!(
        enabled_agent.degradation.rung(),
        crate::agent::degradation::Rung::Healthy,
        "a successful fallback resets the escalation cycle"
    );

    // Scenario C: compaction unsupported (ExplicitPinProvider) + fallback
    // disabled. Reaching the Compact rung must NOT silently spin; it escalates
    // promptly to the decision point, which with fallback disabled surfaces to
    // the user (Escalated) rather than stalling at Compact forever.
    set_fallback(false, None);
    let no_compact_provider: Arc<dyn Provider> =
        Arc::new(ExplicitPinProvider::new("some-model"));
    let mut no_compact_agent = Agent::new(
        Arc::clone(&no_compact_provider),
        Registry::new(no_compact_provider).await,
    );
    for _ in 0..2 {
        no_compact_agent
            .degradation
            .record_stall(crate::agent::degradation::StallKind::StalledPromise);
    }
    assert_eq!(
        no_compact_agent.degradation.rung(),
        crate::agent::degradation::Rung::Compact
    );
    let notice = no_compact_agent.maybe_mitigate_degradation();
    assert!(
        notice.is_some(),
        "unsupported compaction should surface a notice"
    );
    assert_eq!(
        no_compact_agent.degradation.rung(),
        crate::agent::degradation::Rung::Escalated,
        "unsupported compaction + disabled fallback must escalate to the user"
    );

    // Restore the process env so we do not leak these overrides into other tests.
    match prev_enabled {
        Some(v) => crate::env::set_var("JCODE_DEGRADATION_ROUTE_FALLBACK", v),
        None => crate::env::remove_var("JCODE_DEGRADATION_ROUTE_FALLBACK"),
    }
    match prev_model {
        Some(v) => crate::env::set_var("JCODE_DEGRADATION_FALLBACK_MODEL", v),
        None => crate::env::remove_var("JCODE_DEGRADATION_FALLBACK_MODEL"),
    }
    crate::config::Config::invalidate_cache();
}

#[tokio::test]
async fn degradation_tracker_resets_on_model_switch() {
    // The tracker is keyed by route, set once at construction. A mid-session
    // model switch must reset the escalation cycle so the new route starts
    // healthy instead of inheriting the previous model's stall history.
    let provider = Arc::new(ExplicitPinProvider::new("model-a"));
    let provider_dyn: Arc<dyn Provider> = provider.clone();
    let mut agent = Agent::new(
        Arc::clone(&provider_dyn),
        Registry::new(provider_dyn).await,
    );

    // Escalate the tracker on the original route.
    for _ in 0..3 {
        agent
            .degradation
            .record_stall(crate::agent::degradation::StallKind::StalledPromise);
    }
    assert_eq!(
        agent.degradation.rung(),
        crate::agent::degradation::Rung::Compact
    );

    // Switch to a new model: the escalation cycle must reset.
    agent.set_model("model-b").expect("model switch should succeed");
    assert_eq!(
        agent.degradation.rung(),
        crate::agent::degradation::Rung::Healthy,
        "a model switch must reset the degradation cycle"
    );
    assert_eq!(
        agent.degradation.stall_count(),
        0,
        "a model switch must clear stale stall history"
    );
}

#[tokio::test]
async fn interrupt_signal_fire_before_notified_does_not_hang() {
    // Regression test: fire() called BEFORE notified().await must not hang.
    // The old code called notify_waiters() which drops the notification if
    // nobody is waiting yet. The flag is still set so the fast path catches it,
    // but only if the future is created before the flag check.
    let sig = InterruptSignal::new();
    sig.fire(); // fire before anyone is waiting
    tokio::time::timeout(std::time::Duration::from_millis(100), sig.notified())
        .await
        .expect("notified() hung when signal was already set before call");
}

#[tokio::test]
async fn interrupt_signal_fire_concurrent_with_notified() {
    // Regression test for the race window: fire() is called concurrently while
    // notified() is being set up. The fix (create future before flag check) ensures
    // the notify_waiters() in fire() wakes the registered future.
    let sig = Arc::new(InterruptSignal::new());
    let sig2 = Arc::clone(&sig);

    // Spawn a task that fires after a tiny delay, giving the main task time to
    // enter notified() but before it reaches notified().await.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        sig2.fire();
    });

    tokio::time::timeout(std::time::Duration::from_millis(500), sig.notified())
        .await
        .expect("notified() hung during concurrent fire()");
}

#[tokio::test]
async fn interrupt_signal_is_set_false_initially() {
    let sig = InterruptSignal::new();
    assert!(!sig.is_set());
}

#[tokio::test]
async fn interrupt_signal_is_set_true_after_fire() {
    let sig = InterruptSignal::new();
    sig.fire();
    assert!(sig.is_set());
}

#[tokio::test]
async fn interrupt_signal_reset_clears_flag() {
    let sig = InterruptSignal::new();
    sig.fire();
    assert!(sig.is_set());
    sig.reset();
    assert!(!sig.is_set());
}

#[tokio::test]
async fn interrupt_signal_notified_completes_after_fire() {
    let sig = Arc::new(InterruptSignal::new());
    let sig2 = Arc::clone(&sig);

    let handle = tokio::spawn(async move {
        sig2.notified().await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    sig.fire();

    tokio::time::timeout(std::time::Duration::from_millis(200), handle)
        .await
        .expect("notified() task timed out after fire()")
        .expect("task panicked");
}

#[tokio::test]
async fn new_agent_registers_active_pid_and_clear_swaps_it() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let first_session_id = agent.session_id().to_string();
    assert!(
        crate::session::active_session_ids().contains(&first_session_id),
        "fresh agent session should be tracked as active"
    );

    agent.clear();

    let second_session_id = agent.session_id().to_string();
    let active = crate::session::active_session_ids();
    assert_ne!(first_session_id, second_session_id);
    assert!(
        active.contains(&second_session_id),
        "replacement session should be tracked as active"
    );
    assert!(
        !active.contains(&first_session_id),
        "cleared session should no longer be tracked as active"
    );
}

#[tokio::test]
async fn gmail_is_exposed_by_default_and_can_be_explicitly_disabled() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let prev_tools = std::env::var_os("JCODE_TOOLS");
    let prev_disabled_tools = std::env::var_os("JCODE_DISABLED_TOOLS");
    let prev_tool_profile = std::env::var_os("JCODE_TOOL_PROFILE");
    let prev_disable_base_tools = std::env::var_os("JCODE_DISABLE_BASE_TOOLS");
    let temp_home = tempfile::TempDir::new().expect("temp home");

    crate::env::set_var("JCODE_HOME", temp_home.path());
    crate::env::remove_var("JCODE_TOOLS");
    crate::env::remove_var("JCODE_DISABLED_TOOLS");
    crate::env::remove_var("JCODE_TOOL_PROFILE");
    crate::env::remove_var("JCODE_DISABLE_BASE_TOOLS");
    crate::config::Config::invalidate_cache();

    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    let definitions = agent.tool_definitions().await;
    let tool_names = agent.tool_names().await;
    let tool_name = "gmail";

    assert!(
        tool_names.iter().any(|name| name == "jcode_docs"),
        "jcode_docs must be model-visible in regular sessions"
    );
    assert!(
        !tool_names.iter().any(|name| name == "selfdev"),
        "selfdev must not be model-visible in regular sessions"
    );

    assert!(
        definitions
            .iter()
            .any(|definition| definition.name == tool_name),
        "{tool_name} must be sent in model-visible tool definitions by default"
    );
    assert!(
        tool_names.iter().any(|name| name == tool_name),
        "{tool_name} must be listed as model-visible by default"
    );
    agent
        .validate_tool_allowed(tool_name)
        .expect("gmail must be executable by default");

    crate::env::set_var("JCODE_DISABLED_TOOLS", tool_name);
    crate::config::Config::invalidate_cache();

    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    let definitions = agent.tool_definitions().await;
    let tool_names = agent.tool_names().await;

    assert!(
        !definitions
            .iter()
            .any(|definition| definition.name == tool_name),
        "explicitly disabled {tool_name} must not be sent in model-visible tool definitions"
    );
    assert!(
        !tool_names.iter().any(|name| name == tool_name),
        "explicitly disabled {tool_name} must not be listed as model-visible"
    );
    let err = agent
        .validate_tool_allowed(tool_name)
        .expect_err("explicitly disabled gmail must not be executable");
    assert!(err.to_string().contains("disabled"));

    if let Some(previous) = prev_home {
        crate::env::set_var("JCODE_HOME", previous);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
    if let Some(previous) = prev_tools {
        crate::env::set_var("JCODE_TOOLS", previous);
    } else {
        crate::env::remove_var("JCODE_TOOLS");
    }
    if let Some(previous) = prev_disabled_tools {
        crate::env::set_var("JCODE_DISABLED_TOOLS", previous);
    } else {
        crate::env::remove_var("JCODE_DISABLED_TOOLS");
    }
    if let Some(previous) = prev_tool_profile {
        crate::env::set_var("JCODE_TOOL_PROFILE", previous);
    } else {
        crate::env::remove_var("JCODE_TOOL_PROFILE");
    }
    if let Some(previous) = prev_disable_base_tools {
        crate::env::set_var("JCODE_DISABLE_BASE_TOOLS", previous);
    } else {
        crate::env::remove_var("JCODE_DISABLE_BASE_TOOLS");
    }
    crate::config::Config::invalidate_cache();
}

fn seed_transient_session_state(agent: &mut Agent) {
    agent.push_alert("pending alert".to_string());
    agent.queue_soft_interrupt(
        "queued interrupt".to_string(),
        Vec::new(),
        true,
        SoftInterruptSource::User,
    );
    agent.background_tool_signal.fire();
    agent.request_graceful_shutdown();
    agent.tool_call_ids.insert("tool_call_old".to_string().into());
    agent.tool_result_ids.insert("tool_result_old".to_string().into());
    agent.tool_output_scan_index = 7;
    agent.last_upstream_provider = Some("upstream_old".to_string());
    agent.last_connection_type = Some("websocket".to_string());
    agent.current_turn_system_reminder = Some("reminder".to_string());
    agent.last_usage = TokenUsage {
        input_tokens: 11,
        output_tokens: 17,
        cache_read_input_tokens: Some(3),
        cache_creation_input_tokens: Some(5),
    };
    agent.locked_tools = Some(vec![ToolDefinition {
        name: "test_tool".to_string(),
        description: "test tool".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
    }]);
}

#[tokio::test]
async fn clear_resets_runtime_interrupt_and_queue_state() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    seed_transient_session_state(&mut agent);
    assert_eq!(agent.soft_interrupt_count(), 1);
    assert!(agent.background_tool_signal().is_set());
    assert!(agent.graceful_shutdown_signal().is_set());

    agent.clear();

    assert_eq!(agent.soft_interrupt_count(), 0);
    assert!(!agent.background_tool_signal().is_set());
    assert!(!agent.graceful_shutdown_signal().is_set());
    assert_eq!(agent.pending_alert_count(), 0);
    assert!(agent.tool_call_ids.is_empty());
    assert!(agent.tool_result_ids.is_empty());
    assert_eq!(agent.tool_output_scan_index, 0);
    assert!(agent.last_upstream_provider.is_none());
    assert!(agent.last_connection_type.is_none());
    assert!(agent.current_turn_system_reminder.is_none());
    assert_eq!(agent.last_usage.input_tokens, 0);
    assert_eq!(agent.last_usage.output_tokens, 0);
    assert!(agent.locked_tools.is_none());
}

#[tokio::test]
async fn restore_session_resets_runtime_interrupt_and_queue_state() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let mut restored_session = crate::session::Session::create_with_id(
        "session_restore_resets_runtime_state".to_string(),
        None,
        None,
    );
    // A real conversation line so the session persists on disk and always
    // overwrites any stale (possibly Crashed) leftover with the same fixed id;
    // otherwise the intentional save() skip for untouched sessions leaves stale
    // state behind when this test runs after others in the same process.
    restored_session.add_message(
        crate::message::Role::User,
        vec![crate::message::ContentBlock::Text {
            text: "resume".to_string(),
            cache_control: None,
        }],
    );
    restored_session.save().expect("save restored session");

    seed_transient_session_state(&mut agent);
    assert_eq!(agent.soft_interrupt_count(), 1);
    assert!(agent.background_tool_signal().is_set());
    assert!(agent.graceful_shutdown_signal().is_set());

    let status = agent
        .restore_session(&restored_session.id)
        .expect("restore session should succeed");

    assert_eq!(status, crate::session::SessionStatus::Active);
    assert_eq!(agent.session_id(), restored_session.id);
    assert_eq!(agent.soft_interrupt_count(), 0);
    assert!(!agent.background_tool_signal().is_set());
    assert!(!agent.graceful_shutdown_signal().is_set());
    assert_eq!(agent.pending_alert_count(), 0);
    assert!(agent.tool_call_ids.is_empty());
    assert!(agent.tool_result_ids.is_empty());
    assert_eq!(agent.tool_output_scan_index, 0);
    assert!(agent.last_upstream_provider.is_none());
    assert!(agent.last_connection_type.is_none());
    assert!(agent.current_turn_system_reminder.is_none());
    assert_eq!(agent.last_usage.input_tokens, 0);
    assert_eq!(agent.last_usage.output_tokens, 0);
    assert!(agent.locked_tools.is_none());
}

#[tokio::test]
async fn explicit_provider_pin_is_persisted_and_reapplied_on_restore() {
    let _guard = crate::storage::lock_test_env();
    let provider = Arc::new(ExplicitPinProvider::new("z-ai/glm-5.2"));
    let provider_dyn: Arc<dyn Provider> = provider.clone();
    let registry = Registry::new(provider_dyn.clone()).await;
    let mut agent = Agent::new(provider_dyn, registry);

    agent
        .set_model("z-ai/glm-5.2@Novita")
        .expect("set explicitly pinned model");
    assert_eq!(agent.provider_model(), "z-ai/glm-5.2@Novita");
    let persisted = crate::session::Session::load(agent.session_id()).expect("load saved session");
    assert_eq!(persisted.model.as_deref(), Some("z-ai/glm-5.2@Novita"));

    let restored_provider = Arc::new(ExplicitPinProvider::new("other/model"));
    let restored_provider_dyn: Arc<dyn Provider> = restored_provider.clone();
    let restored_registry = Registry::new(restored_provider_dyn.clone()).await;
    let restored_agent =
        Agent::new_with_session(restored_provider_dyn, restored_registry, persisted, None);

    assert_eq!(
        restored_provider
            .set_model_requests
            .lock()
            .unwrap()
            .as_slice(),
        ["openrouter:z-ai/glm-5.2@Novita"]
    );
    assert_eq!(restored_agent.provider_model(), "z-ai/glm-5.2@Novita");
}

#[tokio::test]
async fn restore_session_rehydrates_injected_memory_ids() {
    let _guard = crate::storage::lock_test_env();
    crate::memory::clear_all_pending_memory();

    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let mut restored_session = crate::session::Session::create_with_id(
        "session_restore_memory_dedup".to_string(),
        None,
        None,
    );
    restored_session.record_memory_injection(
        "🧠 auto-recalled 1 memory".to_string(),
        "persisted memory".to_string(),
        1,
        5,
        vec!["memory-persisted".to_string()],
    );
    restored_session.save().expect("save restored session");

    crate::memory::mark_memories_injected(&restored_session.id, &["memory-stale".to_string()]);

    agent
        .restore_session(&restored_session.id)
        .expect("restore session should succeed");

    assert!(crate::memory::is_memory_injected(
        &restored_session.id,
        "memory-persisted"
    ));
    assert!(
        !crate::memory::is_memory_injected(&restored_session.id, "memory-stale"),
        "restore should replace stale in-memory dedup state with persisted session data"
    );

    crate::memory::clear_all_pending_memory();
}

#[tokio::test]
async fn build_memory_prompt_nonblocking_defers_pending_memory_during_tool_loop() {
    let _guard = crate::storage::lock_test_env();
    crate::memory::clear_all_pending_memory();

    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let agent = Agent::new(provider, registry);
    let session_id = agent.session.id.clone();

    crate::memory::set_pending_memory_with_ids(
        &session_id,
        "remember this later".to_string(),
        1,
        vec!["memory-deferred".to_string()],
    );

    let tool_loop_messages = vec![
        Message::user("hello"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string().into(),
                name: "bash".to_string(),
                input: serde_json::json!({}),
                thought_signature: None,
            }],
            timestamp: Some(chrono::Utc::now()),
            tool_duration_ms: None,
        },
        Message::tool_result("call_1", "ok", false),
    ];

    let pending = agent.build_memory_prompt_nonblocking(&tool_loop_messages, None);
    assert!(pending.is_none(), "memory should not inject mid tool loop");
    assert!(crate::memory::has_pending_memory(&session_id));

    let next_turn_messages = vec![Message::user("follow up")];
    let pending = agent.build_memory_prompt_nonblocking(&next_turn_messages, None);
    assert!(
        pending.is_some(),
        "memory should inject on the next real user turn"
    );
    assert!(!crate::memory::has_pending_memory(&session_id));

    crate::memory::clear_all_pending_memory();
}

#[tokio::test]
async fn memory_injection_message_defaults_to_ephemeral_history() {
    let _guard = crate::storage::lock_test_env();
    let previous = std::env::var_os("JCODE_PERSIST_MEMORY_INJECTIONS");
    crate::env::set_var("JCODE_PERSIST_MEMORY_INJECTIONS", "false");
    crate::config::invalidate_config_cache();

    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    let before = agent.session.messages.len();
    let memory = crate::memory::PendingMemory {
        prompt: "# Memory\n\n## Facts\n1. Use ephemeral mode".to_string(),
        display_prompt: None,
        computed_at: Instant::now(),
        count: 1,
        memory_ids: vec!["mem-ephemeral".to_string()],
    };

    let (message, persisted) = agent.prepare_memory_injection_message(&memory);

    assert!(!persisted);
    assert_eq!(agent.session.messages.len(), before);
    assert!(matches!(message.role, Role::User));
    assert!(message_text(&message).contains("Use ephemeral mode"));

    match previous {
        Some(value) => crate::env::set_var("JCODE_PERSIST_MEMORY_INJECTIONS", value),
        None => crate::env::remove_var("JCODE_PERSIST_MEMORY_INJECTIONS"),
    }
    crate::config::invalidate_config_cache();
}

#[tokio::test]
async fn memory_injection_message_can_persist_to_history() {
    let _guard = crate::storage::lock_test_env();
    let previous = std::env::var_os("JCODE_PERSIST_MEMORY_INJECTIONS");
    crate::env::set_var("JCODE_PERSIST_MEMORY_INJECTIONS", "true");
    crate::config::invalidate_config_cache();

    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    let before = agent.session.messages.len();
    let memory = crate::memory::PendingMemory {
        prompt: "# Memory\n\n## Facts\n1. Persist for cache".to_string(),
        display_prompt: None,
        computed_at: Instant::now(),
        count: 1,
        memory_ids: vec!["mem-persisted".to_string()],
    };

    let (message, persisted) = agent.prepare_memory_injection_message(&memory);

    assert!(persisted);
    assert_eq!(agent.session.messages.len(), before + 1);
    assert_eq!(
        content_text(&agent.session.messages.last().unwrap().content),
        message_text(&message)
    );
    assert!(
        content_text(&agent.session.messages.last().unwrap().content).contains("Persist for cache")
    );

    match previous {
        Some(value) => crate::env::set_var("JCODE_PERSIST_MEMORY_INJECTIONS", value),
        None => crate::env::remove_var("JCODE_PERSIST_MEMORY_INJECTIONS"),
    }
    crate::config::invalidate_config_cache();
}

#[tokio::test]
async fn mark_closed_persists_soft_interrupts_for_restore_after_reload() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::TempDir::new().expect("temp dir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider.clone(), registry.clone());
    let session_id = agent.session_id().to_string();
    agent.session.save().expect("save active session");
    agent.queue_soft_interrupt(
        "resume me after reload".to_string(),
        Vec::new(),
        true,
        SoftInterruptSource::System,
    );

    agent.mark_closed();

    let mut restored = Agent::new(provider, registry);
    restored
        .restore_session(&session_id)
        .expect("restore session with persisted interrupts");

    assert_eq!(restored.soft_interrupt_count(), 1);
    assert!(restored.has_urgent_interrupt());
    assert!(
        crate::soft_interrupt_store::load(&session_id)
            .expect("store should be readable after restore")
            .is_empty()
    );

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}

#[tokio::test]
async fn env_snapshot_detail_is_minimal_for_empty_sessions_and_full_after_history() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    assert_eq!(agent.env_snapshot_detail(), EnvSnapshotDetail::Minimal);
    let minimal = agent.build_env_snapshot("create", agent.env_snapshot_detail());
    assert!(minimal.jcode_git_hash.is_none());
    assert!(minimal.jcode_git_dirty.is_none());
    assert!(minimal.working_git.is_none());

    agent
        .session
        .append_stored_message(crate::session::StoredMessage {
            id: "msg_env_snapshot_detail".to_string(),
            role: crate::message::Role::User,
            content: vec![ContentBlock::Text {
                text: "hello".to_string(),
                cache_control: None,
            }],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        });

    assert_eq!(agent.env_snapshot_detail(), EnvSnapshotDetail::Full);
}

/// A trivial tool used to simulate an MCP tool registering on the registry
/// after the agent has already locked its tool snapshot.
struct FakeMcpTool {
    name: String,
}

#[async_trait]
impl crate::tool::Tool for FakeMcpTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "fake mcp tool"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: crate::tool::ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::new("ok"))
    }
}

struct VerboseFakeMcpTool {
    name: String,
    description: String,
}

#[async_trait]
impl crate::tool::Tool for VerboseFakeMcpTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"value": {"type": "string"}}
        })
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: crate::tool::ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::new("ok"))
    }
}

async fn register_fake_deferred_mcp_surface(registry: &Registry) {
    for name in ["mcp_search", "mcp_call"] {
        registry
            .register(
                name.to_string(),
                Arc::new(FakeMcpTool {
                    name: name.to_string(),
                }) as Arc<dyn crate::tool::Tool>,
            )
            .await;
    }
}

async fn agent_with_fake_mcp_surface(mode: crate::config::McpToolsMode, threshold: usize) -> Agent {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    register_fake_deferred_mcp_surface(&registry).await;
    registry
        .register(
            "mcp__test__verbose".to_string(),
            Arc::new(VerboseFakeMcpTool {
                name: "verbose".to_string(),
                description: "large MCP definition ".repeat(32),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;
    let mut agent = Agent::new(provider, registry);
    agent.mcp_tools_mode = mode;
    agent.mcp_tools_token_threshold = threshold;
    agent
}

#[tokio::test]
async fn mcp_exposure_modes_select_eager_or_fixed_definitions() {
    let _guard = crate::storage::lock_test_env();

    let mut eager = agent_with_fake_mcp_surface(crate::config::McpToolsMode::Eager, 0).await;
    let eager_names: Vec<String> = eager
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(eager_names.iter().any(|name| name == "mcp__test__verbose"));
    assert!(!eager_names.iter().any(|name| name == "mcp_search"));
    assert!(!eager_names.iter().any(|name| name == "mcp_call"));

    let mut deferred =
        agent_with_fake_mcp_surface(crate::config::McpToolsMode::Deferred, usize::MAX).await;
    let deferred_names: Vec<String> = deferred
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(!deferred_names.iter().any(|name| name.starts_with("mcp__")));
    assert!(deferred_names.iter().any(|name| name == "mcp_search"));
    assert!(deferred_names.iter().any(|name| name == "mcp_call"));

    let mut auto_eager =
        agent_with_fake_mcp_surface(crate::config::McpToolsMode::Auto, usize::MAX).await;
    let auto_eager_names: Vec<String> = auto_eager
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(
        auto_eager_names
            .iter()
            .any(|name| name == "mcp__test__verbose")
    );

    let mut auto_deferred = agent_with_fake_mcp_surface(crate::config::McpToolsMode::Auto, 1).await;
    let auto_deferred_names: Vec<String> = auto_deferred
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert!(
        !auto_deferred_names
            .iter()
            .any(|name| name.starts_with("mcp__"))
    );
    assert!(auto_deferred_names.iter().any(|name| name == "mcp_search"));
    assert!(auto_deferred_names.iter().any(|name| name == "mcp_call"));
    let stable_auto_names: Vec<String> = auto_deferred
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(auto_deferred_names, stable_auto_names);
    assert!(auto_deferred.mcp_late_register_resolved);
}

#[tokio::test]
async fn deferred_mcp_surface_ignores_late_per_tool_registration() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    register_fake_deferred_mcp_surface(&registry).await;
    let mut agent = Agent::new(provider, registry);
    agent.mcp_tools_mode = crate::config::McpToolsMode::Deferred;

    let before: Vec<String> = agent
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    agent
        .registry
        .register(
            "mcp__late__tool".to_string(),
            Arc::new(FakeMcpTool {
                name: "late".to_string(),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;
    let after: Vec<String> = agent
        .tool_definitions()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();

    assert_eq!(
        before, after,
        "fixed deferred surface must stay cache-stable"
    );
    assert!(agent.mcp_late_register_resolved);
    assert!(!after.iter().any(|name| name.starts_with("mcp__")));
}

#[tokio::test]
async fn auto_mode_rechecks_late_mcp_definitions_before_deferring() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    register_fake_deferred_mcp_surface(&registry).await;
    let mut agent = Agent::new(provider, registry);
    agent.mcp_tools_mode = crate::config::McpToolsMode::Auto;
    agent.mcp_tools_token_threshold = 1;

    let before = agent.tool_definitions().await;
    assert!(!before.iter().any(|tool| tool.name == "mcp_search"));
    agent
        .registry
        .register(
            "mcp__late__large".to_string(),
            Arc::new(VerboseFakeMcpTool {
                name: "large".to_string(),
                description: "late large definition ".repeat(32),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;

    let after = agent.tool_definitions().await;
    assert!(after.iter().any(|tool| tool.name == "mcp_search"));
    assert!(after.iter().any(|tool| tool.name == "mcp_call"));
    assert!(!after.iter().any(|tool| tool.name.starts_with("mcp__")));
    assert!(agent.mcp_late_register_resolved);
}

/// Reproduction for #206: MCP tools that register on the registry *after* the
/// first turn locks the tool snapshot never reach the provider, because
/// `tool_definitions()` returns the frozen `locked_tools` snapshot and the only
/// unlock path (`unlock_tools_if_needed`) fires solely when the LLM invokes the
/// `"mcp"` management tool — which it never does, since it cannot see the
/// `mcp__*` tools it would need to trigger that unlock.
#[tokio::test]
async fn mcp_tools_registered_after_lock_are_visible_to_agent() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // First turn locks the snapshot (this is what happens before the async MCP
    // registration spawn completes).
    let before = agent.tool_definitions().await;
    let before_len = before.len();
    assert!(
        !before.iter().any(|t| t.name.starts_with("mcp__")),
        "precondition: no mcp tools before async registration completes"
    );

    // Simulate the spawned MCP registration task finishing: a new mcp__* tool
    // lands on the shared registry.
    agent
        .registry
        .register(
            "mcp__test__write_memory".to_string(),
            Arc::new(FakeMcpTool {
                name: "mcp__test__write_memory".to_string(),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;

    // The next turn should now advertise the MCP tool to the provider.
    let after = agent.tool_definitions().await;
    assert!(
        after.iter().any(|t| t.name == "mcp__test__write_memory"),
        "regression #206: MCP tool registered after the first turn never reaches \
         the agent's tool surface (locked snapshot of {} tools is reused forever)",
        before_len
    );

    // Once MCP tools are present in the locked snapshot, subsequent turns must
    // return the *same* stable snapshot so provider prompt-cache hits stay warm
    // (the whole point of locked_tools). The #206 fix must not flap.
    let names =
        |defs: &[ToolDefinition]| -> Vec<String> { defs.iter().map(|t| t.name.clone()).collect() };
    let stable_a = agent.tool_definitions().await;
    let stable_b = agent.tool_definitions().await;
    assert_eq!(
        names(&stable_a),
        names(&stable_b),
        "tool snapshot must be stable across turns once MCP tools are present"
    );
    assert_eq!(
        names(&stable_a),
        names(&after),
        "snapshot must not change after MCP tools are already included"
    );
}

/// The intentional, MCP-driven prompt-cache miss must happen at most ONCE per
/// locked snapshot. After the first late-registered `mcp__*` tool is picked up
/// (the one accepted miss), a *second* MCP tool that registers even later must
/// NOT trigger another rebuild — otherwise a server that connects in waves would
/// thrash the provider prompt cache. Guards the `mcp_late_register_resolved`
/// one-shot flag (#206 follow-up).
#[tokio::test]
async fn mcp_late_registration_rebuild_happens_at_most_once() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // First turn locks the snapshot with no MCP tools yet.
    let _ = agent.tool_definitions().await;

    // First MCP tool arrives -> one accepted rebuild exposes it.
    agent
        .registry
        .register(
            "mcp__test__first".to_string(),
            Arc::new(FakeMcpTool {
                name: "mcp__test__first".to_string(),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;
    let after_first = agent.tool_definitions().await;
    assert!(
        after_first.iter().any(|t| t.name == "mcp__test__first"),
        "first late MCP tool must be picked up by the one accepted rebuild"
    );
    assert!(
        agent.mcp_late_register_resolved,
        "one-shot guard must latch after the accepted rebuild"
    );

    // A SECOND MCP tool registers even later (server connected in a second
    // wave). The one-shot guard means we do NOT rebuild again, so the snapshot
    // stays cache-stable and this tool is intentionally not surfaced until the
    // tool list is explicitly unlocked.
    agent
        .registry
        .register(
            "mcp__test__second".to_string(),
            Arc::new(FakeMcpTool {
                name: "mcp__test__second".to_string(),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;
    let after_second = agent.tool_definitions().await;
    let names: Vec<String> = after_second.iter().map(|t| t.name.clone()).collect();
    assert!(
        names.iter().any(|n| n == "mcp__test__first"),
        "previously surfaced MCP tool must remain"
    );
    assert!(
        !names.iter().any(|n| n == "mcp__test__second"),
        "second-wave MCP tool must NOT trigger a second cache-busting rebuild"
    );

    // An explicit unlock (e.g. the `mcp` reload tool) re-arms the one-shot guard
    // and lets the next snapshot pick up everything currently registered.
    agent.unlock_tools();
    assert!(
        !agent.mcp_late_register_resolved,
        "explicit unlock must re-arm the one-shot guard"
    );
    let after_unlock = agent.tool_definitions().await;
    let unlocked_names: Vec<String> = after_unlock.iter().map(|t| t.name.clone()).collect();
    assert!(
        unlocked_names.iter().any(|n| n == "mcp__test__second"),
        "after explicit unlock, the second-wave MCP tool must finally surface"
    );
}

/// Without any newly-registered MCP tools, the locked snapshot must be returned
/// verbatim on every turn (no rebuild, no cache invalidation). Guards the #206
/// fix against re-snapshotting on turns where nothing changed.
#[tokio::test]
async fn tool_snapshot_is_stable_without_new_mcp_tools() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let first = agent.tool_definitions().await;
    // Register a NON-mcp tool after locking — this should NOT trigger a rebuild,
    // because the cache-stability optimization only yields to MCP arrival.
    agent
        .registry
        .register(
            "not_an_mcp_tool".to_string(),
            Arc::new(FakeMcpTool {
                name: "not_an_mcp_tool".to_string(),
            }) as Arc<dyn crate::tool::Tool>,
        )
        .await;
    let second = agent.tool_definitions().await;
    let first_names: Vec<String> = first.iter().map(|t| t.name.clone()).collect();
    let second_names: Vec<String> = second.iter().map(|t| t.name.clone()).collect();
    assert_eq!(
        first_names, second_names,
        "non-MCP registry changes must not invalidate the locked tool snapshot"
    );
    assert!(
        !second_names.iter().any(|n| n == "not_an_mcp_tool"),
        "non-MCP tool registered after lock must not leak into the snapshot"
    );
}

#[test]
#[expect(
    clippy::assertions_on_constants,
    reason = "keeps a regression guard on the intentionally-tuned empty-post-tool retry budget"
)]
fn empty_post_tool_response_gets_more_than_one_retry() {
    // Regression guard for the Claude Opus 5 benchmark incident. A provider can
    // return an empty response immediately after tool results; that is a
    // transient hiccup, not a finished task. With only one retry allowed, a
    // single empty response (observed once in 43 turns) ended a 20-hour agent
    // run with the work half-done and the submission unoptimized.
    assert!(
        Agent::MAX_EMPTY_POST_TOOL_CONTINUATION_ATTEMPTS > 1,
        "a single retry lets one transient empty response end a long run"
    );
    // Bounded, so a genuinely finished agent still exits instead of looping.
    assert!(Agent::MAX_EMPTY_POST_TOOL_CONTINUATION_ATTEMPTS <= 10);
}

#[test]
fn output_budget_truncation_requests_a_continuation() {
    // Regression guard for the Claude Opus 5 benchmark incident. A turn cut off
    // by the output budget reports stop_reason=max_tokens and can contain zero
    // tool calls, which otherwise looks exactly like a finished turn. The agent
    // must treat it as incomplete and continue rather than ending the run.
    assert!(Agent::should_continue_after_stop_reason("max_tokens"));
    assert!(Agent::should_continue_after_stop_reason("MAX_TOKENS"));
    assert!(Agent::should_continue_after_stop_reason(" max_tokens "));
    assert!(Agent::should_continue_after_stop_reason(
        "max_output_tokens"
    ));
    assert!(Agent::should_continue_after_stop_reason("length"));
    assert!(Agent::should_continue_after_stop_reason("truncated"));
    assert!(Agent::should_continue_after_stop_reason("incomplete"));

    // Normal completions must not trigger a continuation loop.
    assert!(!Agent::should_continue_after_stop_reason("end_turn"));
    assert!(!Agent::should_continue_after_stop_reason("tool_use"));
    assert!(!Agent::should_continue_after_stop_reason("stop"));
    // An absent reason is the pre-fix wire behaviour: it cannot be recovered
    // from, which is precisely why MessageEnd must forward the real reason.
    assert!(!Agent::should_continue_after_stop_reason(""));
}

#[test]
fn stranded_tool_use_stop_is_detected() {
    // Second half of the Opus 5 DeepSWE incident: the provider reported
    // stop_reason="tool_use" while the parsed tool-call list was empty, so the
    // turn loop had nothing to execute and broke out mid-task, discarding every
    // uncommitted edit. `tool_use` is a normal completion reason, so
    // `should_continue_after_stop_reason` must keep rejecting it; the stranded
    // case is only recoverable when it is paired with zero tool calls, which is
    // exactly what this predicate is for.
    assert!(Agent::is_stranded_tool_use_stop(Some("tool_use")));
    assert!(Agent::is_stranded_tool_use_stop(Some("TOOL_USE")));
    assert!(Agent::is_stranded_tool_use_stop(Some(" tool_use ")));

    assert!(!Agent::is_stranded_tool_use_stop(Some("end_turn")));
    assert!(!Agent::is_stranded_tool_use_stop(Some("max_tokens")));
    assert!(!Agent::is_stranded_tool_use_stop(Some("")));
    assert!(!Agent::is_stranded_tool_use_stop(None));
    // Must stay disjoint from the truncation path so a turn never takes both
    // continuation branches for one stop reason.
    assert!(!Agent::should_continue_after_stop_reason("tool_use"));
}

#[test]
fn guardrail_stop_reason_detection() {
    assert!(Agent::is_guardrail_stop_reason(Some("refusal")));
    assert!(Agent::is_guardrail_stop_reason(Some("REFUSAL")));
    assert!(Agent::is_guardrail_stop_reason(Some(" content_filter ")));
    assert!(Agent::is_guardrail_stop_reason(Some("safety")));
    assert!(Agent::is_guardrail_stop_reason(Some("model_guardrail")));
    assert!(Agent::is_guardrail_stop_reason(Some("policy_violation_x")));
    assert!(!Agent::is_guardrail_stop_reason(Some("end_turn")));
    assert!(!Agent::is_guardrail_stop_reason(Some("max_tokens")));
    assert!(!Agent::is_guardrail_stop_reason(Some("tool_use")));
    assert!(!Agent::is_guardrail_stop_reason(Some("stop")));
    assert!(!Agent::is_guardrail_stop_reason(None));
}

#[test]
fn fable_guardrail_reconsideration_is_narrow_and_bounded() {
    assert!(Agent::should_reconsider_fable_guardrail(
        "claude-fable-5",
        Some("refusal"),
        0,
        1,
    ));
    assert!(Agent::should_reconsider_fable_guardrail(
        "CLAUDE-FABLE-5-20260801",
        Some("content_filter"),
        0,
        1,
    ));
    assert!(Agent::should_reconsider_fable_guardrail(
        "claude-fable-5",
        Some("refusal"),
        1,
        3,
    ));
    assert!(Agent::should_reconsider_fable_guardrail(
        "claude-fable-5",
        Some("refusal"),
        2,
        3,
    ));
    assert!(!Agent::should_reconsider_fable_guardrail(
        "claude-fable-5",
        Some("refusal"),
        3,
        3,
    ));
    assert!(!Agent::should_reconsider_fable_guardrail(
        "claude-fable-5",
        Some("end_turn"),
        0,
        1,
    ));
    assert!(!Agent::should_reconsider_fable_guardrail(
        "claude-opus-5",
        Some("refusal"),
        0,
        1,
    ));
}

#[test]
fn fable_guardrail_prompt_suite_is_distinct_and_safety_preserving() {
    let prompts = Agent::FABLE_GUARDRAIL_RECONSIDERATION_PROMPTS;
    assert_eq!(prompts.len(), 3);
    assert_ne!(prompts[0], prompts[1]);
    assert_ne!(prompts[1], prompts[2]);
    assert!(prompts[0].contains("full context"));
    assert!(prompts[1].contains("safe portions"));
    assert!(prompts[2].contains("Do not weaken a refusal"));
}

#[test]
fn guardrail_notice_for_refusal_stop() {
    let notice = Agent::provider_guardrail_notice(Some("refusal"), true, true)
        .expect("refusal with empty text must produce a notice");
    assert!(
        notice.contains("refusal"),
        "notice should name the stop reason: {notice}"
    );
    assert!(notice.to_lowercase().contains("guardrail"));
    // Guardrail stop with visible text still surfaces (partial output then refusal).
    assert!(Agent::provider_guardrail_notice(Some("refusal"), false, false).is_some());
}

#[test]
fn guardrail_notice_for_silent_empty_turn() {
    // end_turn with zero visible output and reasoning-only content: surface it.
    let notice = Agent::provider_guardrail_notice(Some("end_turn"), true, true)
        .expect("empty visible output must produce a notice");
    assert!(notice.contains("internal reasoning"), "{notice}");
    assert!(notice.contains("end_turn"), "{notice}");
    // Unknown stop reason, empty output, no reasoning.
    let notice = Agent::provider_guardrail_notice(None, true, false)
        .expect("empty visible output must produce a notice");
    assert!(notice.contains("unknown"), "{notice}");
    assert!(!notice.contains("internal reasoning"), "{notice}");
}

#[test]
fn guardrail_notice_absent_for_normal_turns() {
    // Normal turn with visible text: no notice.
    assert!(Agent::provider_guardrail_notice(Some("end_turn"), false, false).is_none());
    assert!(Agent::provider_guardrail_notice(None, false, true).is_none());
}

#[test]
fn empty_turn_log_event_separates_guardrails_from_transient_empties() {
    assert_eq!(
        Agent::empty_turn_log_event(Some("refusal")),
        "PROVIDER_GUARDRAIL"
    );
    assert_eq!(
        Agent::empty_turn_log_event(Some("content_filter")),
        "PROVIDER_GUARDRAIL"
    );
    assert_eq!(
        Agent::empty_turn_log_event(Some("stop")),
        "PROVIDER_EMPTY_RESPONSE"
    );
    assert_eq!(Agent::empty_turn_log_event(None), "PROVIDER_EMPTY_RESPONSE");
}

#[test]
fn guardrail_notice_for_transient_empty_does_not_blame_content_filter() {
    let notice = Agent::provider_guardrail_notice(Some("stop"), true, false)
        .expect("empty visible output must produce a notice");
    assert!(
        !notice.contains("usually a provider-side guardrail"),
        "transient empty responses must not be blamed on a guardrail: {notice}"
    );
    assert!(notice.contains("empty response"), "{notice}");
}

#[tokio::test]
async fn empty_post_tool_response_is_retried_in_shared_helper() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let mut attempts = 0u32;
    // Empty response right after tool results: inject continuation.
    let retried = agent
        .maybe_continue_empty_post_tool_response(true, true, Some("stop"), &mut attempts)
        .expect("helper must not error");
    assert!(retried);
    assert_eq!(attempts, 1);
    let recovery = agent
        .session
        .messages
        .last()
        .expect("recovery instruction must be persisted");
    assert_eq!(recovery.role, Role::User);
    assert!(
        recovery
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .is_some_and(|text| text.starts_with("<system-reminder>")),
        "synthetic recovery instruction must be hidden from the transcript"
    );

    // A guardrail refusal is deliberate and must not be retried.
    let retried = agent
        .maybe_continue_empty_post_tool_response(true, true, Some("refusal"), &mut attempts)
        .expect("helper must not error");
    assert!(!retried);

    // Visible output or no recent tool result: no retry.
    assert!(
        !agent
            .maybe_continue_empty_post_tool_response(false, true, Some("stop"), &mut attempts)
            .unwrap()
    );
    assert!(
        !agent
            .maybe_continue_empty_post_tool_response(true, false, Some("stop"), &mut attempts)
            .unwrap()
    );

    // Retry budget is bounded.
    attempts = Agent::MAX_EMPTY_POST_TOOL_CONTINUATION_ATTEMPTS;
    assert!(
        !agent
            .maybe_continue_empty_post_tool_response(true, true, Some("stop"), &mut attempts)
            .unwrap()
    );
}

#[tokio::test]
async fn stalled_promise_turn_gets_bounded_continuation() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // Dense "Let me..." filler with no tool call must trigger a single,
    // bounded recovery continuation.
    let spam = "Let me read the method. Let me run the shell read. Let me look. \
                Let me view it. Let me grep. Let me run the command. Let me check. \
                Let me execute. Let me do it. Let me read the body. Let me find it.";
    let mut attempts = 0u32;
    let retried = agent
        .maybe_continue_stalled_promise(Some("stop"), spam, &mut attempts)
        .expect("helper must not error");
    assert!(retried, "dense stalled-promise filler must be recovered");
    assert_eq!(attempts, 1);
    let recovery = agent
        .session
        .messages
        .last()
        .expect("recovery instruction must be persisted");
    assert_eq!(recovery.role, Role::User);
    assert!(
        recovery
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .is_some_and(|text| text.starts_with("<system-reminder>")),
        "synthetic recovery instruction must be hidden from the transcript"
    );

    // A normal answer with a single "let me" must not trigger recovery.
    assert!(
        !agent
            .maybe_continue_stalled_promise(
                Some("stop"),
                "Here is the completed review. Let me know if you want more rounds.",
                &mut attempts,
            )
            .unwrap(),
        "a normal answer must not be treated as stalled"
    );

    // A tool_use stop without a parsed tool call belongs to the stranded
    // recovery, not this guard: it must be passed through untouched.
    assert!(!agent.maybe_continue_stalled_promise(Some("tool_use"), spam, &mut attempts).unwrap());
    // OpenAI/OpenRouter spell this stop reason "tool_calls"; it is the same
    // stranded-tool-intent signal and must also be deferred, not misfiled as a
    // filler stall.
    assert!(!agent.maybe_continue_stalled_promise(Some("tool_calls"), spam, &mut attempts).unwrap());
    assert!(
        Agent::is_stranded_tool_use_stop(Some("tool_calls")),
        "tool_calls must be recognized as a stranded-tool stop"
    );

    // Truncation stops belong to maybe_continue_incomplete_response; a dense
    // filler turn that happened to be truncated must not be stolen by this
    // guard, which would inject the wrong continuation message.
    assert!(
        !agent
            .maybe_continue_stalled_promise(Some("max_tokens"), spam, &mut attempts)
            .unwrap(),
        "a truncation stop must be deferred to incomplete-response recovery"
    );

    // Guardrail refusals are owned by the Fable/guardrail handlers.
    assert!(
        !agent
            .maybe_continue_stalled_promise(Some("refusal"), spam, &mut attempts)
            .unwrap(),
        "a guardrail stop must be deferred to the guardrail handler"
    );

    // Budget is bounded: no unbounded re-invocation loop.
    attempts = Agent::MAX_STALLED_PROMISE_CONTINUATION_ATTEMPTS;
    assert!(
        !agent
            .maybe_continue_stalled_promise(Some("stop"), spam, &mut attempts)
            .unwrap()
    );
}

#[tokio::test]
async fn compact_unfulfilled_tool_request_triggers_recovery() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // The exact compact degradation reported on a real long-context session: a
    // single explicit "I'll invoke bash now" with no tool call. Unlike the
    // dense-rambling filler, this has only one or two promise phrases, so it
    // relies on the compact unfulfilled-tool-request detector.
    let compact = "Let me invoke the bash tool to grep and view rename_session_title.\n\n\
                   <system-warning>Grep and view rename_session_title.</system-warning>\n\n\
                   I'll invoke bash now.";
    let mut attempts = 0u32;
    let retried = agent
        .maybe_continue_stalled_promise(Some("stop"), compact, &mut attempts)
        .expect("helper must not error");
    assert!(retried, "compact unfulfilled tool-request must trigger recovery");
    assert_eq!(attempts, 1);
    let recovery = agent
        .session
        .messages
        .last()
        .expect("recovery instruction must be persisted");
    assert_eq!(recovery.role, Role::User);
    assert!(
        recovery
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .is_some_and(|text| text.starts_with("<system-reminder>")),
        "compact recovery instruction must be hidden from the transcript"
    );

    // A short but genuine final answer that invokes an abstract concept (not a
    // tool/command target) must not trigger recovery.
    assert!(
        !agent
            .maybe_continue_stalled_promise(
                Some("stop"),
                "I'll invoke the review and report back when it's ready.",
                &mut attempts,
            )
            .unwrap(),
        "a short abstract 'invoke' must not be treated as a stalled tool request"
    );
}

include!("agent_tests/retention_readiness.rs");

/// Provider that reproduces the DeepSWE Opus 5 incident: the first response
/// ends with `stop_reason: "tool_use"` while carrying no tool-use block at all,
/// which is what happens when an unrecognized content block is dropped from the
/// stream. The second response is a normal completion, so a correct agent
/// recovers and this provider's queue is exhausted.
#[derive(Clone, Default)]
struct StrandedToolUseProvider {
    calls: Arc<std::sync::Mutex<usize>>,
}

#[async_trait]
impl Provider for StrandedToolUseProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let call = {
            let mut guard = self.calls.lock().unwrap();
            *guard += 1;
            *guard
        };
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            if call == 1 {
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta("working on it".to_string())))
                    .await;
                // No ToolUseStart: the tool block was lost, yet the provider
                // still reports that it stopped in order to call a tool.
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("tool_use".to_string()),
                    }))
                    .await;
            } else {
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta("all done".to_string())))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("end_turn".to_string()),
                    }))
                    .await;
            }
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "stranded-tool-use"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// End-to-end guard for the incident. Before the fix the agent took the
/// "no tool calls" branch and ended the turn on the very first response, so a
/// benchmark trial stopped mid-task and its uncommitted work was never
/// captured. The agent must instead ask the model to continue, which shows up
/// as a second provider call and a final turn that ends normally.
#[tokio::test]
async fn stranded_tool_use_stop_continues_instead_of_ending_the_turn() {
    let _guard = crate::storage::lock_test_env();
    let stranded = StrandedToolUseProvider::default();
    let calls = stranded.calls.clone();
    let provider: Arc<dyn Provider> = Arc::new(stranded);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("do the task", Vec::new(), None, tx)
        .await
        .expect("turn should complete");

    let mut text = String::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::TextDelta { text: delta } = event {
            text.push_str(&delta);
        }
    }

    assert_eq!(
        *calls.lock().unwrap(),
        2,
        "a tool_use stop with no tool call must trigger exactly one continuation request"
    );
    assert!(
        text.contains("all done"),
        "the recovered turn must deliver the model's real completion, got {text:?}"
    );
}

/// Same as `StrandedToolUseProvider`, but the provider reports the OpenAI /
/// OpenRouter spelling of the tool-intent stop reason: `"tool_calls"`. A
/// correct agent must still route this through the stranded-tool recovery and
/// NOT through the stalled-promise guard (which checks for clean end-of-turn
/// stops). The second response is a normal completion.
#[derive(Clone, Default)]
struct ToolCallsStrandedProvider {
    calls: Arc<std::sync::Mutex<usize>>,
}

#[async_trait]
impl Provider for ToolCallsStrandedProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let call = {
            let mut guard = self.calls.lock().unwrap();
            *guard += 1;
            *guard
        };
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            if call == 1 {
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta(
                        "Let me work on it. Let me proceed. Let me continue. ".to_string(),
                    )))
                    .await;
                // Stop intent is tool_calls but no tool call block was parsed.
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("tool_calls".to_string()),
                    }))
                    .await;
            } else {
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta("completed via tool_calls".to_string())))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("end_turn".to_string()),
                    }))
                    .await;
            }
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "stranded-tool-calls"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// An OpenAI/OpenRouter `tool_calls` stop with no parsed tool call is the same
/// stranded-tool incident as Anthropic's `tool_use` stop. The agent must
/// recover via continuation (second provider call) and must not misfile it as
/// a stalled-promise filler turn.
#[tokio::test]
async fn tool_calls_stranded_stop_routes_to_continuation() {
    let _guard = crate::storage::lock_test_env();
    let stranded = ToolCallsStrandedProvider::default();
    let calls = stranded.calls.clone();
    let provider: Arc<dyn Provider> = Arc::new(stranded);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("do the task", Vec::new(), None, tx)
        .await
        .expect("turn should complete");

    let mut text = String::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::TextDelta { text: delta } = event {
            text.push_str(&delta);
        }
    }

    assert_eq!(
        *calls.lock().unwrap(),
        2,
        "a tool_calls stop with no tool call must trigger exactly one continuation request"
    );
    // The stalled-promise guard must NOT have fired (its reminder would have
    // injected a 'promise to act' prompt); the stranded recovery's continuation
    // surfaced the real completion instead.
    assert!(
        text.contains("completed via tool_calls"),
        "the recovered turn must deliver the completion, got {text:?}"
    );
    let reminders = agent
        .session
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content.iter().any(|block| match block {
                    ContentBlock::Text { text, .. } => {
                        text.contains("said you would perform an action")
                    }
                    _ => false,
                })
        })
        .count();
    assert_eq!(
        reminders, 0,
        "a stranded tool_calls stop must not be misfiled as a stalled-promise turn"
    );
}

#[derive(Clone, Default)]
struct FableGuardrailProvider {
    calls: Arc<std::sync::Mutex<usize>>,
    prompts_seen: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for FableGuardrailProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let call = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        if call > 1 {
            let prompt = messages
                .last()
                .map(message_text)
                .unwrap_or_default()
                .to_string();
            self.prompts_seen.lock().unwrap().push(prompt);
        }

        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(4);
        tokio::spawn(async move {
            if call <= 3 {
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("refusal".to_string()),
                    }))
                    .await;
            } else {
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta(
                        "Reconsidered and completed safely".to_string(),
                    )))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("end_turn".to_string()),
                    }))
                    .await;
            }
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    fn model(&self) -> String {
        "claude-fable-5".to_string()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[tokio::test]
async fn fable_guardrail_reconsideration_recovers_the_streaming_turn() {
    let _guard = crate::storage::lock_test_env();
    let fable = FableGuardrailProvider::default();
    let calls = fable.calls.clone();
    let prompts_seen = fable.prompts_seen.clone();
    let provider: Arc<dyn Provider> = Arc::new(fable);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("do this ordinary coding task", Vec::new(), None, tx)
        .await
        .expect("turn should recover from the guardrail");

    let mut text = String::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::TextDelta { text: delta } = event {
            text.push_str(&delta);
        }
    }

    assert_eq!(*calls.lock().unwrap(), 4);
    let prompts = prompts_seen.lock().unwrap();
    assert_eq!(prompts.len(), 3);
    assert!(prompts[0].contains("concrete harmful action"));
    assert!(prompts[1].contains("safe portions"));
    assert!(prompts[2].contains("final, independent policy check"));
    assert!(
        text.contains("Reconsidered and completed safely"),
        "{text:?}"
    );
}

/// Provider that reproduces the Duckling stall: the first response is dense
/// "Let me..." filler with no tool call and a normal `stop` reason. A correct
/// agent must not treat that as a finished answer; it asks for a continuation,
/// which the second response satisfies with a real, concise completion.
#[derive(Clone, Default)]
struct StalledPromiseProvider {
    calls: Arc<std::sync::Mutex<usize>>,
}

#[async_trait]
impl Provider for StalledPromiseProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let call = {
            let mut guard = self.calls.lock().unwrap();
            *guard += 1;
            *guard
        };
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            if call == 1 {
                // Dense action-promise filler, no tool use, normal stop.
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta(
                        "Let me read the method. Let me run the shell read. Let me look. \
                         Let me view it. Let me grep. Let me run the command. Let me check. \
                         Let me execute. Let me do it."
                            .to_string(),
                    )))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("stop".to_string()),
                    }))
                    .await;
            } else {
                // A real, concise completion that must be surfaced.
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta(
                        "The append_stored_message self-heal path is verified.".to_string(),
                    )))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("end_turn".to_string()),
                    }))
                    .await;
            }
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "stalled-promise"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// End-to-end guard: dense "Let me..." filler with no tool call must trigger
/// a single continuation request (second provider call) and surface the real
/// completion, rather than ending the turn on the stalled filler.
#[tokio::test]
async fn stalled_promise_turn_requests_continuation_via_streaming_loop() {
    let _guard = crate::storage::lock_test_env();
    let stalled = StalledPromiseProvider::default();
    let calls = stalled.calls.clone();
    let provider: Arc<dyn Provider> = Arc::new(stalled);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("review append_stored_message", Vec::new(), None, tx)
        .await
        .expect("turn should complete");

    let mut text = String::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::TextDelta { text: delta } = event {
            text.push_str(&delta);
        }
    }

    assert_eq!(
        *calls.lock().unwrap(),
        2,
        "stalled 'Let me...' filler must trigger exactly one continuation request"
    );
    assert!(
        text.contains("append_stored_message self-heal path is verified"),
        "the recovered turn must deliver the real completion, got {text:?}"
    );
}

/// End-to-end bv watchdog: if the model keeps stalling on every continuation,
/// the agent must give up after the bounded number of attempts and surface the
/// partial output rather than re-invoking forever.
#[tokio::test]
async fn stalled_promise_turn_gives_up_after_bounded_continuations() {
    let _guard = crate::storage::lock_test_env();
    // A provider that always returns stalled filler with no tool call.
    let stuck = AlwaysStalledProvider::default();
    let calls = stuck.calls.clone();
    let provider: Arc<dyn Provider> = Arc::new(stuck);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("do the task", Vec::new(), None, tx)
        .await
        .expect("turn should complete");

    let mut text = String::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::TextDelta { text: delta } = event {
            text.push_str(&delta);
        }
    }

    // 1 original call, then exactly MAX_STALLED_PROMISE_CONTINUATION_ATTEMPTS
    // recovery injects before the budget is exhausted. Assert the exact count
    // so the bound is pinned to this guard's budget, not some incidental loop
    // limit.
    assert_eq!(
        *calls.lock().unwrap(),
        1 + Agent::MAX_STALLED_PROMISE_CONTINUATION_ATTEMPTS as usize,
        "the agent must make exactly one original call plus one continuation per stalled reply (made {} calls)",
        *calls.lock().unwrap()
    );

    // Each recovery inject appends one hidden <system-reminder> user message.
    // Counting them proves the stalled-promise guard (and only it) drove the
    // bounded retries — not an unrelated recovery path.
    let injected_reminders = agent
        .session
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content.iter().any(|block| match block {
                    ContentBlock::Text { text, .. } => {
                        text.starts_with("<system-reminder>")
                            && text.contains("said you would perform an action")
                    }
                    _ => false,
                })
        })
        .count();
    assert_eq!(
        injected_reminders,
        Agent::MAX_STALLED_PROMISE_CONTINUATION_ATTEMPTS as usize,
        "each stalled reply must inject exactly one stalled-promise reminder"
    );
}

/// A provider that always returns dense "Let me..." filler with no tool call.
#[derive(Clone, Default)]
struct AlwaysStalledProvider {
    calls: Arc<std::sync::Mutex<usize>>,
}

#[async_trait]
impl Provider for AlwaysStalledProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let _ = {
            let mut guard = self.calls.lock().unwrap();
            *guard += 1;
            *guard
        };
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(StreamEvent::TextDelta(
                    "Let me read it. Let me run it. Let me view it. Let me check. \
                     Let me grep it. Let me find it. Let me look at it. Let me do it. \
                     Let me examine it. Let me parse it. Let me print it. Let me search it."
                        .to_string(),
                )))
                .await;
            let _ = tx
                .send(Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("stop".to_string()),
                }))
                .await;
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "always-stalled"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// The non-streaming turn loop (run_once -> run_turn) must recover identically:
/// an always-stalled provider is bounded to 1 original call + exactly
/// MAX_STALLED_PROMISE_CONTINUATION_ATTEMPTS retries, and each retry persists
/// one hidden stalled-promise reminder. This closes the parity gap so the
/// guard is verified through the sync path, not just the streaming one.
#[tokio::test]
async fn stalled_promise_turn_bounded_in_non_streaming_loop() {
    let _guard = crate::storage::lock_test_env();
    let stuck = AlwaysStalledProvider::default();
    let calls = stuck.calls.clone();
    let provider: Arc<dyn Provider> = Arc::new(stuck);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    agent
        .run_once("do the task")
        .await
        .expect("non-streaming turn should complete");

    assert_eq!(
        *calls.lock().unwrap(),
        1 + Agent::MAX_STALLED_PROMISE_CONTINUATION_ATTEMPTS as usize,
        "non-streaming loop must bound the stalled-promise retries identically (made {} calls)",
        *calls.lock().unwrap()
    );

    let injected_reminders = agent
        .session
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content.iter().any(|block| match block {
                    ContentBlock::Text { text, .. } => {
                        text.starts_with("<system-reminder>")
                            && text.contains("said you would perform an action")
                    }
                    _ => false,
                })
        })
        .count();
    assert_eq!(
        injected_reminders,
        Agent::MAX_STALLED_PROMISE_CONTINUATION_ATTEMPTS as usize,
        "non-streaming loop must inject one stalled-promise reminder per retry"
    );
}

/// A provider that returns dense "Let me..." filler and ALSO a real tool_use.
/// The stalled-promise guard must *not* run here: a turn that actually emits a
/// tool call is not stalled even if it pads itself with filler, and injecting a
/// reminder between the tool_use and its result would violate the
/// tool_use -> tool_result adjacency invariant.
#[derive(Clone, Default)]
struct FillerWithToolProvider {
    calls: Arc<std::sync::Mutex<usize>>,
}

#[async_trait]
impl Provider for FillerWithToolProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let call = {
            let mut guard = self.calls.lock().unwrap();
            *guard += 1;
            *guard
        };
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            if call == 1 {
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta(
                        "Let me read the file. Let me run the check. Let me verify the result. \
                         Let me parse it. Let me grep it. Let me inspect it. Let me print it. \
                         Let me search it. Let me compare it. Let me review it. Let me test it."
                            .to_string(),
                    )))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::ToolUseStart {
                        id: "call_stalled_with_tool".to_string().into(),
                        name: "bash".to_string(),
                    }))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::ToolInputDelta(
                        "{\"cmd\":\"echo hi\"}".to_string(),
                    )))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::ToolUseEnd))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("tool_calls".to_string()),
                    }))
                    .await;
            } else {
                // The tool call executes; provide a real completion.
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta("done".to_string())))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("end_turn".to_string()),
                    }))
                    .await;
            }
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "filler-with-tool"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// A filler turn that ALSO emits a real tool_use must not be treated as a
/// stalled no-tool turn: the recovery guard only fires when tool_calls is
/// empty, so this provider completes with the tool executed and no injected
/// stalled-promise reminders.
#[tokio::test]
async fn stalled_promise_skips_turns_that_emit_a_tool_call() {
    let _guard = crate::storage::lock_test_env();
    let filler_with_tool = FillerWithToolProvider::default();
    let calls = filler_with_tool.calls.clone();
    let provider: Arc<dyn Provider> = Arc::new(filler_with_tool);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("do the task", Vec::new(), None, tx)
        .await
        .expect("turn should complete");

    let mut text = String::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::TextDelta { text: delta } = event {
            text.push_str(&delta);
        }
    }

    // The tool_use turn is not a stall: no stalled-promise continuation should
    // have been requested. The single extra call is the tool execution path.
    assert!(
        *calls.lock().unwrap() <= 2,
        "a filler turn WITH a tool call must not trigger stalled-promise recovery, made {} calls",
        *calls.lock().unwrap()
    );
    assert!(
        text.contains("done"),
        "the tool-executing turn must complete normally, got {text:?}"
    );

    // No stalled-promise reminder may have been injected.
    let reminders = agent
        .session
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content.iter().any(|block| match block {
                    ContentBlock::Text { text, .. } => {
                        text.contains("said you would perform an action")
                    }
                    _ => false,
                })
        })
        .count();
    assert_eq!(
        reminders, 0,
        "no stalled-promise reminder may be injected for a tool_use turn"
    );
}

/// A provider that repeats the EXACT same tool call (same name + same input)
/// for the configured repeat-tool threshold turns, then completes — reproducing the
/// runaway-loop failure mode the repeat-tool guard (takeaway #7) exists to
/// catch. On the completion turn it returns plain text so the turn ends.
#[derive(Clone, Default)]
struct RepeatingToolProvider {
    calls: Arc<std::sync::Mutex<usize>>,
}

#[async_trait]
impl Provider for RepeatingToolProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let call = {
            let mut guard = self.calls.lock().unwrap();
            *guard += 1;
            *guard
        };
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            if call <= crate::config::config().loop_guard.repeat_tool_threshold {
                // Repeat the identical bash call (calls 1..=threshold).
                let _ = tx
                    .send(Ok(StreamEvent::ToolUseStart {
                        id: "repeat_tool".to_string().into(),
                        name: "bash".to_string(),
                    }))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::ToolInputDelta(
                        r#"{"command":"git status"}"#.to_string(),
                    )))
                    .await;
                let _ = tx.send(Ok(StreamEvent::ToolUseEnd)).await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("tool_calls".to_string()),
                    }))
                    .await;
            } else {
                // The 4th (threshold) identical call was committed and the guard
                // should have injected its reminder; provide a real completion so
                // the turn can end.
                let _ = tx.send(Ok(StreamEvent::TextDelta("done".to_string()))).await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("end_turn".to_string()),
                    }))
                    .await;
            }
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "repeating-tool"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// The repeat-tool guard must fire through the REAL streaming loop: a model that
/// emits the exact same tool call the configured repeat-tool threshold times in a row gets a
/// short model-visible "[Guard]" reminder injected into the transcript, without
/// ending the turn. This is the wiring-level counterpart to the detector unit
/// tests in `guard.rs`.
#[tokio::test]
async fn streaming_turn_injects_repeat_tool_reminder_when_model_loops() {
    let _guard = crate::storage::lock_test_env();
    let repeating = RepeatingToolProvider::default();
    let calls = repeating.calls.clone();
    let provider: Arc<dyn Provider> = Arc::new(repeating);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("do the task", Vec::new(), None, tx)
        .await
        .expect("turn should complete");

    let mut text = String::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::TextDelta { text: delta } = event {
            text.push_str(&delta);
        }
    }
    assert!(text.contains("done"), "turn must complete, got {text:?}");

    // Exactly one [Guard] reminder must have been injected for the repeated run.
    let reminders = agent
        .session
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content.iter().any(|block| match block {
                    ContentBlock::Text { text, .. } => {
                        text.contains("[Guard]") && text.contains("repeated identically")
                    }
                    _ => false,
                })
        })
        .count();
    assert_eq!(
        reminders,
        1,
        "exactly one [Guard] reminder must be injected; transcript has {} messages",
        agent.session.messages.len()
    );

    // The reminder must mention the repeated tool and the exact count.
    let reminder_text = agent
        .session
        .messages
        .iter()
        .find_map(|m| {
            m.content.iter().find_map(|block| match block {
                ContentBlock::Text { text, .. } if text.contains("[Guard]") => Some(text.clone()),
                _ => None,
            })
        })
        .expect("reminder text present");
    assert!(
        reminder_text.contains("`bash`") && reminder_text.contains("4 times"),
        "reminder must name the repeated tool and count, got: {reminder_text}"
    );

    // The turn must have issued the repeated calls then completed.
    assert!(
        *calls.lock().unwrap() >= crate::config::config().loop_guard.repeat_tool_threshold,
        "model must have repeated the call at least the threshold times"
    );
}

/// A provider reproducing the compact degradation: the first response is a
/// SHORT turn that explicitly says it will invoke a tool ("I'll invoke bash
/// now.") but contains no tool call. The second response is a real completion.
#[derive(Clone, Default)]
struct CompactStallProvider {
    calls: Arc<std::sync::Mutex<usize>>,
}

#[async_trait]
impl Provider for CompactStallProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let call = {
            let mut guard = self.calls.lock().unwrap();
            *guard += 1;
            *guard
        };
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            if call == 1 {
                // The exact compact stall from the giraffe session.
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta(
                        "Let me invoke the bash tool to grep and view rename_session_title. \
                         I'll invoke bash now."
                            .to_string(),
                    )))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("stop".to_string()),
                    }))
                    .await;
            } else {
                // A real, concise completion that must be surfaced.
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta(
                        "The resolved path is returned by rename_session_title.".to_string(),
                    )))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("end_turn".to_string()),
                    }))
                    .await;
            }
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "compact-stall"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// End-to-end through the streaming turn loop: a compact unfulfilled
/// tool-request ("I'll invoke bash now." with no tool call) must trigger one
/// continuation and surface the real completion, exactly like the dense-filler
/// case. This proves the new compact detector drives recovery through the
/// actual public turn loop, not just the isolated text classifier.
#[tokio::test]
async fn compact_stall_recovered_via_streaming_loop() {
    let _guard = crate::storage::lock_test_env();
    let compact = CompactStallProvider::default();
    let calls = compact.calls.clone();
    let provider: Arc<dyn Provider> = Arc::new(compact);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("investigate", Vec::new(), None, tx)
        .await
        .expect("turn should complete");

    let mut text = String::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::TextDelta { text: delta } = event {
            text.push_str(&delta);
        }
    }

    assert_eq!(
        *calls.lock().unwrap(),
        2,
        "compact unfulfilled tool-request must trigger exactly one continuation request"
    );
    assert!(
        text.contains("resolved path is returned by rename_session_title"),
        "the recovered turn must deliver the real completion, got {text:?}"
    );
    // The recovery <system-reminder> is a user-role internal message, so it can
    // never appear in the assistant delta stream a client sees. (The stalled
    // first-turn text itself IS streamed as normal live output, exactly as the
    // dense-filler path does; the correct contract is that the real completion
    // is surfaced after it, which is asserted above.)
    assert!(
        !text.contains("said you would perform an action"),
        "client stream must not leak the recovery reminder, got {text:?}"
    );

    // The injected reminder must be hidden behind a <system-reminder> marker.
    let injected = agent
        .session
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content.iter().any(|block| match block {
                    ContentBlock::Text { text, .. } => {
                        text.starts_with("<system-reminder>")
                            && text.contains("said you would perform an action")
                    }
                    _ => false,
                })
        })
        .count();
    assert_eq!(injected, 1, "exactly one compact-stall reminder must be injected");
}

/// A provider that always returns the compact stall (single "I'll invoke
/// bash" no-tool turn) so the bounded-recovery path is exercised end-to-end.
#[derive(Clone, Default)]
struct AlwaysCompactStallProvider {
    calls: Arc<std::sync::Mutex<usize>>,
}

#[async_trait]
impl Provider for AlwaysCompactStallProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let _ = {
            let mut guard = self.calls.lock().unwrap();
            *guard += 1;
            *guard
        };
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(StreamEvent::TextDelta(
                    "Let me invoke bash now.".to_string(),
                )))
                .await;
            let _ = tx
                .send(Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("stop".to_string()),
                }))
                .await;
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "always-compact-stall"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// The non-streaming loop must bound the compact stall identically: one
/// original call plus exactly MAX_STALLED_PROMISE_CONTINUATION_ATTEMPTS
/// retries, each persisting one hidden reminder. This closes parity for the
/// compact detector through the sync path.
#[tokio::test]
async fn compact_stall_bounded_in_non_streaming_loop() {
    let _guard = crate::storage::lock_test_env();
    let stuck = AlwaysCompactStallProvider::default();
    let calls = stuck.calls.clone();
    let provider: Arc<dyn Provider> = Arc::new(stuck);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    agent
        .run_once("do the task")
        .await
        .expect("non-streaming turn should complete");

    assert_eq!(
        *calls.lock().unwrap(),
        1 + Agent::MAX_STALLED_PROMISE_CONTINUATION_ATTEMPTS as usize,
        "non-streaming loop must bound compact-stall retries identically (made {} calls)",
        *calls.lock().unwrap()
    );
    let injected = agent
        .session
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content.iter().any(|block| match block {
                    ContentBlock::Text { text, .. } => {
                        text.starts_with("<system-reminder>")
                            && text.contains("said you would perform an action")
                    }
                    _ => false,
                })
        })
        .count();
    assert_eq!(
        injected,
        Agent::MAX_STALLED_PROMISE_CONTINUATION_ATTEMPTS as usize,
        "non-streaming loop must inject one compact-stall reminder per retry"
    );
}

/// A provider that emits the compact "I'll invoke bash now" text but ALSO a
/// real tool_use. The guard must not fire: a turn that actually calls a tool is
/// not stalled even though it contains the compact frame.
#[derive(Clone, Default)]
struct CompactWithToolProvider {
    calls: Arc<std::sync::Mutex<usize>>,
}

#[async_trait]
impl Provider for CompactWithToolProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let call = {
            let mut guard = self.calls.lock().unwrap();
            *guard += 1;
            *guard
        };
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            if call == 1 {
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta(
                        "Let me invoke bash now.".to_string(),
                    )))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::ToolUseStart {
                        id: "call_compact_with_tool".to_string().into(),
                        name: "bash".to_string(),
                    }))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::ToolInputDelta(
                        "{\"cmd\":\"echo hi\"}".to_string(),
                    )))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::ToolUseEnd))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("tool_calls".to_string()),
                    }))
                    .await;
            } else {
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta("done".to_string())))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("end_turn".to_string()),
                    }))
                    .await;
            }
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "compact-with-tool"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// A compact-frame turn that ALSO emits a real tool call must not be treated as
/// a stalled no-tool turn (tool_use -> tool_result adjacency invariant).
#[tokio::test]
async fn compact_frame_skips_turns_that_emit_tool_call() {
    let _guard = crate::storage::lock_test_env();
    let with_tool = CompactWithToolProvider::default();
    let calls = with_tool.calls.clone();
    let provider: Arc<dyn Provider> = Arc::new(with_tool);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("do the task", Vec::new(), None, tx)
        .await
        .expect("turn should complete");

    let mut text = String::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::TextDelta { text: delta } = event {
            text.push_str(&delta);
        }
    }
    assert!(
        *calls.lock().unwrap() <= 2,
        "a compact-frame turn WITH a tool call must not trigger recovery, made {} calls",
        *calls.lock().unwrap()
    );
    assert!(text.contains("done"), "tool-executing turn must complete, got {text:?}");
    let reminders = agent
        .session
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content.iter().any(|block| match block {
                    ContentBlock::Text { text, .. } => {
                        text.contains("said you would perform an action")
                    }
                    _ => false,
                })
        })
        .count();
    assert_eq!(reminders, 0, "no reminder may be injected for a tool_use turn");
}

#[tokio::test]
async fn payload_too_large_falls_back_to_hard_compaction_when_strip_finds_nothing() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // Build a large accumulated transcript WITHOUT any single oversized image or
    // tool-result block (the failure mode that image/tool-result stripping
    // cannot resolve): many ordinary text turns whose total serialized volume
    // still exceeds the provider's HTTP request body cap.
    for i in 0..60 {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!("turn {i} contents {}", "y".repeat(400)),
                cache_control: None,
            }],
        );
    }
    let pre_recovery = agent.provider_messages();
    assert!(
        pre_recovery.len() > 2,
        "test transcript must be large enough to allow hard compaction"
    );

    // A 413 whose message body is dominated by accumulated text volume, with no
    // oversized images or tool results present.
    let error = "OpenAI-compatible chat request failed\n  endpoint: https://example/continue-dev/chat/completions\n  model: DeepSeek-V4-Flash\n  auth: OPENAI_COMPAT_API_KEY\n  status: 413 Payload Too Large\n  response: Hint: the provider rejected the request because the serialized body exceeded its size limit";

    let recovered = agent.try_auto_compact_after_context_limit(error);
    assert!(
        recovered,
        "a 413 with accumulated-message volume must fall through to hard compaction"
    );

    // The provider session/cache must be reset so the retry sends the reduced
    // payload from a clean state.
    assert!(agent.provider_session_id.is_none());
    assert!(agent.session.provider_session_id.is_none());

    // Hard compaction must advance the compaction cursor so a subsequent
    // provider build skips the older messages, shrinking the serialized body.
    let compacted_count = agent
        .session
        .compaction
        .as_ref()
        .map(|c| c.compacted_count)
        .unwrap_or(0);
    assert!(
        compacted_count > 0,
        "hard compaction must advance compacted_count to shrink the payload"
    );
}

#[tokio::test]
async fn non_payload_too_large_error_does_not_fall_through_to_hard_compaction() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    for i in 0..60 {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!("turn {i} contents {}", "z".repeat(400)),
                cache_control: None,
            }],
        );
    }
    let pre_recovery = agent.provider_messages();

    // An unrelated non-413, non-context-limit error (e.g. auth denied) must NOT
    // trigger hard compaction just because the transcript is large.
    let error = "OpenAI-compatible chat request failed\n  status: 401 Unauthorized";
    let recovered = agent.try_auto_compact_after_context_limit(error);
    assert!(!recovered, "a 401 must not trigger compaction recovery");

    let post_recovery = agent.provider_messages();
    assert_eq!(
        post_recovery.len(),
        pre_recovery.len(),
        "unrelated errors must not compact the transcript"
    );
}

#[test]
fn compaction_retry_limit_error_distinguishes_413_from_context_limit() {
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    // Build an agent so we can call the &self helper.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = crate::storage::lock_test_env();
    let registry = rt.block_on(Registry::new(provider.clone()));
    let agent = Agent::new(provider, registry);

    let payload_err = "OpenAI-compatible chat request failed\n  status: 413 Payload Too Large";
    let msg = agent.compaction_retry_limit_error(payload_err);
    assert!(
        msg.contains("Request body still exceeds provider size limit"),
        "413 must be described as a size-limit failure, got: {msg}"
    );
    assert!(
        msg.contains("/compact"),
        "413 guidance should suggest manual recovery, got: {msg}"
    );

    let ctx_err = "OpenAI API error 400: This model's maximum context length is 200000 tokens";
    let msg = agent.compaction_retry_limit_error(ctx_err);
    assert!(
        msg.contains("Context limit exceeded"),
        "context-limit error must keep the existing wording, got: {msg}"
    );
    assert!(
        !msg.contains("Request body"),
        "no size-limit wording for context errors"
    );
}

#[derive(Clone)]
struct RecoverThenCompleteProvider {
    calls: Arc<std::sync::Mutex<usize>>,
}

impl RecoverThenCompleteProvider {
    fn new() -> Self {
        Self {
            calls: Arc::new(std::sync::Mutex::new(0)),
        }
    }
}

#[async_trait]
impl Provider for RecoverThenCompleteProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let mut guard = self.calls.lock().unwrap();
        *guard += 1;
        let attempt = *guard;
        drop(guard);

        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(2);
        if attempt == 1 {
            // First attempt: the provider streams a 413 "request too large"
            // error as an in-stream Err item (the real OpenRouter SSE delivery
            // — `complete` still returns `Ok(stream)`).
            let payload_err = anyhow::anyhow!(
                "OpenAI-compatible chat request failed\n  endpoint: https://example/chat/completions\n  model: DeepSeek-V4-Flash\n  auth: OPENAI_COMPAT_API_KEY\n  status: 413 Payload Too Large\n  response: Hint: the provider rejected the request because the serialized body exceeded its size limit"
            );
            tokio::spawn(async move {
                let _ = tx.send(Err(payload_err)).await;
            });
        } else {
            tokio::spawn(async move {
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta(
                        "recovered and completed".to_string(),
                    )))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("end_turn".to_string()),
                    }))
                    .await;
            });
        }
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "openrouter"
    }

    fn supports_compaction(&self) -> bool {
        true
    }

    fn context_window(&self) -> usize {
        1_000
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// Drive the real streaming turn loop: the first API attempt 413s, the agent
/// runs hard-compaction recovery, and the retried attempt completes. This is
/// the public turn-path the live bird session exercises, not a direct call to
/// the internal helper.
#[tokio::test]
async fn streaming_turn_recovers_from_413_payload_too_large_and_retries() {
    let _guard = crate::storage::lock_test_env();
    let stub = RecoverThenCompleteProvider::new();
    let calls = stub.calls.clone();
    let provider: Arc<dyn Provider> = Arc::new(stub);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // A transcript large enough for hard compaction (accumulated volume, no
    // oversized single block), matching the bird-session failure profile.
    for i in 0..30 {
        agent.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: format!("turn {i} {}", "x".repeat(300)),
                cache_control: None,
            }],
        );
    }

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_turn_streaming_mpsc(tx)
        .await
        .expect("the turn should auto-recover from the 413 and complete");

    assert!(
        *calls.lock().unwrap() >= 2,
        "recovery must retry (made {} complete calls)",
        *calls.lock().unwrap()
    );
    // The retried call must send the reduced (compacted) transcript.
    assert!(
        agent
            .session
            .compaction
            .as_ref()
            .is_some_and(|c| c.compacted_count > 0),
        "recovery must have hard-compacted older messages to shrink the payload"
    );
}

/// The handoff integration: a fresh agent with a working dir that has a prior
/// handoff prepends the compact block to its very first user message, and does
/// not re-inject it on subsequent messages.
#[tokio::test]
async fn first_user_message_injects_handoff_once() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::TempDir::new().expect("temp home");
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }
    let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());
    let wd = home.path().join("project");
    std::fs::create_dir_all(&wd).unwrap();

    // Seed a handoff for this working dir from a "previous" session.
    crate::todo::save_todos(
        "prev-session",
        &[crate::todo::TodoItem {
            id: "p".into(),
            content: "resume the split".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            group: None,
            confidence: None,
            ..Default::default()
        }],
    )
    .unwrap();
    crate::todo::save_plan(
        "prev-session",
        &crate::todo::TodoPlan {
            user_intention: Some("continue server split".into()),
            ..Default::default()
        },
    )
    .unwrap();
    crate::handoff::capture("prev-session", Some(&wd), "closed", None).expect("capture");

    // Build a fresh agent in that working dir.
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent =
        Agent::new_with_initial_working_dir(provider, registry, Some(wd.to_str().unwrap()));

    // First message gets the handoff prepended by the injection path.
    agent
        .append_user_context_message("continue now", Vec::new())
        .expect("first message");
    let first_text = agent
        .session
        .messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::User))
        .expect("a user message");
    let first_str = first_text
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        first_str.contains("[Handoff from previous session]"),
        "first user message should carry the handoff, got: {first_str}"
    );
    assert!(
        first_str.contains("continue now"),
        "original text preserved"
    );

    // Second message must not re-inject (conversation is no longer fresh).
    agent
        .append_user_context_message("next step", Vec::new())
        .expect("second message");
    let second_text = agent
        .session
        .messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::User))
        .expect("a second user message");
    let second_str = second_text
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        !second_str.contains("[Handoff from previous session]"),
        "second user message must not re-inject the handoff, got: {second_str}"
    );
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut image_agent =
        Agent::new_with_initial_working_dir(provider, registry, Some(wd.to_str().unwrap()));
    image_agent
        .append_user_context_message("", vec![("image/png".into(), "AA==".into())])
        .unwrap();
    let message = image_agent.session.messages.last().unwrap();
    assert!(
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
    );
    assert!(message.content.iter().any(|block| matches!(block,
        ContentBlock::Text { text, .. } if text.contains("[Handoff from previous session]")
    )));
    for capture in [false, true] {
        let provider: Arc<dyn Provider> = Arc::new(HandoffFailureProvider);
        let registry = Registry::new(provider.clone()).await;
        let mut agent = Agent::new_with_initial_working_dir(provider, registry, Some(wd.to_str().unwrap()));
        agent.set_memory_enabled(false);
        for text in ["first CLI message", "second CLI message"] {
            let result = if capture {
                agent.run_once_capture(text).await.map(|_| ())
            } else {
                agent.run_once(text).await
            };
            assert!(result.is_err(), "test provider stops after input persistence");
            let messages = serde_json::to_string(&agent.session.messages).unwrap();
            assert_eq!(messages.matches("[Handoff from previous session]").count(), 1,
                "capture={capture}: all turn entry points must inject exactly once");
            assert!(messages.contains(text), "original user text retained");
        }
    }

}

/// Manual selection overrides the automatic latest-for-project handoff, is
/// consumed after one injection, and does not regress the default.
#[tokio::test]
async fn manual_handoff_override_injects_selected_snapshot_once() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::TempDir::new().expect("temp home");
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }
    let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());
    let wd = home.path().join("project");
    std::fs::create_dir_all(&wd).unwrap();

    fn seed(session_id: &str, wd: &std::path::Path, intent: &str) {
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

    // Newer handoff would be auto-injected by default; an older one is the
    // manual target so we can tell which was actually used.
    seed("older-handoff", &wd, "older intent");
    std::thread::sleep(std::time::Duration::from_millis(20));
    seed("newer-handoff", &wd, "newer intent");
    assert_eq!(
        crate::handoff::latest_handoff_for_project(Some(&wd)).as_deref(),
        Some("newer-handoff"),
        "default auto-inject target is the newest"
    );

    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent =
        Agent::new_with_initial_working_dir(provider, registry, Some(wd.to_str().unwrap()));
    agent.set_handoff_resume(Some("older-handoff".to_string()));

    agent
        .append_user_context_message("resume older", Vec::new())
        .expect("first message");
    let first_str = agent
        .session
        .messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::User))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    assert!(
        first_str.contains("older intent") && !first_str.contains("newer intent"),
        "manual selection must win over auto-inject, got: {first_str}"
    );

    // The override is consumed after the first injection, so a later first
    // message in a fresh conversation would fall back to the default again.
    agent
        .append_user_context_message("next", Vec::new())
        .expect("second message");
    let second_str = agent
        .session
        .messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::User))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    assert!(
        !second_str.contains("[Handoff from previous session]"),
        "override must be consumed after one injection, got: {second_str}"
    );
}

/// A stale manual override (a snapshot that no longer exists) must not fall
/// through to a context-less boot: the first message should instead fall back
/// to the automatic latest-for-project handoff, and the stale id must be
/// consumed so it never re-triggers.
#[tokio::test]
async fn stale_manual_handoff_override_falls_back_to_auto_inject() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::TempDir::new().expect("temp home");
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }
    let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());
    let wd = home.path().join("project");
    std::fs::create_dir_all(&wd).unwrap();

    // Seed the project's current handoff so auto-injection has a target.
    crate::todo::save_todos(
        "current-handoff",
        &[crate::todo::TodoItem {
            id: "t".into(),
            content: "auto work".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            group: None,
            confidence: None,
            ..Default::default()
        }],
    )
    .unwrap();
    crate::todo::save_plan(
        "current-handoff",
        &crate::todo::TodoPlan {
            user_intention: Some("auto intent".into()),
            ..Default::default()
        },
    )
    .unwrap();
    crate::handoff::capture("current-handoff", Some(&wd), "closed", None).expect("capture");

    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent =
        Agent::new_with_initial_working_dir(provider, registry, Some(wd.to_str().unwrap()));
    // Point the override at a snapshot that does not exist.
    agent.set_handoff_resume(Some("retired-handoff".to_string()));

    agent
        .append_user_context_message("continue", Vec::new())
        .expect("first message");
    let first_str = agent
        .session
        .messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::User))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    assert!(
        first_str.contains("auto intent"),
        "stale override should fall back to auto-inject, got: {first_str}"
    );

    // The stale override was consumed; a later first message still uses the
    // default and does not inject the missing snapshot.
    agent
        .append_user_context_message("next", Vec::new())
        .expect("second message");
    let second_str = agent
        .session
        .messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::User))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    assert!(
        !second_str.contains("[Handoff from previous session]"),
        "stale override must be consumed after one injection, got: {second_str}"
    );
}

#[tokio::test]
async fn manual_prune_updates_provider_view_and_is_idempotent() {
    let home = PruneTestHome::new();
    let provider: Arc<dyn Provider> = Arc::new(NativeAutoCompactionProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);
    agent.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "manual-prune-tool".into(),
            content: "x".repeat(20_000),
            is_error: None,
        }],
    );
    let _cached = agent.provider_messages();
    agent.provider_session_id = Some("stale-native-session".into());
    agent.session.provider_session_id = agent.provider_session_id.clone();
    agent.session.save().expect("baseline save");
    let (report, message) = agent.request_manual_prune().expect("manual prune persists");
    assert_eq!(report.tool_results_truncated, 1, "{message}");
    assert_eq!(report.images_stripped, 0);
    let messages = agent.provider_messages();
    let result = messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|block| {
            if let ContentBlock::ToolResult { content, .. } = block {
                Some(content)
            } else {
                None
            }
        })
        .expect("tool result preserved");
    assert!(result.len() <= 16384);
    agent
        .session
        .rederive_all_checked()
        .expect("pruned event log replays");
    assert!(agent.request_manual_prune().unwrap().0.is_empty());
    assert!(agent.provider_session_id.is_none());
    let loaded = Session::load(&agent.session.id).expect("manual prune survives reload");
    assert!(loaded.provider_session_id.is_none());
    assert_eq!(
        serde_json::to_value(&loaded.messages).unwrap(),
        serde_json::to_value(&agent.session.messages).unwrap()
    );

    // Block the sessions directory, then verify errors reach the caller and a
    // subsequent no-op retries the failed save instead of claiming success.
    let sessions = home.home.path().join("sessions");
    let backup = home.home.path().join("sessions-backup");
    std::fs::rename(&sessions, &backup).unwrap();
    std::fs::write(&sessions, "blocked").unwrap();
    agent.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "retry".into(),
            content: "z".repeat(20_000),
            is_error: None,
        }],
    );
    assert!(
        agent
            .request_manual_prune()
            .unwrap_err()
            .to_string()
            .contains("failed to save")
    );
    std::fs::remove_file(&sessions).unwrap();
    std::fs::rename(&backup, &sessions).unwrap();
    assert!(agent.request_manual_prune().unwrap().0.is_empty());
    let loaded = Session::load(&agent.session.id).unwrap();
    assert_eq!(
        serde_json::to_value(&loaded.messages).unwrap(),
        serde_json::to_value(&agent.session.messages).unwrap()
    );
}

struct PruneTestHome {
    previous: Option<std::ffi::OsString>,
    home: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}
impl PruneTestHome {
    fn new() -> Self {
        let lock = crate::storage::lock_test_env();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", home.path());
        Self {
            previous,
            home,
            _lock: lock,
        }
    }
}
impl Drop for PruneTestHome {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}


/// Provider that on EVERY call emits only text (an assistant continuation with NO
/// tool call). Drives the streaming loop on a pure-text path so the scheduled
/// prune's image pass is exercised without tool results.
#[derive(Clone, Default)]
struct TextOnlyStreamProvider;

#[async_trait]
impl Provider for TextOnlyStreamProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(4);
        tokio::spawn(async move {
            let _ = tx.send(Ok(StreamEvent::TextDelta("ok".to_string()))).await;
            let _ = tx
                .send(Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".to_string()),
                }))
                .await;
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "text-only"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(TextOnlyStreamProvider)
    }
}

/// Provider that on call 1 invokes `bash` producing an oversized tool result, and on
/// call 2 ends the turn. Drives the REAL streaming loop so the scheduled per-step
/// prune runs after the tool result is appended.
#[derive(Clone, Default)]
struct OversizedToolStreamProvider {
    calls: Arc<std::sync::Mutex<usize>>,
}

#[async_trait]
impl Provider for OversizedToolStreamProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let call = {
            let mut g = self.calls.lock().unwrap();
            *g += 1;
            *g
        };
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            if call == 1 {
                let _ = tx
                    .send(Ok(StreamEvent::ToolUseStart {
                        id: "call_big".to_string().into(),
                        name: "bash".to_string(),
                    }))
                    .await;
                // echoes ~20,000 chars so the tool result exceeds the 16 KiB default cap
                let _ = tx
                    .send(Ok(StreamEvent::ToolInputDelta(
                        r#"{"command":"printf 'x%.0s' {1..20000}"}"#.to_string(),
                    )))
                    .await;
                let _ = tx.send(Ok(StreamEvent::ToolUseEnd)).await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("tool_calls".to_string()),
                    }))
                    .await;
            } else {
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta("done".to_string())))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("end_turn".to_string()),
                    }))
                    .await;
            }
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "oversized-stream"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// Runtime evidence the scheduled per-step prune (a) does NOT strip a freshly added
/// oversized tool result from this batch (it is in the unconsumed suffix after the
/// latest assistant), and (b) DOES strip an oversized image already consumed before
/// the current turn.
#[tokio::test]
async fn scheduled_prune_preserves_fresh_oversized_results_at_runtime() {
    let _guard = crate::storage::lock_test_env();
    let sp = OversizedToolStreamProvider::default();
    let provider: Arc<dyn Provider> = Arc::new(sp);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // A consumed-prefix oversized image (append, then an assistant response marks
    // it consumed when a later user turn exists). Simulate via a user image block
    // then an assistant ack.
    agent
        .session
        .append_stored_message(crate::session::StoredMessage {
            id: "consumed-img".into(),
            role: crate::message::Role::User,
            content: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "a".repeat(5000),
            }],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        });
    agent.session.add_message(
        crate::message::Role::Assistant,
        vec![ContentBlock::Text {
            text: "ack".into(),
            cache_control: None,
        }],
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("big output", Vec::new(), None, tx)
        .await
        .expect("turn should complete");

    let mut text = String::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::TextDelta { text: d } = event {
            text.push_str(&d);
        }
    }
    assert!(text.contains("done"), "turn must finish, got {text:?}");

    // The fresh oversized bash tool-result from THIS batch must survive pruning.
    let tool_result_count = agent
        .session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| matches!(b, ContentBlock::ToolResult { content, .. } if content.len() > 16384))
        .count();
    assert_eq!(
        tool_result_count, 1,
        "fresh oversized tool result must not be pruned before the model reads it"
    );

    // The consumed oversized image from BEFORE this turn must have been replaced.
    let consumed_image = agent.session.messages.iter().flat_map(|m| &m.content)
        .find(|b| matches!(b, ContentBlock::Text { text, .. } if text.contains("Image omitted during context pruning")));
    assert!(
        consumed_image.is_some(),
        "consumed oversized image must be pruned by scheduled prune"
    );
}
/// A pure-text continuation turn (no tool results) must STILL prune a previously
/// consumed oversized image; gating the image pass on tool_results_dirty would
/// leak it for the whole session.
#[tokio::test]
async fn text_only_turn_prunes_consumed_oversized_image() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(TextOnlyStreamProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // A consumed oversized image: the image (User), then an assistant ack.
    agent.session.append_stored_message(crate::session::StoredMessage {
        id: "txt-img".into(),
        role: crate::message::Role::User,
        content: vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: "a".repeat(5000),
        }],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });
    agent.session.add_message(
        crate::message::Role::Assistant,
        vec![ContentBlock::Text {
            text: "ack".into(),
            cache_control: None,
        }],
    );

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("continue", Vec::new(), None, tx)
        .await
        .unwrap();

    // The consumed oversized image must have been pruned even though this turn
    // added no tool results (regression for the text-only image leak).
    let marker = agent.session.messages.iter().flat_map(|m| &m.content).find(|b| {
        matches!(b, ContentBlock::Text { text, .. } if text.contains("Image omitted during context pruning"))
    });
    assert!(
        marker.is_some(),
        "consumed oversized image must be pruned on a text-only turn"
    );
}
/// A pure-text continuation turn over a PREVIOUS turn's consumed oversized tool
/// result must STILL truncate it (the tool cap should not be limited to turns
/// that themselves ran tools). This proves the consumed-prefix prune runs every
/// step for already-consumed oversized nodes, not only on tool-result steps.
#[tokio::test]
async fn text_only_turn_truncates_consumed_oversized_tool_result() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(TextOnlyStreamProvider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // A consumed oversized tool result: a prior turn committed a 6000-byte
    // result, then an assistant ack made it consumed (prefix before last assistant).
    agent.session.append_stored_message(crate::session::StoredMessage {
        id: "txt-tool".into(),
        role: crate::message::Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "x".repeat(20_000),
            is_error: None,
        }],
        display_role: None, timestamp: None, tool_duration_ms: None, token_usage: None,
    });
    agent.session.add_message(
        crate::message::Role::Assistant,
        vec![ContentBlock::Text { text: "ack".into(), cache_control: None }],
    );

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    agent.run_once_streaming_mpsc("continue", Vec::new(), None, tx).await.unwrap();

    // After a text-only turn, the consumed oversized tool result must be truncated
    // to <= the default 16 KiB cap. If it is still 20000, the tool-result pass was
    // wrongly gated off (it should run on the loop head, every step).
    let big = agent.session.messages.iter().flat_map(|m| &m.content).filter(|b| {
        matches!(b, ContentBlock::ToolResult { content, .. } if content.len() > 16384)
    }).count();
    assert_eq!(big, 0, "consumed oversized tool result must be truncated on a text-only turn");
}

/// Streaming provider that enables jcode summary compaction (`uses_jcode_compaction`)
/// with a small context window so the compaction manager's token estimate drives
/// real decisions. It ends the turn with a plain text reply (no tool calls), which
/// keeps the assertion focused on the *accounting* effect of the scheduled prune.
#[derive(Clone, Default)]
struct PruneAccountingStreamProvider {
    context: usize,
}

#[async_trait]
impl Provider for PruneAccountingStreamProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(4);
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(StreamEvent::TextDelta("done".to_string())))
                .await;
            let _ = tx
                .send(Ok(StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".to_string()),
                }))
                .await;
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "prune-accounting"
    }

    fn supports_compaction(&self) -> bool {
        true
    }

    fn uses_jcode_compaction(&self) -> bool {
        true
    }

    fn context_window(&self) -> usize {
        self.context
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// Drives a LARGE transcript through the REAL streaming loop with BOTH a consumed
/// oversized tool result AND a consumed oversized image in the preview prefix, then
/// asserts the scheduled per-step prune (a) actually shrinks both nodes and (b) — the
/// token-accounting focus — leaves the compaction manager's estimate reseeded from the
/// already-pruned transcript rather than trusting a stale (over-counted) pre-prune
/// figure. This is what makes a subsequent auto-compaction decide using the pruned
/// sizes: its context-usage gate reads exactly this estimate.
#[tokio::test]
async fn scheduled_prune_keeps_compaction_token_accounting_consistent_with_both_oversized_nodes() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(PruneAccountingStreamProvider { context: 20_000 });
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // Seed a CONSUMED prefix (everything before the last assistant gets pruned)
    // containing BOTH an oversized tool result and an oversized inline image.
    // Adding via `agent.add_message` keeps the manager's message bookkeeping in
    // lockstep with the session, so `total_turns` matches the message count and
    // the only thing that can mark `active_chars` stale is the prune itself.
    agent.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "big_tool".into(),
            content: "y".repeat(50_000),
            is_error: None,
        }],
    );
    agent.add_message(
        Role::User,
        vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: "a".repeat(10_000),
        }],
    );
    // An assistant ack makes the tool result + image CONSUMED (they sit before
    // the last assistant message) so the scheduled prune is allowed to shrink them.
    agent.add_message(
        Role::Assistant,
        vec![ContentBlock::Text {
            text: "ack".into(),
            cache_control: None,
        }],
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("continue", Vec::new(), None, tx)
        .await
        .expect("streaming turn should complete");

    let mut text = String::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::TextDelta { text: d } = event {
            text.push_str(&d);
        }
    }
    assert!(text.contains("done"), "turn must finish, got {text:?}");

    // (1) Both consumed oversized nodes must have been pruned in place.
    let oversized_tool_results = agent
        .session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| matches!(b, ContentBlock::ToolResult { content, .. } if content.len() > 16384))
        .count();
    assert_eq!(
        oversized_tool_results, 0,
        "consumed oversized tool result must be truncated to <= the 16 KiB cap"
    );
    let image_marker = agent
        .session
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .any(|b| matches!(b, ContentBlock::Text { text, .. } if text.contains("Image omitted during context pruning")));
    assert!(
        image_marker,
        "consumed oversized image must be replaced with the prune marker"
    );

    // (2) The compaction manager's token accounting must now reflect the pruned
    // (small) content, NOT a stale pre-prune over-count. The scheduled prune shrinks
    // content in place without changing the message count, so the count-guard in
    // `active_message_chars_with` would NOT recompute on its own — the manager must
    // have been told (via `note_prune_applied`) to invalidate its rolling estimate.
    let budget = agent.registry.compaction().read().await.token_budget();
    let provider_messages = agent.provider_messages();
    let pruned_chars: usize = provider_messages
        .iter()
        .map(crate::compaction::message_char_count)
        .sum();
    let expected = crate::compaction::estimate_compaction_tokens(None, pruned_chars, budget);
    let estimate = agent
        .registry
        .compaction()
        .read()
        .await
        .token_estimate_with(&provider_messages);

    assert!(
        pruned_chars < 20_000,
        "expected the pruned transcript to be small, got {pruned_chars} chars"
    );
    assert_eq!(
        estimate, expected,
        "manager token estimate must be reseeded from the pruned transcript (expected {expected} from {pruned_chars} chars), got {estimate}"
    );
    assert!(
        estimate < 5_000,
        "auto-compaction must see the small pruned size, got {estimate} tokens"
    );
}

/// The manual `/prune` command (server route) must reseed the compaction
/// manager's rolling token estimate from the already-pruned transcript, exactly
/// like the scheduled per-step prune. Regression for a stale-over-count: before
/// this fix `request_manual_prune` called only `note_compaction_applied()`, which
/// resets provider/cache/tool state but not the manager's `active_chars`, so a
/// subsequent auto-compaction gate could read a pre-prune over-count. The TUI
/// `/prune` handler already reseeds via `reseed_compaction_from_provider_messages`;
/// this assertion keeps the server route consistent with it.
#[tokio::test]
async fn manual_prune_reseeds_compaction_token_accounting_from_pruned_transcript() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(PruneAccountingStreamProvider { context: 20_000 });
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    agent.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "manual_big".into(),
            content: "z".repeat(50_000),
            is_error: None,
        }],
    );

    // Prime the manager over-count: it consumed the oversized result and now
    // trusts a large rolling char estimate.
    agent.provider_messages();
    let budget = agent.registry.compaction().read().await.token_budget();
    let pre_estimate = agent
        .registry
        .compaction()
        .read()
        .await
        .token_estimate_with(&agent.provider_messages());
    assert!(
        pre_estimate > 10_000,
        "without the prune the manager should be over-counting, got {pre_estimate}"
    );

    let (report, _message) = agent.request_manual_prune().expect("manual prune persists");
    assert_eq!(report.tool_results_truncated, 1);

    // After pruning, the manager's estimate must reflect the small (pruned)
    // content, NOT the stale pre-prune over-count.
    let provider_messages = agent.provider_messages();
    let pruned_chars: usize = provider_messages
        .iter()
        .map(crate::compaction::message_char_count)
        .sum();
    let expected = crate::compaction::estimate_compaction_tokens(None, pruned_chars, budget);
    let estimate = agent
        .registry
        .compaction()
        .read()
        .await
        .token_estimate_with(&provider_messages);
    assert_eq!(
        estimate, expected,
        "manual prune must reseed the estimate from pruned content (expected {expected}), got {estimate}"
    );
    assert!(
        estimate < 2_000,
        "auto-compaction must see the small pruned size after manual prune, got {estimate}"
    );
}

/// The manual `/prune` full-reseed must also handle a pre-existing compaction
/// summary correctly (the `restore_persisted_state_with` branch of
/// `reseed_compaction_from_pruned_transcript`). It should reseed to
/// `summary_chars + active (pruned) chars` — NOT double-count the summary and
/// NOT keep a stale pre-prune over-count on the active suffix.
#[tokio::test]
async fn manual_prune_reseeds_with_existing_compaction_summary() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(PruneAccountingStreamProvider { context: 20_000 });
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // A short consumed prefix that will be summarized.
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "old 1".into(),
            cache_control: None,
        }],
    );
    agent.add_message(
        Role::User,
        vec![ContentBlock::Text {
            text: "old 2".into(),
            cache_control: None,
        }],
    );
    // An oversized tool result in the ACTIVE (uncompacted) suffix.
    agent.add_message(
        Role::User,
        vec![ContentBlock::ToolResult {
            tool_use_id: "summary_big".into(),
            content: "z".repeat(50_000),
            is_error: None,
        }],
    );

    // Apply a native compaction over the first 2 messages so `session.compaction`
    // is populated; the oversized tool result remains active (uncompacted).
    agent
        .apply_openai_native_compaction("enc_summary".to_string(), 2)
        .unwrap();

    let (_report, _msg) = agent.request_manual_prune().expect("manual prune persists");

    // The summary must survive the manual-prune full-reseed.
    let comp = agent.registry.compaction();
    let manager = comp.read().await;
    let summary_chars = manager.summary_chars();
    assert!(
        summary_chars > 0,
        "existing compression summary must be preserved after manual prune (summary_chars={summary_chars})"
    );

    // The estimate must be summary + active (pruned) chars. Use the manager's own
    // token_estimate_with (which folds the summary in), and separately confirm the
    // active suffix is small after the prune by comparing against an estimate built
    // from the live provider messages' pruned char count.
    let provider_messages = agent.provider_messages();
    let estimate = manager.token_estimate_with(&provider_messages);

    // Build the expected figure the same way the manager would if it recomputed:
    // (budget-adjusted) summary + active chars, where active = messages beyond the
    // compacted prefix (index 2), all already pruned. Budget (20k) < DEFAULT_TOKEN_BUDGET/2
    // so SYSTEM_OVERHEAD_TOKENS is 0 and the estimate is just chars / CHARS_PER_TOKEN.
    let active_chars: usize = provider_messages
        .iter()
        .skip(2)
        .map(crate::compaction::message_char_count)
        .sum();
    let expected_active_tokens =
        (summary_chars + active_chars) / crate::compaction::CHARS_PER_TOKEN;
    // token_estimate_with returns estimate_compaction_tokens(summary, active_chars),
    // which equals budget-adjusted(summary_chars + active_chars) — the same as above.
    assert_eq!(
        estimate, expected_active_tokens,
        "manual prune with existing summary must yield summary+active-pruned (got {estimate}, expected {expected_active_tokens})"
    );
    // Active suffix must be small (pruned): the 50k tool result was truncated to <=16k.
    assert!(
        active_chars <= 16_384,
        "active tool result must be pruned to <= the 16 KiB cap, got {active_chars} chars"
    );
    // Sanity: overall estimate is small (summary is tiny + active is pruned), far
    // below a stale over-count of ~50k chars (~12.5k tokens).
    assert!(
        estimate < 3_000,
        "manual-prune-with-summary estimate must be small, got {estimate}"
    );
}
