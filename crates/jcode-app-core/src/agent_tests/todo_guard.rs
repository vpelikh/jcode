use super::*;

/// A provider that works for many rounds WITHOUT ever touching the todo tool,
/// then completes. Each bash call uses a distinct command so the repeat-tool
/// guard (which only trips on identical repeats) stays silent, isolating the
/// stale-todo guard.
#[derive(Clone, Default)]
struct BusyWithoutTodoProvider {
    calls: Arc<std::sync::Mutex<usize>>,
}

#[async_trait]
impl Provider for BusyWithoutTodoProvider {
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
        let threshold = crate::config::config().loop_guard.todo_stale_threshold;
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            if call <= threshold {
                // A distinct bash call each round, so this is not a repeat.
                let _ = tx
                    .send(Ok(StreamEvent::ToolUseStart {
                        id: format!("busy_{call}").into(),
                        name: "bash".to_string(),
                    }))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::ToolInputDelta(format!(
                        r#"{{"command":"true # step {call}"}}"#
                    ))))
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
        "busy-without-todo"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// The stale-todo guard must fire through the REAL streaming loop: a model that
/// makes many tool calls without touching the todo list, while incomplete work
/// remains, gets a hidden stale-todo reminder injected. This is the wiring-level
/// fix for the observed "every todo updated once at the very end" behavior.
#[tokio::test]
async fn streaming_turn_injects_stale_todo_reminder_when_model_ignores_todos() {
    /// Restores `JCODE_HOME` on drop, including on assertion panic.
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => crate::env::set_var("JCODE_HOME", value),
                None => crate::env::remove_var("JCODE_HOME"),
            }
        }
    }

    let _guard = crate::storage::lock_test_env();
    let home = tempfile::TempDir::new().expect("tempdir");
    let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
    crate::env::set_var("JCODE_HOME", home.path());

    let busy = BusyWithoutTodoProvider::default();
    let provider: Arc<dyn Provider> = Arc::new(busy);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // Seed incomplete work for this session so the guard has something to keep
    // fresh. The session id is only known after construction.
    crate::todo::save_todos(
        &agent.session.id,
        &[crate::todo::TodoItem {
            id: "w".into(),
            content: "multi-step work".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            group: None,
            confidence: None,
            ..Default::default()
        }],
    )
    .expect("seed todos");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("do the long task", Vec::new(), None, tx)
        .await
        .expect("turn should complete");

    let mut text = String::new();
    let mut injected_system = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            ServerEvent::TextDelta { text: delta } => text.push_str(&delta),
            ServerEvent::SoftInterruptInjected { content, .. } => {
                injected_system.push(content);
            }
            _ => {}
        }
    }
    assert!(text.contains("done"), "turn must complete, got {text:?}");

    // The live client event must be a clean, tag-free notice. Forwarding the
    // stored reminder text would print the raw <system-reminder> wrapper (and
    // the model instructions) in the UI, diverging from reload, which hides it.
    assert!(
        injected_system
            .iter()
            .any(|content| content.to_ascii_lowercase().contains("todo")),
        "a todo reminder notice must reach the client, got {injected_system:?}"
    );
    for content in &injected_system {
        assert!(
            !content.contains("<system-reminder>") && !content.contains("</system-reminder>"),
            "the hidden system-reminder wrapper must not leak into the client event: {content:?}"
        );
    }

    // A hidden stale-todo reminder must have been injected, and it must stay
    // hidden behind the system-reminder marker.
    let reminders = agent
        .session
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content.iter().any(|block| match block {
                    ContentBlock::Text { text, .. } => {
                        text.trim_start().starts_with("<system-reminder>")
                            && text.contains("todo")
                            && text.contains("without updating")
                    }
                    _ => false,
                })
        })
        .count();
    assert!(
        reminders >= 1,
        "at least one stale-todo reminder must be injected; transcript has {} messages",
        agent.session.messages.len()
    );
}
