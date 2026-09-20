//! Shared handle for Telegram (and other remote-channel) session control.
//!
//! The Telegram reply loop runs independently of ambient mode and has no direct
//! reference to the server's live-session registry. To support `/resume` (send a
//! prompt to a live session and get the reply), the server registers its
//! live-session map here at startup (mirroring the `tool::ambient` static-handle
//! pattern), and the channel calls [`resume_session_for_control`].

use crate::agent::Agent;
use crate::protocol::ServerEvent;
use crate::provider::Provider;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tokio::sync::RwLock;

/// Live session registry: session_id -> live Agent. Mirrors
/// `server::client_lifecycle`'s `SessionAgents`.
pub type SessionAgents = Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>;

static LIVE_SESSIONS: OnceLock<SessionAgents> = OnceLock::new();
static PROVIDER: OnceLock<Arc<dyn Provider>> = OnceLock::new();

/// Register the server's live-session registry. Safe to call once at startup;
/// a second call is ignored.
pub fn register_live_sessions(sessions: SessionAgents) {
    let _ = LIVE_SESSIONS.set(sessions);
}

/// Register the server's provider so closed sessions can be resumed headlessly
/// (a provider is required to build a new `Agent`).
pub fn register_provider(provider: Arc<dyn Provider>) {
    let _ = PROVIDER.set(provider);
}

/// Send `text` to the live session `session_id` and return the assistant's
/// latest reply text. Errors if the session is not currently live.
pub async fn resume_session_for_control(session_id: &str, text: &str) -> anyhow::Result<String> {
    let Some(sessions) = LIVE_SESSIONS.get() else {
        anyhow::bail!("Telegram control is not wired to a server runtime");
    };

    let agent = {
        let guard = sessions.read().await;
        guard.get(session_id).cloned()
    };
    let Some(agent) = agent else {
        anyhow::bail!("session '{session_id}' is not live in this Jcode server");
    };

    run_turn_and_read_reply(agent, text).await
}

/// Send `text` to the session `session_id`, auto-resuming it headlessly if it
/// is not currently live. Returns the assistant's reply, or an error if the
/// session cannot be loaded or the server lacks a registered provider.
pub async fn resume_session_for_control_or_spawn(
    session_id: &str,
    text: &str,
) -> anyhow::Result<String> {
    let sessions = match LIVE_SESSIONS.get() {
        Some(s) => s,
        None => anyhow::bail!("Telegram control is not wired to a server runtime"),
    };

    // If it's already live, just run the turn.
    let live = {
        let guard = sessions.read().await;
        guard.get(session_id).cloned()
    };
    if let Some(agent) = live {
        return run_turn_and_read_reply(agent, text).await;
    }

    // Not live — auto-resume the persisted session headlessly.
    let provider = PROVIDER
        .get()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no provider registered for session auto-resume"))?;
    let session = crate::session::Session::load(session_id)?;
    let provider = provider.fork();
    let registry = crate::tool::Registry::new(provider.clone()).await;
    let agent = Agent::new_with_session(Arc::clone(&provider), registry, session, None);
    let agent = Arc::new(Mutex::new(agent));
    {
        let mut guard = sessions.write().await;
        guard.insert(session_id.to_string(), Arc::clone(&agent));
    }
    crate::logging::info(&format!(
        "telegram auto-resumed session {session_id} headlessly"
    ));
    run_turn_and_read_reply(agent, text).await
}

/// Like [`resume_session_for_control_or_spawn`] but streams partial assistant
/// text to `on_progress` as the turn generates, so the Telegram user sees live
/// progress instead of waiting for the full reply. The final reply is returned.
pub async fn resume_session_for_control_or_spawn_streaming<F, Fut>(
    session_id: &str,
    text: &str,
    on_progress: F,
) -> anyhow::Result<String>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let sessions = match LIVE_SESSIONS.get() {
        Some(s) => s,
        None => anyhow::bail!("Telegram control is not wired to a server runtime"),
    };
    let live = {
        let guard = sessions.read().await;
        guard.get(session_id).cloned()
    };
    if let Some(agent) = live {
        return run_turn_streaming(agent, text, on_progress).await;
    }
    let provider = PROVIDER
        .get()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no provider registered for session auto-resume"))?;
    let session = crate::session::Session::load(session_id)?;
    let provider = provider.fork();
    let registry = crate::tool::Registry::new(provider.clone()).await;
    let agent = Agent::new_with_session(Arc::clone(&provider), registry, session, None);
    let agent = Arc::new(Mutex::new(agent));
    {
        let mut guard = sessions.write().await;
        guard.insert(session_id.to_string(), Arc::clone(&agent));
    }
    crate::logging::info(&format!(
        "telegram auto-resumed session {session_id} headlessly (streaming)"
    ));
    run_turn_streaming(agent, text, on_progress).await
}

