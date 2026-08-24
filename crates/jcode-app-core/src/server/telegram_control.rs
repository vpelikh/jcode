//! Shared handle for Telegram (and other remote-channel) session control.
//!
//! The Telegram reply loop runs independently of ambient mode and has no direct
//! reference to the server's live-session registry. To support `/resume` (send a
//! prompt to a live session and get the reply), the server registers its
//! live-session map here at startup (mirroring the `tool::ambient` static-handle
//! pattern), and the channel calls [`resume_session_for_control`].

use crate::agent::Agent;
use crate::protocol::ServerEvent;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tokio::sync::RwLock;

/// Live session registry: session_id -> live Agent. Mirrors
/// `server::client_lifecycle`'s `SessionAgents`.
pub type SessionAgents = Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>;

static LIVE_SESSIONS: OnceLock<SessionAgents> = OnceLock::new();

/// Register the server's live-session registry. Safe to call once at startup;
/// a second call is ignored.
pub fn register_live_sessions(sessions: SessionAgents) {
    let _ = LIVE_SESSIONS.set(sessions);
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

    let start_message_index = {
        let guard = agent.lock().await;
        guard.message_count()
    };

    // No live client to receive the event stream; discard it. The channel is
    // unbounded so it cannot block the turn, just accumulates garbage.
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<ServerEvent>();

    run_turn_and_read_reply(agent, text, start_message_index, event_tx).await
}

/// Actually run the turn on a locked agent and read back the assistant reply.
async fn run_turn_and_read_reply(
    agent: Arc<Mutex<Agent>>,
    text: &str,
    start_message_index: usize,
    event_tx: tokio::sync::mpsc::UnboundedSender<ServerEvent>,
) -> anyhow::Result<String> {
    let mut guard = agent.lock().await;
    guard
        .run_once_streaming_mpsc(text, Vec::new(), None, event_tx)
        .await?;
    let reply = guard.latest_assistant_text_after(start_message_index);
    drop(guard);
    Ok(reply.unwrap_or_else(|| "Message processed; no assistant text was produced.".to_string()))
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
    if let Ok(raw) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(&path, raw);
    }
}

/// The session id currently selected for `chat_id`, if any.
pub fn active_session_for(chat_id: &str) -> Option<String> {
    load_state().get(chat_id).cloned()
}

/// Select the active session for `chat_id`.
pub fn set_active_session(chat_id: &str, session_id: &str) {
    let mut state = load_state();
    state.insert(chat_id.to_string(), session_id.to_string());
    save_state(&state);
}

/// Clear the selected session for `chat_id`.
pub fn clear_active_session(chat_id: &str) {
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