/// Request a graceful shutdown of the live session `session_id`'s in-flight
/// turn (if any). The agent's run loop checks the graceful_shutdown flag at
/// safe points and stops generating, returning whatever partial response was
/// produced so far. Returns `false` if the session is not live.
pub async fn request_graceful_shutdown_for_control(session_id: &str) -> bool {
    let Some(sessions) = LIVE_SESSIONS.get() else {
        return false;
    };
    let agent = {
        let guard = sessions.read().await;
        guard.get(session_id).cloned()
    };
    let Some(agent) = agent else {
        return false;
    };
    let guard = agent.lock().await;
    guard.request_graceful_shutdown();
    true
}

/// Live agent count (size of the in-memory registry) for status reporting.
pub async fn live_session_count() -> Option<usize> {
    let sessions = LIVE_SESSIONS.get()?;
    let guard = sessions.read().await;
    Some(guard.len())
}

/// Return the currently-registered live sessions for this runtime. Used by the
/// Telegram `/sessions` listing. Includes only sessions the Telegram control
/// path knows about.
pub async fn live_sessions_snapshot() -> Option<Vec<String>> {
    let sessions = LIVE_SESSIONS.get()?;
    let guard = sessions.read().await;
    Some(guard.keys().cloned().collect())
}

/// Remove a session from the live registry without persisting it. Used by
/// `/free` to drop headless sessions the user is no longer talking to so they
/// don't leak memory. Returns `true` if a session was removed.
pub async fn free_session_for_control(session_id: &str) -> bool {
    let Some(sessions) = LIVE_SESSIONS.get() else {
        return false;
    };
    let mut guard = sessions.write().await;
    guard.remove(session_id).is_some()
}

/// Create a brand-new session, run the first turn with `text` (if non-empty),
/// register it as the live session, and return its id plus the assistant reply.
///
/// Used by the Telegram `/new` command so a user can start a fresh session from
/// the chat rather than only talking to existing ones.
pub async fn create_session_for_control(
    text: Option<&str>,
) -> anyhow::Result<(String, String)> {
    let sessions = LIVE_SESSIONS
        .get()
        .ok_or_else(|| anyhow::anyhow!("Telegram control is not wired to a server runtime"))?;
    let provider = PROVIDER
        .get()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no provider registered for session creation"))?;
    let provider = provider.fork();
    let registry = crate::tool::Registry::new(provider.clone()).await;
    let agent = Agent::new_with_initial_working_dir(Arc::clone(&provider), registry, None);
    let session_id = agent.session_id().to_string();
    let agent = Arc::new(Mutex::new(agent));
    {
        let mut guard = sessions.write().await;
        guard.insert(session_id.clone(), Arc::clone(&agent));
    }
    crate::logging::info(&format!(
        "telegram created new session {session_id}"
    ));
    let reply = match text {
        Some(text) if !text.trim().is_empty() => {
            run_turn_and_read_reply(agent, text.trim()).await?
        }
        _ => "New session created.".to_string(),
    };
    Ok((session_id, reply))
}

/// Actually run the turn on a locked agent and read back the assistant reply.
async fn run_turn_and_read_reply(
    agent: Arc<Mutex<Agent>>,
    text: &str,
) -> anyhow::Result<String> {
    let start_message_index = {
        let guard = agent.lock().await;
        guard.message_count()
    };
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<ServerEvent>();
    let mut guard = agent.lock().await;
    guard
        .run_once_streaming_mpsc(text, Vec::new(), None, event_tx)
        .await?;
    let reply = guard.latest_assistant_text_after(start_message_index);
    drop(guard);
    Ok(reply.unwrap_or_else(|| "Message processed; no assistant text was produced.".to_string()))
}

/// Run a turn while streaming partial assistant text to a caller-supplied
/// progress sink. `on_progress` is invoked (throttled to avoid rate limits)
/// with the accumulated text so far each time new `text_delta` arrives. Unlike
/// [`run_turn_and_read_reply`], the caller gets live updates instead of only
/// the final message. Returns the final assistant reply.
pub async fn run_turn_streaming<F, Fut>(
    agent: Arc<Mutex<Agent>>,
    text: &str,
    mut on_progress: F,
) -> anyhow::Result<String>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use std::time::Instant;
    let start_message_index = {
        let guard = agent.lock().await;
        guard.message_count()
    };
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<ServerEvent>();
    // Spawn the turn in its own task so the agent's mutable borrow is confined
    // there. We consume the event stream here and, once the task finishes, read
    // the final assistant text back from the (now-unborrowed) agent. This avoids
    // holding a mutable borrow of `agent` across the streaming select loop.
    let text_owned = text.to_string();
    let agent_for_task = Arc::clone(&agent);
    let mut handle = tokio::spawn(async move {
        let mut guard = agent_for_task.lock().await;
        guard
            .run_once_streaming_mpsc(&text_owned, Vec::new(), None, event_tx)
            .await
    });
    let mut last_emit = Instant::now();
    let mut acc = String::new();
    loop {
        tokio::select! {
            biased;
            res = &mut handle => {
                let _ = res;
                break;
            }
            evt = event_rx.recv() => {
                match evt {
                    Some(ServerEvent::TextDelta { text }) => {
                        acc.push_str(&text);
                        if last_emit.elapsed().as_millis() >= 1500 {
                            on_progress(acc.clone()).await;
                            last_emit = Instant::now();
                        }
                    }
                    Some(ServerEvent::TextReplace { text }) => {
                        acc = text;
                        if last_emit.elapsed().as_millis() >= 1500 {
                            on_progress(acc.clone()).await;
                            last_emit = Instant::now();
                        }
                    }
                    Some(ServerEvent::Done { .. }) | Some(ServerEvent::Error { .. }) => {
                        break;
                    }
                    None => {
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    // Drain any events the select didn't pick up before the task completed so
    // the live preview reflects the complete text.
    while let Ok(evt) = event_rx.try_recv() {
        match evt {
            ServerEvent::TextDelta { text } => acc.push_str(&text),
            ServerEvent::TextReplace { text } => acc = text,
            _ => {}
        }
    }
    // Flush a final progress update so the user always sees the complete text.
    on_progress(acc.clone()).await;
    // The turn task has finished; read the final assistant text from the agent.
    let guard = agent.lock().await;
    let reply = guard.latest_assistant_text_after(start_message_index);
    let out = reply.unwrap_or(acc);
    Ok(if out.trim().is_empty() {
        "Message processed; no assistant text was produced.".to_string()
    } else {
        out
    })
}

/// Return the ids of the currently-live sessions, or `None` if the server has
/// not registered its registry.
pub async fn live_session_ids() -> Option<Vec<String>> {
    let sessions = LIVE_SESSIONS.get()?;
    let guard = sessions.read().await;
    Some(guard.keys().cloned().collect())
}

// ---------------------------------------------------------------------------
// Per-chat active session state
// ---------------------------------------------------------------------------

fn state_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("telegram-control-state.json"))
}

/// Guards all read-modify-write cycles of the control-state file so concurrent
/// chats never clobber each other's selection.
fn state_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

fn load_state() -> std::collections::HashMap<String, String> {
    let Ok(path) = state_path() else {
        return std::collections::HashMap::new();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return std::collections::HashMap::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_state(state: &std::collections::HashMap<String, String>) {
    let Ok(path) = state_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(parent);
    let Ok(raw) = serde_json::to_string_pretty(state) else {
        return;
    };
    // Atomic write: write to a temp file in the same directory then rename, so
    // a crash mid-write can never leave a truncated/corrupt state file.
    let tmp = parent.join("telegram-control-state.json.tmp");
    if std::fs::write(&tmp, raw).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// The session id currently selected for `chat_id`, if any.
pub fn active_session_for(chat_id: &str) -> Option<String> {
    let _guard = state_lock().lock().unwrap_or_else(|e| e.into_inner());
    load_state().get(chat_id).cloned()
}

/// Select the active session for `chat_id`.
pub fn set_active_session(chat_id: &str, session_id: &str) {
    let _guard = state_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load_state();
    state.insert(chat_id.to_string(), session_id.to_string());
    save_state(&state);
}

/// Clear the selected session for `chat_id`.
pub fn clear_active_session(chat_id: &str) {
    let _guard = state_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load_state();
    state.remove(chat_id);
    save_state(&state);
}

// ---------------------------------------------------------------------------
// History rendering
// ---------------------------------------------------------------------------

/// Render a compact transcript of a session's recent user/assistant messages.
/// `session_id` may be a live session id or any persisted session. Returns a
/// Markdown-friendly text block.
pub fn render_session_history(
    session_id: &str,
    limit: usize,
) -> anyhow::Result<String> {
    use crate::session::Session;
    use jcode_message_types::Role;
    let session = Session::load(session_id)?;
    let messages: Vec<jcode_session_types::StoredMessage> = session
        .visible_conversation_messages()
        .into_iter()
        .cloned()
        .collect();

    let take = messages.len().saturating_sub(limit);
    let slice = &messages[take..];
    let mut lines = Vec::new();
    for m in slice {
        let text = m.content_preview();
        match m.role {
            Role::User => lines.push(format!("🧑 *you:* {text}")),
            Role::Assistant => lines.push(format!("🤖 *jcode:* {text}")),
        }
    }
    if lines.is_empty() {
        Ok("(no visible messages)".to_string())
    } else {
        Ok(lines.join("\n\n"))
    }
}