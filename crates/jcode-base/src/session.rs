use crate::message::{ContentBlock, Message};
use crate::id::{extract_session_name, new_id, new_memorable_session_id_avoiding};
pub use crate::storage::{
    SessionCounts, SessionPresence, active_session_ids, find_active_session_id_by_pid,
    mark_streaming, session_counts, session_presence, unmark_streaming, user_session_counts,
    user_session_presence,
};
use crate::storage::{active_pids_dir, register_active_pid, unregister_active_pid};

/// RAII guard that marks a session as actively streaming for its lifetime.
///
/// Wraps the on-disk streaming marker from `jcode-storage` (cleared on every
/// exit path so presence UIs never show a phantom streaming session) and
/// additionally holds a macOS power assertion so the system does not
/// idle-sleep in the middle of a streaming model response.
pub struct StreamingGuard {
    _marker: crate::storage::StreamingGuard,
    #[allow(dead_code)]
    sleep_assertion: crate::platform::PowerAssertion,
}

impl StreamingGuard {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self::with_reason(session_id, "Jcode streaming model response")
    }

    pub(crate) fn with_reason(session_id: impl Into<String>, reason: &str) -> Self {
        Self {
            _marker: crate::storage::StreamingGuard::new(session_id),
            sleep_assertion: crate::platform::PowerAssertion::prevent_user_idle_system_sleep(reason),
        }
    }
}
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
pub mod event_types;
pub use invariants::{
    CompactionBracket, InvariantLog, InvariantRegistry, InvariantViolation, LogInvariant,
    LogProjection, MessageCountProjection, fold_projection, project_map,
};
mod crash;
mod invariants;
mod journal;
mod load_telemetry;
mod maintenance;
mod memory_profile;
pub mod model;
mod persistence;
mod render;
mod storage_paths;
pub use crash::{
    CrashedSessionsInfo, detect_crashed_sessions, find_recent_crashed_sessions,
    find_session_by_name_or_id, recover_crashed_sessions, recover_crashed_sessions_by_ids,
};
pub use jcode_session_types::{
    EnvSnapshot, GitState, ReviewLoopState, SessionImproveMode, SessionStatus,
    StoredCompactionState, StoredDisplayRole, StoredMemoryInjection, StoredMessage,
    StoredTokenUsage,
};
// The public event-log seam (`append_session_event`, `event_log`,
// `fork_event_log`) exposes `SessionEvent`/`SessionEventMap`/`SessionEventError`
// types directly, so re-export them at the session-module level uniformly with
// `SessionEventOp` instead of forcing external callers to reach into
// `event_types::`.
pub use event_types::{SessionEvent, SessionEventError, SessionEventMap, SessionEventOp};
use jcode_message_types::Role;
use journal::{PersistVectorMode, SessionJournalMeta, SessionPersistState};
pub use maintenance::prune_old_session_backups;
pub use memory_profile::SessionMemoryProfileSnapshot;
use memory_profile::{
    ContentBlockMemoryStats, SessionMemoryProfileCache, summarize_blocks, summarize_message_content,
};
use model::SESSION_CONTEXT_PREFIX;
pub use model::StoredReplayEvent;
pub use model::StoredReplayEventKind;
pub use render::{
    RenderedCompactedHistoryInfo, RenderedImage, RenderedImageAnchor, RenderedImageSource,
    RenderedMessage, has_rendered_images, is_attached_image_label_text, render_images,
    render_messages, render_messages_and_images, render_messages_and_images_with_compacted_history,
    summarize_tool_calls,
};
pub use storage_paths::session_journal_path_from_snapshot;
#[cfg(test)]
pub(crate) use storage_paths::session_path_in_dir;
use storage_paths::{estimate_json_bytes, persist_vector_mode_label};
pub use storage_paths::{session_exists, session_journal_path, session_path};

fn stored_messages_to_messages(messages: &[StoredMessage]) -> Vec<Message> {
    messages.iter().map(StoredMessage::to_message).collect()
}

fn is_internal_system_reminder_message(message: &StoredMessage) -> bool {
    message
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.trim_start()),
            _ => None,
        })
        .is_some_and(|text| text.starts_with("<system-reminder>"))
}

fn is_visible_conversation_message(message: &StoredMessage) -> bool {
    message.display_role.is_none()
        && !is_internal_system_reminder_message(message)
        && !is_scheduled_task_message(message)
}

/// Recognize scheduler prompts persisted before they received an explicit
/// system display role. This keeps old sessions from treating them as user
/// prompts after resume.
pub fn is_scheduled_task_message(message: &StoredMessage) -> bool {
    message.role == Role::User
        && message.content.iter().any(|block| {
            matches!(block, ContentBlock::Text { text, .. } if text.trim_start().starts_with("[Scheduled task]\n"))
        })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub parent_id: Option<String>,
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub messages: Vec<StoredMessage>,
    /// Persisted compacted-view state so reload/resume can continue using the
    /// active summary + recent tail instead of re-sending the full transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<StoredCompactionState>,
    /// Provider-specific session ID (e.g., Claude Code CLI session for resume)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    /// Stable provider/profile key for session-source filtering (e.g. "openai",
    /// "opencode", "opencode-go").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_key: Option<String>,
    /// Model identifier for this session (e.g., "gpt-5.2-codex")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// API method/runtime route used to select this model (e.g. "openrouter",
    /// "openai-compatible:nvidia-nim", "openai-api").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_api_method: Option<String>,
    /// Provider reasoning/thinking effort for this session (e.g., OpenAI low|medium|high|xhigh).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Optional fixed model to use for subagents launched from this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_model: Option<String>,
    /// Last requested `/improve` mode for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub improve_mode: Option<SessionImproveMode>,
    /// Active review loop (post-completion review rounds), if any. Persisted so
    /// a resumed loop reloads its lens progress and accumulated findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_loop: Option<ReviewLoopState>,
    /// Whether automatic end-of-turn review is enabled for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoreview_enabled: Option<bool>,
    /// Whether automatic end-of-turn judging is enabled for this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autojudge_enabled: Option<bool>,
    /// Whether this session is a canary session (testing new builds)
    #[serde(default)]
    pub is_canary: bool,
    /// Build hash this session is testing (if canary)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub testing_build: Option<String>,
    /// Working directory (for self-dev detection)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Memorable short name (e.g., "fox", "oak")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_name: Option<String>,
    /// Session exit status - why it ended (if not active)
    #[serde(default)]
    pub status: SessionStatus,
    /// PID of the process that last owned this session (for crash detection)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pid: Option<u32>,
    /// Last time the session was marked active
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<DateTime<Utc>>,
    /// Whether this is a debug/test session (created via debug socket)
    #[serde(default)]
    pub is_debug: bool,
    /// Whether this session has been saved/bookmarked by the user
    #[serde(default)]
    pub saved: bool,
    /// Optional user-provided label for saved sessions
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_label: Option<String>,
    /// Environment snapshots for post-mortem debugging
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_snapshots: Vec<EnvSnapshot>,
    /// Memory injection events (for replay visualization)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_injections: Vec<StoredMemoryInjection>,
    /// Non-conversation UI/state events persisted for higher-fidelity replay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replay_events: Vec<StoredReplayEvent>,
    /// Event-sourced session log - single source of truth for all session state.
    ///
    /// Kept `pub(crate)` so callers outside this crate must go through the
    /// `Session` mutation API (which keeps `messages` and the event log in
    /// sync). Direct external mutation would desync the two sources of truth.
    ///
    /// Persisted in the snapshot so the append-only event log survives a
    /// restart as the authoritative record (takeaways #3/#4/#5 target this
    /// end-to-end). `skip_serializing_if` keeps new/empty logs out of the JSON
    /// so a session snapshot written with no events round-trips byte-compatible
    /// with the historical format; load prefers a non-empty persisted log and
    /// otherwise falls back to `rebuild_event_map()` (migration for sessions
    /// written before this field was persisted).
    #[serde(default, skip_serializing_if = "SessionEventMap::is_empty")]
    pub(crate) event_map: SessionEventMap,
    #[serde(skip)]
    persist_state: SessionPersistState,
    #[serde(skip)]
    provider_messages_cache: Vec<Message>,
    #[serde(skip)]
    provider_message_prefix_hashes_cache: Vec<u64>,
    #[serde(skip)]
    provider_messages_cache_len: usize,
    #[serde(skip)]
    provider_messages_cache_mode: PersistVectorMode,
    #[serde(skip)]
    memory_profile_cache: SessionMemoryProfileCache,
    #[serde(skip)]
    memory_profile_dirty: bool,
}

#[derive(Debug, Deserialize)]
struct SessionStartupStub {
    id: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    custom_title: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    compaction: Option<StoredCompactionState>,
    #[serde(default)]
    provider_session_id: Option<String>,
    #[serde(default)]
    provider_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    route_api_method: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    subagent_model: Option<String>,
    #[serde(default)]
    improve_mode: Option<SessionImproveMode>,
    #[serde(default)]
    autoreview_enabled: Option<bool>,
    #[serde(default)]
    autojudge_enabled: Option<bool>,
    #[serde(default)]
    is_canary: bool,
    #[serde(default)]
    testing_build: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    short_name: Option<String>,
    #[serde(default)]
    status: SessionStatus,
    #[serde(default)]
    last_pid: Option<u32>,
    #[serde(default)]
    last_active_at: Option<DateTime<Utc>>,
    #[serde(default)]
    is_debug: bool,
    #[serde(default)]
    saved: bool,
    #[serde(default)]
    save_label: Option<String>,
}

const MAX_SESSION_JOURNAL_BYTES: u64 = 512 * 1024;

/// Max number of environment snapshots to retain per session
const MAX_ENV_SNAPSHOTS: usize = 8;

fn current_working_dir_string() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            let trimmed = v.trim();
            !trimmed.is_empty() && trimmed != "0" && !trimmed.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

fn default_is_test_session() -> bool {
    env_flag_enabled("JCODE_TEST_SESSION")
}

pub fn derive_session_provider_key(provider_name: &str) -> Option<String> {
    let normalized_name = provider_name.trim().to_ascii_lowercase();
    if normalized_name == "jcode" {
        return Some("jcode".to_string());
    }

    if let Ok(runtime_provider) = std::env::var("JCODE_RUNTIME_PROVIDER") {
        let runtime_provider = runtime_provider.trim().to_ascii_lowercase();
        if !runtime_provider.is_empty() && runtime_provider != "openai-compatible" {
            return Some(runtime_provider);
        }
    }

    if let Ok(namespace) = std::env::var("JCODE_OPENROUTER_CACHE_NAMESPACE") {
        let namespace = namespace.trim().to_ascii_lowercase();
        if !namespace.is_empty() {
            return Some(namespace);
        }
    }

    if let Ok(active) = std::env::var("JCODE_ACTIVE_PROVIDER") {
        let active = active.trim().to_ascii_lowercase();
        if !active.is_empty() {
            return Some(active);
        }
    }

    let fallback = match normalized_name.as_str() {
        "anthropic" | "claude" | "claude cli" => "claude",
        "openai" => "openai",
        "github copilot" | "copilot" => "copilot",
        "openrouter" => "openrouter",
        "cursor" => "cursor",
        "gemini" => "gemini",
        "antigravity" => "antigravity",
        "" => return None,
        other => other,
    };

    Some(fallback.to_string())
}

impl Session {
    fn session_from_startup_stub(stub: SessionStartupStub) -> Self {
        let mut session = Self::create_with_id(stub.id, stub.parent_id, stub.title);
        session.custom_title = stub.custom_title;
        session.created_at = stub.created_at;
        session.updated_at = stub.updated_at;
        session.compaction = stub.compaction;
        session.provider_session_id = stub.provider_session_id;
        session.provider_key = stub.provider_key;
        session.model = stub.model;
        session.route_api_method = stub.route_api_method;
        session.reasoning_effort = stub.reasoning_effort;
        session.subagent_model = stub.subagent_model;
        session.improve_mode = stub.improve_mode;
        session.autoreview_enabled = stub.autoreview_enabled;
        session.autojudge_enabled = stub.autojudge_enabled;
        session.is_canary = stub.is_canary;
        session.testing_build = stub.testing_build;
        session.working_dir = stub.working_dir;
        session.short_name = stub.short_name;
        session.status = stub.status;
        session.last_pid = stub.last_pid;
        session.last_active_at = stub.last_active_at;
        session.is_debug = stub.is_debug;
        session.saved = stub.saved;
        session.save_label = stub.save_label;
        session.messages.clear();
        session.env_snapshots.clear();
        session.memory_injections.clear();
        session.replay_events.clear();
        // Keep the (empty) event log consistent with the stripped legacy state
        // so any later derived view on this stub stays in sync.
        session.rebuild_event_map();
        session.rebuild_memory_profile_cache();
        session.reset_persist_state(true);
        session
    }

    fn session_from_remote_startup_snapshot(snapshot: RemoteStartupSessionSnapshot) -> Self {
        let mut session = Self::create_with_id(snapshot.id, snapshot.parent_id, snapshot.title);
        session.custom_title = snapshot.custom_title;
        session.created_at = snapshot.created_at;
        session.updated_at = snapshot.updated_at;
        session.messages = snapshot.messages;
        session.compaction = snapshot.compaction;
        session.provider_session_id = snapshot.provider_session_id;
        session.provider_key = snapshot.provider_key;
        session.model = snapshot.model;
        session.route_api_method = snapshot.route_api_method;
        session.reasoning_effort = snapshot.reasoning_effort;
        session.subagent_model = snapshot.subagent_model;
        session.improve_mode = snapshot.improve_mode;
        session.autoreview_enabled = snapshot.autoreview_enabled;
        session.autojudge_enabled = snapshot.autojudge_enabled;
        session.is_canary = snapshot.is_canary;
        session.testing_build = snapshot.testing_build;
        session.working_dir = snapshot.working_dir;
        session.short_name = snapshot.short_name;
        session.status = snapshot.status;
        session.last_pid = snapshot.last_pid;
        session.last_active_at = snapshot.last_active_at;
        session.is_debug = snapshot.is_debug;
        session.saved = snapshot.saved;
        session.save_label = snapshot.save_label;
        session.replay_events.clear();
        session.env_snapshots.clear();
        session.memory_injections.clear();
        // Rebuild so the (non-empty) event log agrees with the snapshot vectors.
        session.rebuild_event_map();
        session.mark_memory_profile_dirty();
        session.reset_persist_state(true);
        session.reset_provider_messages_cache();
        session
    }

    pub fn debug_memory_profile(&self) -> serde_json::Value {
        let message_stats =
            summarize_message_content(self.messages.iter().map(|message| &message.content));

        let session_message_json_bytes: usize = self.messages.iter().map(estimate_json_bytes).sum();
        let provider_cache_stats = summarize_message_content(
            self.provider_messages_cache
                .iter()
                .map(|message| &message.content),
        );
        let provider_messages_cache_json_bytes: usize = self
            .provider_messages_cache
            .iter()
            .map(estimate_json_bytes)
            .sum();
        let env_snapshots_json_bytes: usize =
            self.env_snapshots.iter().map(estimate_json_bytes).sum();
        let memory_injections_json_bytes: usize =
            self.memory_injections.iter().map(estimate_json_bytes).sum();
        let replay_events_json_bytes: usize =
            self.replay_events.iter().map(estimate_json_bytes).sum();
        let event_log_json_bytes: usize = self
            .event_map
            .events
            .iter()
            .map(estimate_json_bytes)
            .sum();
        let compaction_json_bytes = self
            .compaction
            .as_ref()
            .map(estimate_json_bytes)
            .unwrap_or(0);
        let compaction_summary_bytes = self
            .compaction
            .as_ref()
            .map(|c| c.summary_text.len())
            .unwrap_or(0);
        let compaction_encrypted_bytes = self
            .compaction
            .as_ref()
            .and_then(|c| c.openai_encrypted_content.as_ref())
            .map(|text| text.len())
            .unwrap_or(0);

        serde_json::json!({
            "session_id": self.id,
            "messages": {
                "count": self.messages.len(),
                "json_bytes": session_message_json_bytes,
                "memory": message_stats.to_json(),
            },
            "compaction": {
                "present": self.compaction.is_some(),
                "covers_up_to_turn": self
                    .compaction
                    .as_ref()
                    .map(|c| c.covers_up_to_turn)
                    .unwrap_or(0),
                "original_turn_count": self
                    .compaction
                    .as_ref()
                    .map(|c| c.original_turn_count)
                    .unwrap_or(0),
                "compacted_count": self
                    .compaction
                    .as_ref()
                    .map(|c| c.compacted_count)
                    .unwrap_or(0),
                "json_bytes": compaction_json_bytes,
                "summary_text_bytes": compaction_summary_bytes,
                "encrypted_content_bytes": compaction_encrypted_bytes,
            },
            "env_snapshots": {
                "count": self.env_snapshots.len(),
                "json_bytes": env_snapshots_json_bytes,
            },
            "memory_injections": {
                "count": self.memory_injections.len(),
                "json_bytes": memory_injections_json_bytes,
            },
            "replay_events": {
                "count": self.replay_events.len(),
                "json_bytes": replay_events_json_bytes,
            },
            "event_log": {
                "count": self.event_map.events.len(),
                "json_bytes": event_log_json_bytes,
            },
            "provider_messages_cache": {
                "count": self.provider_messages_cache.len(),
                "source_len": self.provider_messages_cache_len,
                "mode": persist_vector_mode_label(self.provider_messages_cache_mode),
                "json_bytes": provider_messages_cache_json_bytes,
                "memory": provider_cache_stats.to_json(),
            },
            "totals": {
                "payload_text_bytes": message_stats.payload_text_bytes(),
                "json_bytes": session_message_json_bytes
                    + provider_messages_cache_json_bytes
                    + env_snapshots_json_bytes
                    + memory_injections_json_bytes
                    + replay_events_json_bytes
                    + event_log_json_bytes
                    + compaction_json_bytes,
                "canonical_transcript_json_bytes": session_message_json_bytes,
                "provider_cache_json_bytes": provider_messages_cache_json_bytes,
                "canonical_tool_result_bytes": message_stats.tool_result_bytes,
                "provider_cache_tool_result_bytes": provider_cache_stats.tool_result_bytes,
                "canonical_large_blob_bytes": message_stats.large_block_bytes,
                "provider_cache_large_blob_bytes": provider_cache_stats.large_block_bytes,
            }
        })
    }

    fn journal_meta(&self) -> SessionJournalMeta {
        SessionJournalMeta {
            parent_id: self.parent_id.clone(),
            title: self.title.clone(),
            custom_title: self.custom_title.clone(),
            updated_at: self.updated_at,
            compaction: self.compaction.clone(),
            provider_session_id: self.provider_session_id.clone(),
            provider_key: self.provider_key.clone(),
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            subagent_model: self.subagent_model.clone(),
            improve_mode: self.improve_mode,
            review_loop: self.review_loop.clone(),
            autoreview_enabled: self.autoreview_enabled,
            autojudge_enabled: self.autojudge_enabled,
            is_canary: self.is_canary,
            testing_build: self.testing_build.clone(),
            working_dir: self.working_dir.clone(),
            short_name: self.short_name.clone(),
            status: self.status.clone(),
            last_pid: self.last_pid,
            last_active_at: self.last_active_at,
            is_debug: self.is_debug,
            saved: self.saved,
            save_label: self.save_label.clone(),
        }
    }

    fn reset_persist_state(&mut self, snapshot_exists: bool) {
        self.persist_state = SessionPersistState {
            snapshot_exists,
            messages_len: self.messages.len(),
            env_snapshots_len: self.env_snapshots.len(),
            memory_injections_len: self.memory_injections.len(),
            replay_events_len: self.replay_events.len(),
            events_len: self.event_map.events.len(),
            messages_mode: PersistVectorMode::Clean,
            env_snapshots_mode: PersistVectorMode::Clean,
            memory_injections_mode: PersistVectorMode::Clean,
            replay_events_mode: PersistVectorMode::Clean,
            events_mode: PersistVectorMode::Clean,
            last_meta: Some(self.journal_meta()),
        };
    }

    fn reset_provider_messages_cache(&mut self) {
        self.provider_messages_cache.clear();
        self.provider_message_prefix_hashes_cache.clear();
        self.provider_messages_cache_len = 0;
        self.provider_messages_cache_mode = PersistVectorMode::Full;
        self.memory_profile_cache.provider_cache_count = 0;
        self.memory_profile_cache.provider_cache_json_bytes = 0;
        self.memory_profile_cache.provider_cache_stats = ContentBlockMemoryStats::default();
    }

    /// Drop the derived provider-facing transcript once the current request has
    /// copied the messages it needs. The canonical [`StoredMessage`] history is
    /// still retained, so this cache can be rebuilt on the next provider call.
    ///
    /// Long-running server sessions otherwise keep two fully owned transcript
    /// copies while waiting on the network. Tool results and reasoning payloads
    /// can make that duplicate tens of MiB per active session.
    pub fn release_provider_messages_cache(&mut self) {
        self.provider_messages_cache = Vec::new();
        self.provider_message_prefix_hashes_cache = Vec::new();
        self.provider_messages_cache_len = 0;
        self.provider_messages_cache_mode = PersistVectorMode::Full;
        self.memory_profile_cache.provider_cache_count = 0;
        self.memory_profile_cache.provider_cache_json_bytes = 0;
        self.memory_profile_cache.provider_cache_stats = ContentBlockMemoryStats::default();
    }

    fn push_provider_message_cache_entry(&mut self, message: Message) {
        let message_hash = crate::message::stable_message_hash(&message);
        let prefix_hash = self
            .provider_message_prefix_hashes_cache
            .last()
            .copied()
            .map(|prev| crate::message::extend_stable_hash(prev, message_hash))
            .unwrap_or(message_hash);
        self.memory_profile_cache.provider_cache_count += 1;
        self.memory_profile_cache.provider_cache_json_bytes += estimate_json_bytes(&message);
        self.memory_profile_cache
            .provider_cache_stats
            .merge_from(&summarize_blocks(&message.content));
        self.provider_messages_cache.push(message);
        self.provider_message_prefix_hashes_cache.push(prefix_hash);
    }

    fn mark_memory_profile_dirty(&mut self) {
        self.memory_profile_dirty = true;
    }

    fn rebuild_memory_profile_cache(&mut self) {
        let message_stats =
            summarize_message_content(self.messages.iter().map(|message| &message.content));
        let provider_cache_stats = summarize_message_content(
            self.provider_messages_cache
                .iter()
                .map(|message| &message.content),
        );

        self.memory_profile_cache = SessionMemoryProfileCache {
            messages_count: self.messages.len(),
            messages_json_bytes: self.messages.iter().map(estimate_json_bytes).sum(),
            message_stats,
            env_snapshots_count: self.env_snapshots.len(),
            env_snapshots_json_bytes: self.env_snapshots.iter().map(estimate_json_bytes).sum(),
            memory_injections_count: self.memory_injections.len(),
            memory_injections_json_bytes: self
                .memory_injections
                .iter()
                .map(estimate_json_bytes)
                .sum(),
            replay_events_count: self.replay_events.len(),
            replay_events_json_bytes: self.replay_events.iter().map(estimate_json_bytes).sum(),
            event_log_count: self.event_map.events.len(),
            event_log_json_bytes: self
                .event_map
                .events
                .iter()
                .map(estimate_json_bytes)
                .sum(),
            provider_cache_count: self.provider_messages_cache.len(),
            provider_cache_json_bytes: self
                .provider_messages_cache
                .iter()
                .map(estimate_json_bytes)
                .sum(),
            provider_cache_stats,
        };
        self.memory_profile_dirty = false;
    }

    fn ensure_memory_profile_cache(&mut self) {
        if self.memory_profile_dirty {
            self.rebuild_memory_profile_cache();
        }
    }

    pub fn memory_profile_snapshot(&mut self) -> SessionMemoryProfileSnapshot {
        self.ensure_memory_profile_cache();
        let compaction_json_bytes = self
            .compaction
            .as_ref()
            .map(estimate_json_bytes)
            .unwrap_or(0);

        SessionMemoryProfileSnapshot {
            message_count: self.memory_profile_cache.messages_count,
            provider_cache_message_count: self.memory_profile_cache.provider_cache_count,
            env_snapshot_count: self.memory_profile_cache.env_snapshots_count,
            memory_injection_count: self.memory_profile_cache.memory_injections_count,
            replay_event_count: self.memory_profile_cache.replay_events_count,
            event_log_count: self.memory_profile_cache.event_log_count,
            event_log_json_bytes: self.memory_profile_cache.event_log_json_bytes,
            payload_text_bytes: self.memory_profile_cache.message_stats.payload_text_bytes(),
            total_json_bytes: self.memory_profile_cache.messages_json_bytes
                + self.memory_profile_cache.provider_cache_json_bytes
                + self.memory_profile_cache.env_snapshots_json_bytes
                + self.memory_profile_cache.memory_injections_json_bytes
                + self.memory_profile_cache.replay_events_json_bytes
                + self.memory_profile_cache.event_log_json_bytes
                + compaction_json_bytes,
            provider_cache_json_bytes: self.memory_profile_cache.provider_cache_json_bytes,
            canonical_tool_result_bytes: self.memory_profile_cache.message_stats.tool_result_bytes,
            provider_cache_tool_result_bytes: self
                .memory_profile_cache
                .provider_cache_stats
                .tool_result_bytes,
            canonical_large_blob_bytes: self.memory_profile_cache.message_stats.large_block_bytes,
            provider_cache_large_blob_bytes: self
                .memory_profile_cache
                .provider_cache_stats
                .large_block_bytes,
        }
    }

    fn mark_messages_append_dirty(&mut self) {
        if self.persist_state.messages_mode != PersistVectorMode::Full {
            self.persist_state.messages_mode = PersistVectorMode::Append;
        }
        if self.provider_messages_cache_mode != PersistVectorMode::Full {
            self.provider_messages_cache_mode = PersistVectorMode::Append;
        }
    }

    fn mark_messages_full_dirty(&mut self) {
        self.persist_state.messages_mode = PersistVectorMode::Full;
        self.provider_messages_cache_mode = PersistVectorMode::Full;
    }

    fn mark_env_snapshots_append_dirty(&mut self) {
        if self.persist_state.env_snapshots_mode != PersistVectorMode::Full {
            self.persist_state.env_snapshots_mode = PersistVectorMode::Append;
        }
    }

    fn mark_env_snapshots_full_dirty(&mut self) {
        self.persist_state.env_snapshots_mode = PersistVectorMode::Full;
    }

    fn mark_memory_injections_append_dirty(&mut self) {
        if self.persist_state.memory_injections_mode != PersistVectorMode::Full {
            self.persist_state.memory_injections_mode = PersistVectorMode::Append;
        }
    }

    fn mark_replay_events_append_dirty(&mut self) {
        if self.persist_state.replay_events_mode != PersistVectorMode::Full {
            self.persist_state.replay_events_mode = PersistVectorMode::Append;
        }
    }

    /// Mark that the event log was reconstructed (not merely appended to), so a
    /// tail-delta journal entry would not capture the change. Forces a full
    /// snapshot on the next save.
    fn mark_events_full_dirty(&mut self) {
        self.persist_state.events_mode = PersistVectorMode::Full;
    }

    fn apply_journal_meta(&mut self, meta: SessionJournalMeta) {
        self.parent_id = meta.parent_id;
        self.title = meta.title;
        self.custom_title = meta.custom_title;
        self.updated_at = meta.updated_at;
        self.compaction = meta.compaction;
        self.provider_session_id = meta.provider_session_id;
        self.provider_key = meta.provider_key;
        self.model = meta.model;
        self.reasoning_effort = meta.reasoning_effort;
        self.subagent_model = meta.subagent_model;
        self.improve_mode = meta.improve_mode;
        self.autoreview_enabled = meta.autoreview_enabled;
        self.autojudge_enabled = meta.autojudge_enabled;
        self.is_canary = meta.is_canary;
        self.testing_build = meta.testing_build;
        self.working_dir = meta.working_dir;
        self.short_name = meta.short_name;
        self.status = meta.status;
        self.last_pid = meta.last_pid;
        self.last_active_at = meta.last_active_at;
        self.is_debug = meta.is_debug;
        self.saved = meta.saved;
        self.save_label = meta.save_label;
        self.mark_memory_profile_dirty();
    }

    pub fn create_with_id(
        session_id: String,
        parent_id: Option<String>,
        title: Option<String>,
    ) -> Self {
        let now = Utc::now();
        let is_debug = default_is_test_session();
        // Try to extract short name from ID if it's a memorable ID
        let short_name = extract_session_name(&session_id).map(|s| s.to_string());
        let mut session = Self {
            id: session_id,
            parent_id,
            title,
            custom_title: None,
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
            compaction: None,
            provider_session_id: None,
            provider_key: None,
            model: None,
            route_api_method: None,
            reasoning_effort: None,
            subagent_model: None,
            improve_mode: None,
            review_loop: None,
            autoreview_enabled: None,
            autojudge_enabled: None,
            is_canary: false,
            testing_build: None,
            working_dir: current_working_dir_string(),
            short_name,
            status: SessionStatus::Active,
            last_pid: Some(std::process::id()),
            last_active_at: Some(now),
            is_debug,
            saved: false,
            save_label: None,
            env_snapshots: Vec::new(),
            memory_injections: Vec::new(),
            replay_events: Vec::new(),
            event_map: SessionEventMap::default(),
            persist_state: SessionPersistState::default(),
            provider_messages_cache: Vec::new(),
            provider_message_prefix_hashes_cache: Vec::new(),
            provider_messages_cache_len: 0,
            provider_messages_cache_mode: PersistVectorMode::Full,
            memory_profile_cache: SessionMemoryProfileCache::default(),
            memory_profile_dirty: false,
        };
        session.reset_persist_state(false);
        session
    }

    pub fn create(parent_id: Option<String>, title: Option<String>) -> Self {
        let now = Utc::now();
        // Keep memorable identities distinct across all currently active
        // sessions. This naturally covers swarm members and survives a server
        // reload because active PID markers retain their encoded short names.
        let used_names = active_session_ids()
            .into_iter()
            .filter_map(|session_id| extract_session_name(&session_id).map(str::to_string))
            .collect::<HashSet<_>>();
        let (id, short_name) = new_memorable_session_id_avoiding(&used_names);
        let is_debug = default_is_test_session();
        let mut session = Self {
            id,
            parent_id,
            title,
            custom_title: None,
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
            compaction: None,
            provider_session_id: None,
            provider_key: None,
            model: None,
            route_api_method: None,
            reasoning_effort: None,
            subagent_model: None,
            improve_mode: None,
            review_loop: None,
            autoreview_enabled: None,
            autojudge_enabled: None,
            is_canary: false,
            testing_build: None,
            working_dir: current_working_dir_string(),
            short_name: Some(short_name),
            status: SessionStatus::Active,
            last_pid: Some(std::process::id()),
            last_active_at: Some(now),
            is_debug,
            saved: false,
            save_label: None,
            env_snapshots: Vec::new(),
            memory_injections: Vec::new(),
            replay_events: Vec::new(),
            event_map: SessionEventMap::default(),
            persist_state: SessionPersistState::default(),
            provider_messages_cache: Vec::new(),
            provider_message_prefix_hashes_cache: Vec::new(),
            provider_messages_cache_len: 0,
            provider_messages_cache_mode: PersistVectorMode::Full,
            memory_profile_cache: SessionMemoryProfileCache::default(),
            memory_profile_dirty: false,
        };
        session.reset_persist_state(false);
        session
    }

    /// Mark this session as a debug/test session
    pub fn set_debug(&mut self, is_debug: bool) {
        self.is_debug = is_debug;
        // Debug status can change after activation (e.g. debug-socket created
        // sessions); keep presence UIs in sync when we are the active owner.
        if self.status == SessionStatus::Active {
            self.sync_internal_presence_flag();
        }
    }

    /// Save/bookmark this session with an optional label
    pub fn mark_saved(&mut self, label: Option<String>) {
        self.saved = true;
        if label.is_some() {
            self.save_label = label;
        }
    }

    /// Remove the saved/bookmark status
    pub fn unmark_saved(&mut self) {
        self.saved = false;
        self.save_label = None;
    }

    /// Set or clear the user-provided display title.
    ///
    /// This intentionally does not change the immutable session id, memorable
    /// short name, generated title, provider session id, or saved/bookmark label.
    pub fn rename_title(&mut self, title: Option<String>) {
        self.custom_title = title.and_then(|title| {
            let title = title.trim();
            (!title.is_empty()).then(|| title.to_string())
        });
        self.updated_at = Utc::now();
    }

    /// Get the title users should see for this session: custom rename first,
    /// then the generated/imported title, if one exists.
    pub fn display_title(&self) -> Option<&str> {
        fn non_empty_trimmed(title: Option<&str>) -> Option<&str> {
            title.map(str::trim).filter(|title| !title.is_empty())
        }

        non_empty_trimmed(self.custom_title.as_deref())
            .or_else(|| non_empty_trimmed(self.title.as_deref()))
    }

    /// Get a visible label for title-oriented surfaces, falling back to the
    /// memorable session name when there is no generated or custom title.
    pub fn display_title_or_name(&self) -> &str {
        self.display_title().unwrap_or_else(|| self.display_name())
    }

    /// Record an environment snapshot for post-mortem debugging
    pub fn record_env_snapshot(&mut self, snapshot: EnvSnapshot) {
        self.memory_profile_cache.env_snapshots_count += 1;
        self.memory_profile_cache.env_snapshots_json_bytes += estimate_json_bytes(&snapshot);
        self.env_snapshots.push(snapshot);
        if self.env_snapshots.len() > MAX_ENV_SNAPSHOTS {
            let excess = self.env_snapshots.len() - MAX_ENV_SNAPSHOTS;
            self.env_snapshots.drain(0..excess);
            self.mark_memory_profile_dirty();
            self.mark_env_snapshots_full_dirty();
        } else {
            self.mark_env_snapshots_append_dirty();
        }
    }

    pub fn has_session_context_message(&self) -> bool {
        self.messages.iter().any(|message| {
            message.content.iter().any(|block| match block {
                ContentBlock::Text { text, .. } => text.starts_with(SESSION_CONTEXT_PREFIX),
                _ => false,
            })
        })
    }

    /// Whether the session carries any message content beyond the auto-added
    /// session-context placeholder. Sessions whose only transcript is that
    /// context stub are effectively untouched and may be skipped on save.
    pub(crate) fn has_message_beyond_session_context(&self) -> bool {
        self.messages.iter().any(|message| {
            message.content.iter().any(|block| match block {
                ContentBlock::Text { text, .. } => !text.starts_with(SESSION_CONTEXT_PREFIX),
                _ => true,
            })
        })
    }

    /// Persist an immutable session-context snapshot as the first provider-visible
    /// transcript item for new sessions. Existing non-empty sessions are left
    /// untouched so their historical context is never rewritten with newer state.
    pub fn ensure_initial_session_context_message(&mut self) -> bool {
        if !self.messages.is_empty() || self.has_session_context_message() {
            return false;
        }

        // Preserve an explicitly bound session directory. Shared-server clients
        // provide their cwd before this message is created, and replacing it with
        // the daemon process cwd would leak the directory that launched the server.
        if self.working_dir.is_none() {
            self.working_dir = current_working_dir_string();
        }

        let context =
            crate::prompt::build_session_context(self.working_dir.as_deref().map(Path::new));
        let wrapped = format!("<system-reminder>\n{}\n</system-reminder>", context.trim());
        self.add_message_with_display_role(
            Role::User,
            vec![ContentBlock::Text {
                text: wrapped,
                cache_control: None,
            }],
            Some(StoredDisplayRole::System),
        );
        true
    }

    /// Refresh the initial immutable session-context message if the session has
    /// not started a real conversation yet. This covers remote/client-server
    /// startup where the server creates an Agent before the subscribing client
    /// sends the terminal working directory that tools will use.
    pub fn refresh_initial_session_context_message(&mut self) -> bool {
        if self.messages.iter().any(is_visible_conversation_message) {
            return false;
        }

        let Some(message) = self.messages.iter_mut().find(|message| {
            message.content.iter().any(|block| match block {
                ContentBlock::Text { text, .. } => text.starts_with(SESSION_CONTEXT_PREFIX),
                _ => false,
            })
        }) else {
            return false;
        };

        let context =
            crate::prompt::build_session_context(self.working_dir.as_deref().map(Path::new));
        let wrapped = format!("<system-reminder>\n{}\n</system-reminder>", context.trim());
        for block in &mut message.content {
            if let ContentBlock::Text { text, .. } = block
                && text.starts_with(SESSION_CONTEXT_PREFIX)
            {
                if *text == wrapped {
                    return false;
                }
                *text = wrapped;
                self.mark_memory_profile_dirty();
                self.mark_messages_full_dirty();
                self.record_transcript_replacement();
                return true;
            }
        }

        false
    }

    /// Get the display name for this session (short memorable name if available)
    pub fn display_name(&self) -> &str {
        self.short_name
            .as_deref()
            .or_else(|| extract_session_name(&self.id))
            .unwrap_or(&self.id)
    }

    /// Append a model-visible notice telling the agent this session is a fork
    /// of `parent_session_id`'s conversation.
    ///
    /// Forking happens when the user splits a window mid-conversation (often
    /// while the parent agent is still streaming) and points the new window at
    /// a clone of the transcript. Without this notice the forked agent assumes
    /// it owns the in-flight request, duplicating the parent's work. The
    /// notice is wrapped in `<system-reminder>` so it stays out of the visible
    /// transcript while still reaching the model on the next turn.
    pub fn append_fork_notice(&mut self, parent_session_id: &str, parent_display_name: &str) {
        let text = format!(
            "<system-reminder>\nThis session was forked (split) from session {parent} ({parent_id}) by the user. \
The full conversation above is inherited from that session, but the original agent in {parent} \
is still active and will continue handling whatever request or work was in progress there. \
Do NOT continue or duplicate that in-flight work here. Treat the next user message as a fresh \
request in this new forked session, using the inherited conversation only as context.\n</system-reminder>",
            parent = parent_display_name,
            parent_id = parent_session_id,
        );
        self.add_message_with_display_role(
            Role::User,
            vec![ContentBlock::Text {
                text,
                cache_control: None,
            }],
            Some(StoredDisplayRole::System),
        );
    }

    /// Append a model-visible notice that the session's working directory
    /// changed (e.g. the user ran `/cd` to move into a linked git worktree).
    ///
    /// Unlike `refresh_initial_session_context_message`, this works even after
    /// a conversation has progressed, because it appends a fresh
    /// `<system-reminder>` rather than rewriting history. It lets the model
    /// re-scope AGENTS.md / skills / tool cwd to the new directory on the next
    /// turn without disturbing the existing transcript.
    pub fn append_working_dir_notice(&mut self, old_dir: &str, new_dir: &str) {
        let text = format!(
            "<system-reminder>\nThe session working directory changed from `{old}` to `{new}` by the user. \
Re-scope your context to the new directory: AGENTS.md, project skills, and the default cwd for shell/read/write \
tools all follow it. Do not assume the previous directory still applies.\n</system-reminder>",
            old = old_dir,
            new = new_dir,
        );
        self.add_message_with_display_role(
            Role::User,
            vec![ContentBlock::Text {
                text,
                cache_control: None,
            }],
            Some(StoredDisplayRole::System),
        );
    }

    /// Mark this session as a canary tester
    pub fn set_canary(&mut self, build_hash: &str) {
        self.is_canary = true;
        self.testing_build = Some(build_hash.to_string());
    }

    /// Clear canary status
    pub fn clear_canary(&mut self) {
        self.is_canary = false;
        self.testing_build = None;
    }

    /// Set the session status
    pub fn set_status(&mut self, status: SessionStatus) {
        self.status = status;
    }

    /// Mark session as closed normally
    pub fn mark_closed(&mut self) {
        self.status = SessionStatus::Closed;
        unregister_active_pid(&self.id);
    }

    /// Mark session as crashed
    pub fn mark_crashed(&mut self, message: Option<String>) {
        self.status = SessionStatus::Crashed { message };
        unregister_active_pid(&self.id);
    }

    /// Mark session as having an error
    pub fn mark_error(&mut self, message: String) {
        self.status = SessionStatus::Error { message };
    }

    /// Mark session as active (e.g., when resuming)
    pub fn mark_active(&mut self) {
        self.status = SessionStatus::Active;
        let pid = std::process::id();
        self.last_pid = Some(pid);
        self.last_active_at = Some(Utc::now());
        register_active_pid(&self.id, pid);
        self.sync_internal_presence_flag();
    }

    /// Mark session as active for a specific PID
    pub fn mark_active_with_pid(&mut self, pid: u32) {
        self.status = SessionStatus::Active;
        self.last_pid = Some(pid);
        self.last_active_at = Some(Utc::now());
        register_active_pid(&self.id, pid);
        self.sync_internal_presence_flag();
    }

    /// Keep the on-disk internal-session flag in sync with this session's
    /// role. Debug/test sessions and spawned children (swarm workers,
    /// subagents) are internal: they stay tracked for lifecycle purposes but
    /// are hidden from user-facing presence UIs like the menu bar (issue
    /// #508).
    fn sync_internal_presence_flag(&self) {
        let internal = self.is_debug || self.parent_id.is_some();
        crate::storage::set_session_internal(&self.id, internal);
    }

    /// Detect if an active session likely crashed (process no longer running)
    /// Returns true if status was updated.
    pub fn detect_crash(&mut self) -> bool {
        if self.status != SessionStatus::Active {
            return false;
        }

        if let Some(pid) = self.last_pid {
            if !crash::is_pid_running(pid) {
                self.mark_crashed(Some(format!(
                    "Process {} exited unexpectedly (no shutdown signal captured)",
                    pid
                )));
                return true;
            }
        } else {
            // No PID info (older sessions): fall back to age heuristic
            let age = Utc::now().signed_duration_since(self.updated_at);
            if age.num_seconds() > 120 {
                self.mark_crashed(Some(
                    "Stale active session (possible abrupt termination)".to_string(),
                ));
                return true;
            }
        }

        false
    }

    /// Check if this session is working on the jcode repository
    pub fn is_self_dev(&self) -> bool {
        if let Some(ref dir) = self.working_dir {
            // Check if working dir contains jcode source
            let path = std::path::Path::new(dir);
            path.join("Cargo.toml").exists()
                && path.join("src/main.rs").exists()
                && std::fs::read_to_string(path.join("Cargo.toml"))
                    .map(|s| s.contains("name = \"jcode\""))
                    .unwrap_or(false)
        } else {
            false
        }
    }

    pub fn redacted_for_export(&self) -> Self {
        let mut redacted = self.clone();
        if let Some(title) = redacted.title.as_mut() {
            *title = crate::message::redact_secrets(title);
        }
        if let Some(title) = redacted.custom_title.as_mut() {
            *title = crate::message::redact_secrets(title);
        }
        if let Some(compaction) = redacted.compaction.as_mut() {
            compaction.summary_text = crate::message::redact_secrets(&compaction.summary_text);
        }
        for msg in &mut redacted.messages {
            for block in &mut msg.content {
                match block {
                    ContentBlock::Text { text, .. }
                    | ContentBlock::Reasoning { text }
                    | ContentBlock::ReasoningTrace { text } => {
                        *text = crate::message::redact_secrets(text);
                    }
                    ContentBlock::AnthropicThinking { thinking, .. } => {
                        *thinking = crate::message::redact_secrets(thinking);
                    }
                    ContentBlock::OpenAIReasoning { summary, .. } => {
                        for item in summary {
                            *item = crate::message::redact_secrets(item);
                        }
                    }
                    ContentBlock::ToolResult { content, .. } => {
                        *content = crate::message::redact_secrets(content);
                    }
                    ContentBlock::ToolUse { input, .. } => redact_json_value(input),
                    ContentBlock::Image { .. } => {}
                    ContentBlock::OpenAICompaction { .. } => {}
                }
            }
        }
        for event in &mut redacted.replay_events {
            match &mut event.kind {
                StoredReplayEventKind::DisplayMessage { title, content, .. } => {
                    if let Some(title) = title.as_mut() {
                        *title = crate::message::redact_secrets(title);
                    }
                    *content = crate::message::redact_secrets(content);
                }
                StoredReplayEventKind::SwarmStatus { members } => {
                    for member in members {
                        if let Some(detail) = member.detail.as_mut() {
                            *detail = crate::message::redact_secrets(detail);
                        }
                    }
                }
                StoredReplayEventKind::SwarmPlan { items, reason, .. } => {
                    if let Some(reason) = reason.as_mut() {
                        *reason = crate::message::redact_secrets(reason);
                    }
                    for item in items {
                        item.content = crate::message::redact_secrets(&item.content);
                    }
                }
            }
        }
        redacted
    }

    pub fn token_usage_totals(&self) -> crate::protocol::TokenUsageTotals {
        let mut totals = crate::protocol::TokenUsageTotals::default();
        for message in &self.messages {
            let Some(usage) = message.token_usage.as_ref() else {
                continue;
            };
            totals.messages_with_token_usage = totals.messages_with_token_usage.saturating_add(1);
            totals.input_tokens = totals.input_tokens.saturating_add(usage.input_tokens);
            totals.output_tokens = totals.output_tokens.saturating_add(usage.output_tokens);
            if usage.cache_read_input_tokens.is_some()
                || usage.cache_creation_input_tokens.is_some()
            {
                totals.cache_reported_input_tokens = totals
                    .cache_reported_input_tokens
                    .saturating_add(usage.input_tokens);
            }
            totals.cache_read_input_tokens = totals
                .cache_read_input_tokens
                .saturating_add(usage.cache_read_input_tokens.unwrap_or(0));
            totals.cache_creation_input_tokens = totals
                .cache_creation_input_tokens
                .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0));
        }
        totals
    }

    pub fn add_message(&mut self, role: Role, content: Vec<ContentBlock>) -> String {
        self.add_message_ext_with_display_role(role, content, None, None, None)
    }

    pub fn add_message_with_duration(
        &mut self,
        role: Role,
        content: Vec<ContentBlock>,
        tool_duration_ms: Option<u64>,
    ) -> String {
        self.add_message_ext_with_display_role(role, content, tool_duration_ms, None, None)
    }

    pub fn add_message_with_display_role(
        &mut self,
        role: Role,
        content: Vec<ContentBlock>,
        display_role: Option<StoredDisplayRole>,
    ) -> String {
        self.add_message_ext_with_display_role(role, content, None, None, display_role)
    }

    pub fn add_message_ext(
        &mut self,
        role: Role,
        content: Vec<ContentBlock>,
        tool_duration_ms: Option<u64>,
        token_usage: Option<StoredTokenUsage>,
    ) -> String {
        self.add_message_ext_with_display_role(role, content, tool_duration_ms, token_usage, None)
    }

    pub fn add_message_ext_with_display_role(
        &mut self,
        role: Role,
        content: Vec<ContentBlock>,
        tool_duration_ms: Option<u64>,
        token_usage: Option<StoredTokenUsage>,
        display_role: Option<StoredDisplayRole>,
    ) -> String {
        let id = new_id("message");
        self.append_stored_message(StoredMessage {
            id: id.clone(),
            role,
            content,
            display_role,
            timestamp: Some(Utc::now()),
            tool_duration_ms,
            token_usage,
        });
        id
    }

    /// Emit a `ReplaceMessages` event capturing the full current transcript.
    ///
    /// Used by in-place transcript mutations (`strip_oversized_images`,
    /// `emergency_truncate_tool_results`, `remove_tool_use_blocks`, and
    /// `refresh_initial_session_context_message`) that modify `self.messages`
    /// directly without re-entering an event-emitting append/insert/replace
    /// path. Emitting a full replacement keeps the event log the single source
    /// of truth and stays robust regardless of where in the log the mutation
    /// lands.
    fn record_transcript_replacement(&mut self) {
        let event = SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: crate::id::new_id("transcript_mutation"),
            op: SessionEventOp::ReplaceMessages {
                start_index: 0,
                end_index: usize::MAX,
                messages: self.messages.clone(),
            },
            parent_id: None,
            version: 1,
        };
        self.event_map.append_event(event);
    }

    pub fn append_stored_message(&mut self, message: StoredMessage) {
        // Ensure a stable event id even when the message id is empty, so the
        // event log and the legacy `messages` vector never diverge (an empty
        // event_id is rejected by validation, which would skip the event while
        // the message is still pushed below).
        let message_id = if message.id.is_empty() {
            crate::id::new_id("message")
        } else {
            message.id.clone()
        };
        let event = SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: message_id.clone(),
            op: SessionEventOp::AppendMessage {
                message_id: message_id.clone(),
                message: message.clone(),
            },
            parent_id: None,
            version: 1,
        };
        let events_before = self.event_map.events.len();
        self.event_map.append_event(event);
        // If validation rejected the event (e.g. an empty-content message from
        // an external import), the legacy vector below will still receive the
        // message. Rebuild the log from the legacy vector afterwards so the two
        // sources of truth never silently diverge.
        //
        // Detect rejection by event-count growth rather than by the tail id: a
        // message may legitimately share an id with a prior accepted event (e.g.
        // the same message id appended more than once), which would make a
        // last-event-id comparison falsely report success.
        let recorded = self.event_map.events.len() > events_before;
        
        // Keep backward compatibility
        self.memory_profile_cache.messages_count += 1;
        self.memory_profile_cache.messages_json_bytes += estimate_json_bytes(&message);
        self.memory_profile_cache
            .message_stats
            .merge_from(&summarize_blocks(&message.content));
        self.messages.push(message);
        self.mark_messages_append_dirty();
        // The event log grew with the new message. `append_stored_message`
        // updates the cache's message counts in-place but it must also reflect
        // the appended event, so mark the memory profile dirty to force an exact
        // rebuild (including event_log_count/event_log_json_bytes). Keeping the
        // in-place updates is harmless; the dirty flag guarantees the cache is
        // never observed with a stale event-log count after appends.
        self.mark_memory_profile_dirty();
        if !recorded {
            self.rebuild_event_map();
        }
    }

    pub fn insert_message(&mut self, index: usize, message: StoredMessage) {
        // Append to event log. Use a unique event id rather than one derived
        // from the index: inserting at the same index twice (e.g. repeated
        // tool-output repair) would otherwise collide and be skipped by
        // validation.
        let message_id = crate::id::new_id("insert");
        let event = SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: message_id.clone(),
            op: SessionEventOp::InsertMessage { index, message: message.clone() },
            parent_id: None,
            version: 1,
        };
        let events_before = self.event_map.events.len();
        self.event_map.append_event(event);
        // If validation rejected the event, the legacy insert below still
        // applies; rebuild so the log agrees with the legacy vector. Use
        // event-count growth (not tail-id compare) for robustness.
        let recorded = self.event_map.events.len() > events_before;
        
        // Keep backward compatibility. Clamp the index to the live length to
        // match `derive_messages` (which clamps `InsertMessage` to `len`): an
        // out-of-range index must not panic the legacy vector while replay
        // tolerates it. `index == len` is a valid end-append.
        let idx = index.min(self.messages.len());
        self.messages.insert(idx, message);
        self.mark_memory_profile_dirty();
        self.mark_messages_full_dirty();
        if !recorded {
            self.rebuild_event_map();
        }
    }

    pub fn replace_messages(&mut self, messages: Vec<StoredMessage>) {
        // Append to event log (replace all).
        //
        // `end_index` uses usize::MAX rather than the current length so that
        // replay is deterministic: a full replacement must cover the entire
        // derived transcript regardless of where it sits in the event stream
        // (e.g. after a prior truncate shortened the tail). `derive_messages`
        // caps `end_index` at the live length, so usize::MAX always means
        // "to the end".
        let event = SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: crate::id::new_id("replace_all"),
            op: SessionEventOp::ReplaceMessages {
                start_index: 0,
                end_index: usize::MAX,
                messages: messages.clone(),
            },
            parent_id: None,
            version: 1,
        };
        self.event_map.append_event(event);
        
        // Keep backward compatibility
        self.messages = messages;
        self.mark_memory_profile_dirty();
        self.mark_messages_full_dirty();
    }

    pub fn truncate_messages(&mut self, len: usize) {
        // Truncating to zero messages is a full clear. Emit `ClearAll` rather
        // than a `ReplaceMessages` with an empty prefix: a `ReplaceMessages`
        // with `start == end` cannot clear an already-empty transcript during
        // replay, so the event log would otherwise desync from `self.messages`.
        if len == 0 {
            if !self.messages.is_empty() {
                self.clear_messages();
            }
            return;
        }
        if len < self.messages.len() {
            // Append to event log. Truncating to `len` keeps the first `len`
            // messages and drops the tail. This is a *splice-out* of the span
            // `[len..]`, so the event is `ReplaceMessages { start: len,
            // end: usize::MAX, messages: vec![] }` — NOT `{ start: 0, end: len,
            // messages: prefix }`, which would splice `[0..len]` back with the
            // same prefix and leave the tail `[len..]` intact (a no-op that
            // desyncs the derived log from the truncated legacy vector).
            let event = SessionEvent {
                timestamp: chrono::Utc::now(),
                event_id: crate::id::new_id("truncate"),
                op: SessionEventOp::ReplaceMessages {
                    start_index: len,
                    end_index: usize::MAX,
                    messages: Vec::new(),
                },
                parent_id: None,
                version: 1,
            };
            self.event_map.append_event(event);
            
            self.messages.truncate(len);
            self.mark_memory_profile_dirty();
            self.mark_messages_full_dirty();
        }
    }

    /// Clear every message in the transcript.
    ///
    /// Emits a `ClearAll` event so replay deterministically yields an empty
    /// transcript regardless of preceding message/replace/truncate events.
    /// Unlike `truncate_messages(0)`, this does not leave a stale snapshot of
    /// the prefix; the event log records the intent instead.
    pub fn clear_messages(&mut self) {
        let event = SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: crate::id::new_id("clear_all"),
            op: SessionEventOp::ClearAll,
            parent_id: None,
            version: 1,
        };
        self.event_map.append_event(event);
        self.messages.clear();
        // Also drop any persisted compaction state, since it refers to
        // messages that no longer exist.
        self.compaction = None;
        self.mark_memory_profile_dirty();
        self.mark_messages_full_dirty();
    }

    /// Drop oversized inline images from the stored transcript, oldest-first,
    /// until the total remaining base64 image payload fits within
    /// `target_total_chars`. Used to recover from provider HTTP 413
    /// "request too large" errors, which are driven by base64 image payload size
    /// rather than the token context window.
    ///
    /// Mutates and persists the authoritative transcript (replacing each dropped
    /// image with a short text marker) and invalidates the provider-message
    /// cache so the next API call reflects the reduced payload. Returns the
    /// number of images that were stripped.
    pub fn strip_oversized_images(&mut self, target_total_chars: usize) -> usize {
        let mut contents: Vec<&mut Vec<ContentBlock>> =
            self.messages.iter_mut().map(|m| &mut m.content).collect();
        let stripped = jcode_compaction_core::strip_large_images_in_contents(
            &mut contents,
            target_total_chars,
        );
        if stripped > 0 {
            self.mark_memory_profile_dirty();
            self.mark_messages_full_dirty();
            self.record_transcript_replacement();
        }
        stripped
    }

    /// Shorten oversized tool-result text in the stored transcript, oldest-first,
    /// until the total remaining tool-result payload fits within
    /// `target_total_chars`. Used to recover from provider HTTP 413
    /// "request too large" errors that are driven by accumulated large tool
    /// outputs (e.g. file/cat/read results) rather than inline images.
    ///
    /// Unlike image stripping (which drops whole image blocks), this keeps the
    /// head and tail of each oversized tool result so the model retains the
    /// beginning and end of the content. Mutates and persists the authoritative
    /// transcript and invalidates the provider-message cache. Returns the number
    /// of tool results that were truncated.
    pub fn emergency_truncate_tool_results(&mut self, target_total_chars: usize) -> usize {
        let mut contents: Vec<&mut Vec<ContentBlock>> =
            self.messages.iter_mut().map(|m| &mut m.content).collect();
        let truncated =
            jcode_compaction_core::emergency_truncate_tool_results_in_contents(
                &mut contents,
                target_total_chars,
            );
        if truncated > 0 {
            self.mark_memory_profile_dirty();
            self.mark_messages_full_dirty();
            self.record_transcript_replacement();
        }
        truncated
    }

    pub fn visible_conversation_message_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|message| is_visible_conversation_message(message))
            .count()
    }

    pub fn visible_conversation_messages(&self) -> Vec<&StoredMessage> {
        self.messages
            .iter()
            .filter(|message| is_visible_conversation_message(message))
            .collect()
    }

    pub fn stored_len_for_visible_conversation_message(
        &self,
        visible_index: usize,
    ) -> Option<usize> {
        if visible_index == 0 {
            return None;
        }

        let mut count = 0usize;
        for (stored_index, message) in self.messages.iter().enumerate() {
            if is_visible_conversation_message(message) {
                count += 1;
                if count == visible_index {
                    return Some(stored_index + 1);
                }
            }
        }
        None
    }

    /// Stored-message indices of the rewind targets shown in the TUI's
    /// numbered `/rewind` list, in display order.
    ///
    /// The TUI numbers user/assistant *transcript entries* (what the user
    /// actually sees), not raw stored messages. Stored tool-result messages
    /// and tool-call-only assistant messages render as tool cards or nothing,
    /// so counting raw stored messages diverges wildly from the on-screen
    /// numbering in tool-heavy sessions (issue #432). Deriving targets from
    /// the same rendering used for the transcript keeps `/rewind N` aligned
    /// with the numbers `/rewind` prints.
    ///
    /// A single stored message can produce multiple transcript entries (text
    /// split around a tool result); each entry keeps its own number and maps
    /// to the same stored index so numbering matches the visible list exactly.
    pub fn rewind_target_stored_indices(&self) -> Vec<usize> {
        render_messages(self)
            .into_iter()
            .filter(|message| matches!(message.role.as_str(), "user" | "assistant"))
            .filter_map(|message| message.stored_index)
            .collect()
    }

    /// Number of `/rewind` targets (see [`Self::rewind_target_stored_indices`]).
    pub fn rewind_target_count(&self) -> usize {
        self.rewind_target_stored_indices().len()
    }

    /// Record a memory injection event for replay visualization
    pub fn record_memory_injection(
        &mut self,
        summary: String,
        content: String,
        count: u32,
        age_ms: u64,
        memory_ids: Vec<String>,
    ) {
        let injection = StoredMemoryInjection {
            summary,
            content,
            count,
            memory_ids,
            age_ms: Some(age_ms),
            before_message: Some(self.messages.len()),
            timestamp: Utc::now(),
        };
        
        // Append to event log
        let event = SessionEvent {
            timestamp: injection.timestamp,
            event_id: crate::id::new_id("mem_inj"),
            op: SessionEventOp::MemoryInjection {
                memory_injection: injection.clone(),
            },
            parent_id: None,
            version: 1,
        };
        let events_before = self.event_map.events.len();
        self.event_map.append_event(event);
        // Only publish the legacy vector when the event was actually recorded.
        // `append_event` silently skips an invalid memory injection (e.g. empty
        // content), which would otherwise leave `self.memory_injections`
        // diverging from the derived log.
        let recorded = self.event_map.events.len() > events_before;
        if recorded {
            // Keep backward compatibility
            self.memory_profile_cache.memory_injections_count += 1;
            self.memory_profile_cache.memory_injections_json_bytes += estimate_json_bytes(&injection);
            self.memory_injections.push(injection);
            self.mark_memory_injections_append_dirty();
            // The event log grew with the new event. The in-place injection
            // count is already updated, but the profile's `event_log_count` /
            // `event_log_json_bytes` fields (derived from `event_map.events`)
            // must reflect the appended event too. Mark the profile dirty so an
            // exact rebuild happens, mirroring `append_stored_message`.
            self.mark_memory_profile_dirty();
        }
    }

    pub fn injected_memory_ids(&self) -> Vec<String> {
        let mut ids = HashSet::new();
        for injection in &self.memory_injections {
            ids.extend(injection.memory_ids.iter().cloned());
        }
        ids.into_iter().collect()
    }

    pub fn record_replay_display_message(
        &mut self,
        role: impl Into<String>,
        title: Option<String>,
        content: impl Into<String>,
    ) {
        let event_data = StoredReplayEvent {
            timestamp: Utc::now(),
            kind: StoredReplayEventKind::DisplayMessage {
                role: role.into(),
                title,
                content: content.into(),
            },
        };
        self.record_replay_event(&event_data);
    }

    /// Record an already-constructed replay event into the event log.
    pub fn record_replay_event(&mut self, replay_event: &StoredReplayEvent) {
        let event = SessionEvent {
            timestamp: replay_event.timestamp,
            event_id: crate::id::new_id("replay"),
            op: SessionEventOp::ReplayEvent {
                replay_event: replay_event.clone(),
            },
            parent_id: None,
            version: 1,
        };
        let events_before = self.event_map.events.len();
        self.event_map.append_event(event);
        // Only publish the legacy vector when the event was actually recorded.
        // `append_event` silently skips an invalid replay event, which would
        // otherwise leave `self.replay_events` diverging from the derived log.
        let recorded = self.event_map.events.len() > events_before;
        if recorded {
            self.memory_profile_cache.replay_events_count += 1;
            self.memory_profile_cache.replay_events_json_bytes += estimate_json_bytes(replay_event);
            self.replay_events.push(replay_event.clone());
            self.mark_replay_events_append_dirty();
            // The event log grew with the new event; keep the profile's
            // `event_log_count`/`event_log_json_bytes` in sync (see
            // `append_stored_message`).
            self.mark_memory_profile_dirty();
        }
    }

    /// Get current messages from event log (derives pure state)
    pub fn derive_messages(&self) -> Vec<StoredMessage> {
        self.event_map.derive_messages()
    }

    /// Get current compaction from event log (derives pure state)
    pub fn derive_compaction(&self) -> Option<StoredCompactionState> {
        self.event_map.current_compaction()
    }

    /// Get memory injections from event log (derives pure state)
    pub fn derive_memory_injections(&self) -> Vec<StoredMemoryInjection> {
        self.event_map.memory_injections()
    }

    /// Get replay events from event log (derives pure state)
    pub fn derive_replay_events(&self) -> Vec<StoredReplayEvent> {
        self.event_map.replay_events()
    }

    /// Append a session event through the validated event log.
    ///
    /// Callers outside `jcode-base` should use this instead of reaching into
    /// `event_map` so the append path stays consistent. Returns `false` if the
    /// event was rejected by validation (and therefore not recorded).
    ///
    /// This appends to the *event log only*. It does **not** update the legacy
    /// surface vectors (`messages`, `compaction`, `memory_injections`,
    /// `replay_events`), so a caller that appends a state-carrying event
    /// (`AppendMessage`/`InsertMessage`/`SetCompaction`/`ReplayEvent`/
    /// `MemoryInjection`) must update the corresponding legacy field itself (or
    /// call the dedicated `Session` method that keeps them in sync), otherwise
    /// the two sources of truth diverge. For log-only events (`ClearAll`,
    /// compaction brackets, plugin `Unknown`) no legacy update is needed.
    pub fn append_session_event(&mut self, event: SessionEvent) -> bool {
        // Capture whether the event was recorded before mutating (append_event
        // skips invalid events internally).
        let before = self.event_map.events.len();
        // Capture the op before `append_event` moves it (needed for the
        // debug-only self-check below). Only computed in debug builds so release
        // does not pay the clone.
        #[cfg(debug_assertions)]
        let op_kind = event.op.clone();
        self.event_map.append_event(event);
        let recorded = self.event_map.events.len() > before;
        if recorded {
            // The event log grew by one event; keep the profile's
            // `event_log_count`/`event_log_json_bytes` in sync (see
            // `append_stored_message`). Cheap flag-only; the profile rebuilds
            // lazily on the next snapshot.
            self.mark_memory_profile_dirty();
            // Debug-only self-check for the dual-source-of-truth contract. The
            // API warns that a state-carrying op (`AppendMessage`/`InsertMessage`/
            // `ReplaceMessages`/`ClearAll`) requires the caller to keep the legacy
            // `messages` vector in sync. Verify that contract here so a forgetful
            // caller is caught in dev instead of silently desyncing the two
            // sources (which would otherwise force a divergent-load rebuild that
            // drops log-only markers). Log-only events (`Unknown`, brackets,
            // `SetCompaction`, `ReplayEvent`, `MemoryInjection`) do not count into
            // `messages`, so they never trip the check.
            #[cfg(debug_assertions)]
            {
                let message_ops = matches!(
                    op_kind,
                    SessionEventOp::AppendMessage { .. }
                        | SessionEventOp::InsertMessage { .. }
                        | SessionEventOp::ReplaceMessages { .. }
                        | SessionEventOp::ClearAll
                );
                if message_ops {
                    // Compare only lengths to keep the check cheap and avoid
                    // requiring `PartialEq` (which `StoredMessage` deliberately
                    // lacks). A caller that synced the legacy vector exactly (or
                    // used the dedicated Session methods) matches here.
                    let derived = self.event_map.derive_messages().len();
                    assert!(
                        derived == self.messages.len(),
                        "session event-log/legacy desync after append_session_event: \
                         log derives {derived} messages but session.messages has {}",
                        self.messages.len()
                    );
                }
            }
        }
        recorded
    }

    /// Read-only view of the append-only event log.
    ///
    /// This is the public inspection seam for the event-sourced log: plugins
    /// (and callers outside `jcode-base`) can enumerate committed events —
    /// including their own `Unknown` escape-hatch events — without reaching into
    /// the crate-private `event_map`. The log is append-only; use the
    /// mutation API (`append_session_event`, `set_compaction`,
    /// `compact_transcript_with_bracket`, ...) to change it.
    pub fn event_log(&self) -> &[SessionEvent] {
        &self.event_map.events
    }

    /// Fork the event log up to a boundary and return the prefix as a new map.
    pub fn fork_event_log(&self, boundary_index: usize) -> SessionEventMap {
        self.event_map.fork_up_to_boundary(boundary_index)
    }

    /// Set compaction state in event log
    pub fn set_compaction(&mut self, compaction: StoredCompactionState) {
        let event = SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: crate::id::new_id("set_compaction"),
            op: SessionEventOp::SetCompaction {
                compaction: compaction.clone(),
            },
            parent_id: None,
            version: 1,
        };
        let events_before = self.event_map.events.len();
        self.event_map.append_event(event);
        let recorded = self.event_map.events.len() > events_before;
        if recorded {
            // Only publish the legacy vector when the event was actually recorded.
            // `append_event` silently skips an invalid compaction (e.g.
            // `covers_up_to_turn`/`compacted_count` exceeding
            // `original_turn_count`), which would otherwise leave `self.compaction`
            // diverging from the derived log and fail `rederive_all_checked`.
            self.compaction = Some(compaction);
            self.mark_messages_append_dirty();
            // The event log grew with the new SetCompaction event; keep the
            // profile's `event_log_count`/`event_log_json_bytes` in sync (see
            // `append_stored_message`).
            self.mark_memory_profile_dirty();
        }
    }

    /// Complete a log-bracketed compaction (deepseek-harness takeaway #5).
    ///
    /// Compaction is a **log-bracketed, replayable operation**: a run appends a
    /// `CompactionStart` marker, replaces the old transcript span with the
    /// summary, and closes with `CompactionEnd`. A crash between `Start` and
    /// `End` leaves a *detectable orphaned lock* (see
    /// [`SessionEventMap::orphaned_compaction`]) rather than a half-applied
    /// summary, so replay can surface an explicit "incomplete compaction" state
    /// instead of silently trusting a partial surface mutation.
    ///
    /// This is the producer seam producers should call instead of reaching into
    /// `set_compaction` / `replace_messages` separately. It emits the balanced
    /// bracket around a single surface mutation and keeps the legacy
    /// `self.compaction` vector in sync with what the log derives.
    ///
    /// If the `CompactionStart` is rejected by validation (e.g. a
    /// `covers_up_to_turn` of zero), no bracket is opened — the method falls
    /// back to a plain `SetCompaction` (via `set_compaction`) so the compaction
    /// still persists in the log without a dangling `CompactionEnd` that would
    /// violate the `CompactionBracket` invariant.
    ///
    /// If the `compaction` state itself is invalid (e.g. `covers_up_to_turn`
    /// exceeding `original_turn_count`), it cannot be represented as any event,
    /// so no bracket (and no `SetCompaction`) is written at all — the method
    /// applies only the message replacement and leaves `self.compaction` unset,
    /// rather than leaving an orphaned bracket that could never be closed.
    ///
    /// Returns the compaction id of the (new) bracket, so a caller can correlate
    /// a later crash-recovery report back to the span being summarized. When an
    /// orphaned bracket is already open (a prior run crashed mid-bracket) the
    /// orphan is completed rather than re-opened, so the returned id is the
    /// retry's, not the original crashed run's. Callers should snap `messages`
    /// (and `covers_up_to_turn`) to tool-pairing boundaries so no open
    /// tool/call pair crosses the cut (the `ToolPairingBalanced` invariant
    /// enforces this on replay).
    pub fn compact_transcript_with_bracket(
        &mut self,
        compaction_id: impl Into<String>,
        messages: Vec<StoredMessage>,
        compaction: StoredCompactionState,
        covers_up_to_turn: usize,
    ) -> String {
        let compaction_id = compaction_id.into();
        // Pre-validate the compaction state before opening a bracket. If it's
        // invalid (e.g. `covers_up_to_turn`/`compacted_count` exceeding
        // `original_turn_count`), we cannot represent it in ANY event — both a
        // `CompactionEnd` and a `SetCompaction` would be rejected by `append_event`
        // validation — so opening a `CompactionStart` would leave an orphaned
        // bracket (append-only log) we could never close with a valid End. Degrade
        // to a message-only replacement and leave `self.compaction` unset instead
        // of creating a malformed bracket.
        if SessionEventMap::validate_compaction(&compaction).is_err() {
            self.replace_messages(messages);
            return compaction_id;
        }
        // Crash-safety against a *retried* compaction: if a previous run crashed
        // mid-bracket (an orphaned `CompactionStart` is still open), do not
        // start a second, nested bracket — that would create a depth-2 malformed
        // bracket the invariant flags as silent corruption. Instead, complete the
        // in-flight bracket: the surface mutation and `CompactionEnd` below close
        // the orphan, so retry is idempotent with respect to the bracket shape.
        let started = if self.event_map.orphaned_compaction().is_none() {
            let before = self.event_map.events.len();
            self.event_map
                .start_compaction(compaction_id.clone(), covers_up_to_turn);
            // `append_event` silently skips an invalid event (e.g. a
            // `CompactionStart` with `covers_up_to_turn == 0` fails validation).
            // Detect that by event-count growth so we never close a bracket whose
            // opening marker was dropped — that would emit a dangling
            // `CompactionEnd` and violate the `CompactionBracket` invariant.
            self.event_map.events.len() > before
        } else {
            // Completing an existing orphaned bracket; the opening marker is
            // already in the log, so closing it is valid.
            true
        };
        // One surface mutation inside the bracket: replace the transcript with
        // the summarized tail. This is the only change replay needs to apply.
        self.replace_messages(messages);
        if started {
            self.event_map.end_compaction(compaction.clone());
            self.compaction = Some(compaction);
            // Enforce the compaction-bracket invariant at the completion point.
            // This is a SAFE, deliberately narrow call site for `enforce()`: at
            // the moment a balanced bracket has just been closed and persisted,
            // an open/duplicated bracket is provably a bug — a compaction that
            // claims completion while leaving the bracket malformed. Enforce ONLY
            // `CompactionBracket` here (not the full builtin registry) because
            // `ToolPairingBalanced` legitimately fires on compactions that do not
            // snap to a tool-pairing boundary (a valid, documented call pattern),
            // so enforcing it here would hard-fail a correct run.
            let mut bracket_registry = invariants::InvariantRegistry::default();
            bracket_registry.add(invariants::CompactionBracket);
            let log = bracket_registry.check(&self.event_map);
            // `enforce` panics in debug builds on a violation; release logs.
            invariants::InvariantLog::enforce(&log, "compact_transcript_with_bracket");
        } else {
            // The `CompactionStart` was rejected by validation (e.g. an invalid
            // `covers_up_to_turn`), so no bracket is open. Persist the compaction
            // as a plain `SetCompaction` instead — this keeps `derive_compaction`
            // in agreement with `self.compaction` without creating a malformed
            // bracket (no dangling `CompactionEnd`).
            self.set_compaction(compaction);
        }
        compaction_id
    }

    /// Fork session up to a boundary (returns new session with prefix of events)
    pub fn fork_up_to_boundary(&self, boundary_index: usize) -> Self {
        let mut fork = self.clone();
        
        // Create fork with prefix of events
        fork.event_map = self.fork_event_log(boundary_index);
        
        // Reset the derived fields for the fork
        fork.messages = fork.derive_messages();
        fork.compaction = fork.derive_compaction();
        fork.memory_injections = fork.derive_memory_injections();
        fork.replay_events = fork.derive_replay_events();
        
        // The parent's provider message cache reflects the full (longer)
        // transcript; the fork is truncated, so any cached provider messages and
        // prefix hashes are stale. Reset them so the next call to
        // provider_messages()/messages_for_provider() recomputes from the fork's
        // truncated transcript rather than returning the parent's cache.
        fork.reset_provider_messages_cache();
        // The cloned `memory_profile_cache` still describes the PARENT's (larger)
        // transcript, injections and replay events, while the fork's derived
        // vectors are truncated to the boundary. Mark the profile dirty so the
        // next ensure_memory_profile_cache/debug_memory_profile rebuilds from the
        // fork's actual state instead of reporting the parent's counts.
        fork.mark_memory_profile_dirty();
        
        // Generate new ID for the fork
        fork.id = new_id("fork");
        fork.updated_at = chrono::Utc::now();

        // The fork cloned the parent's persist_state, which is wrong for a new id:
        // snapshot_exists and the vector/event lengths describe the PARENT's
        // on-disk snapshot, not this fork (which has no snapshot file, and whose
        // event log is truncated to the boundary). Reset so the fork's first save
        // is forced to write its own snapshot (snapshot_exists=false); otherwise a
        // fork that keeps the full event log would journal-append with no snapshot
        // and become unloadable under its own id.
        fork.reset_persist_state(false);

        fork
    }

    /// Re-derive all state from event log (pure computation)
    pub fn rederive_all(&self) -> (Vec<StoredMessage>, Option<StoredCompactionState>) {
        let messages = self.derive_messages();
        let compaction = self.derive_compaction();
        (messages, compaction)
    }

    /// Re-derive all state and validate internal consistency.
    ///
    /// The event-sourced migration relies on `event_map` being the single
    /// source of truth. This diagnostic checks that the state derived from the
    /// event log matches the legacy vectors (`messages`, `compaction`), and
    /// that compaction turn bounds are internally sane. It never mutates the
    /// session.
    pub fn rederive_all_checked(&self) -> Result<(Vec<StoredMessage>, Option<StoredCompactionState>), String> {
        let (messages, compaction) = self.rederive_all();

        // The event log must agree with the legacy transcript vector.
        if messages.len() != self.messages.len() {
            return Err(format!(
                "event_map derived {} messages but session.messages has {} (hydration mismatch)",
                messages.len(),
                self.messages.len()
            ));
        }
        for (i, (derived, legacy)) in messages.iter().zip(self.messages.iter()).enumerate() {
            if derived.id != legacy.id {
                return Err(format!(
                    "event_map message[{}] id mismatch: derived={}, legacy={}",
                    i, derived.id, legacy.id
                ));
            }
            if derived.content.len() != legacy.content.len() {
                return Err(format!(
                    "event_map message[{}] content block count mismatch: derived={}, legacy={}",
                    i, derived.content.len(), legacy.content.len()
                ));
            }
        }

        // Compaction must also agree.
        if compaction != self.compaction {
            return Err(format!(
                "event_map compaction mismatch: derived={:?}, legacy={:?}",
                compaction, self.compaction
            ));
        }

        if let Some(comp) = &compaction {
            // covers_up_to_turn must not exceed the original turn count.
            if comp.covers_up_to_turn > comp.original_turn_count {
                return Err(format!(
                    "compaction covers_up_to_turn ({}) exceeds original_turn_count ({})",
                    comp.covers_up_to_turn,
                    comp.original_turn_count
                ));
            }
            if comp.compacted_count > comp.original_turn_count {
                return Err(format!(
                    "compaction compacted_count ({}) exceeds original_turn_count ({})",
                    comp.compacted_count,
                    comp.original_turn_count
                ));
            }
        }

        // Memory injections must also agree (by count; the type lacks PartialEq).
        let derived_inj = self.derive_memory_injections();
        if derived_inj.len() != self.memory_injections.len() {
            return Err(format!(
                "event_map derived {} memory injections but session.memory_injections has {}",
                derived_inj.len(),
                self.memory_injections.len()
            ));
        }

        // Replay events must also agree (StoredReplayEvent derives PartialEq).
        let derived_replay = self.derive_replay_events();
        if derived_replay != self.replay_events {
            return Err(format!(
                "event_map replay events diverge from session.replay_events (derived {} vs {} legacy)",
                derived_replay.len(),
                self.replay_events.len()
            ));
        }

        Ok((messages, compaction))
    }

    /// Reconcile the event log after loading a session from disk.
    ///
    /// `event_map` is now persisted in the snapshot, so after `load_from_path`
    /// the log may already be populated (the authoritative append-only record
    /// from a prior save). This method makes the loaded session's `event_map`
    /// consistent with the legacy vectors regardless of what the snapshot had:
    ///
    /// - If the persisted log is empty but the legacy vectors are populated,
    ///   rebuild it (migration from sessions written before `event_map` was
    ///   persisted).
    /// - If the persisted log is non-empty *and* rederiving from it exactly
    ///   matches the legacy vectors (`rederive_all_checked`), keep it — it is
    ///   the authoritative record, including compaction brackets and plugin
    ///   (`Unknown`) events that a rebuild-from-vectors cannot reproduce.
    /// - Otherwise (persisted log stale or missing journal-appended events),
    ///   rebuild from the vectors so the two sources of truth never diverge.
    ///
    /// Returns `true` when the authoritative persisted log was kept, `false`
    /// when it was rebuilt from the legacy vectors.
    pub fn reconcile_event_map_after_load(&mut self) -> bool {
        if self.event_map.is_empty() {
            if !self.messages.is_empty()
                || !self.memory_injections.is_empty()
                || !self.replay_events.is_empty()
                || self.compaction.is_some()
            {
                self.rebuild_event_map();
            }
            return false;
        }
        match self.rederive_all_checked() {
            Ok(_) => true,
            Err(_) => {
                self.rebuild_event_map();
                false
            }
        }
    }

    /// Rebuild the event log from the legacy session vectors.
    ///
    /// `event_map` is now persisted in the snapshot, so after loading a
    /// session from disk it is normally already populated as the authoritative
    /// append-only record. This method discards that record and rebuilds the
    /// log from the legacy `messages`, `compaction`, `memory_injections`, and
    /// `replay_events` vectors. Prefer `reconcile_event_map_after_load` for the
    /// load path, which keeps the persisted log when it agrees with the
    /// vectors and only rebuilds when they diverge or the log is empty
    /// (migration of pre-persistence snapshots).
    ///
    /// This is *not* idempotent-by-guard: it unconditionally rebuilds the log
    /// from the authoritative legacy vectors. Forking a session sets
    /// `compaction`/`memory_injections`/`replay_events` directly without
    /// emitting matching events, so a guard that bails on a non-empty log would
    /// leave those fields missing from the derived event state. Rebuilding from
    /// the vectors guarantees every call yields a log that agrees with the
    /// legacy state, regardless of prior content.
    pub fn rebuild_event_map(&mut self) {
        let mut map = SessionEventMap::default();
        let now = chrono::Utc::now();

        // Plugin `Unknown` events are purely additive: they affect no derive
        // path (messages, compaction, memory injections, replay events), so a
        // rebuild must PRESERVE them rather than drop them. Otherwise any caller
        // that rebuilds the log from the legacy vectors (reconcile-on-divergence,
        // the app-core sanitize-clear path) would silently lose durable plugin
        // data, defeating the escape hatch (takeaway #13).
        //
        // Orphaned `CompactionStart` markers are likewise preserved: an unmatched
        // open bracket is the durable "incomplete compaction" signal (takeaway
        // #5), and a divergent-load rebuild would otherwise erase it. Because an
        // open `Start` affects neither `derive_messages` nor
        // `current_compaction` (only a matching `CompactionEnd` or `SetCompaction`
        // persists compaction), re-appending them keeps the rebuilt log deriving
        // exactly the same state as the legacy vectors while retaining the orphan
        // signal. Closed bracket pairs and ordinary state-carrying events are NOT
        // preserved here: they are reconstructed (or deliberately omitted) from
        // the legacy vectors so the rebuilt log agrees with
        // `self.compaction`/`self.messages`.
        let preserved_unknown: Vec<SessionEvent> = self
            .event_map
            .events
            .iter()
            .filter(|e| matches!(e.op, SessionEventOp::Unknown { .. }))
            .cloned()
            .collect();
        // Capture orphans BEFORE the map is replaced below.
        let preserved_orphan_starts: Vec<SessionEvent> = self
            .event_map
            .orphaned_compactions()
            .into_iter()
            .cloned()
            .collect();

        for (i, message) in self.messages.iter().enumerate() {
            map.push_event(SessionEvent {
                timestamp: message.timestamp.unwrap_or(now),
                event_id: format!("rehydrate_{}", i),
                op: SessionEventOp::AppendMessage {
                    message_id: message.id.clone(),
                    message: message.clone(),
                },
                parent_id: None,
                version: 1,
            });
        }

        for (j, injection) in self.memory_injections.iter().enumerate() {
            map.push_event(SessionEvent {
                timestamp: injection.timestamp,
                event_id: format!("rehydrate_mem_{}", j),
                op: SessionEventOp::MemoryInjection {
                    memory_injection: injection.clone(),
                },
                parent_id: None,
                version: 1,
            });
        }

        for (k, replay) in self.replay_events.iter().enumerate() {
            map.push_event(SessionEvent {
                timestamp: replay.timestamp,
                event_id: format!("rehydrate_replay_{}", k),
                op: SessionEventOp::ReplayEvent {
                    replay_event: replay.clone(),
                },
                parent_id: None,
                version: 1,
            });
        }

        if let Some(compaction) = &self.compaction {
            map.push_event(SessionEvent {
                timestamp: now,
                event_id: "rehydrate_compaction".to_string(),
                op: SessionEventOp::SetCompaction {
                    compaction: compaction.clone(),
                },
                parent_id: None,
                version: 1,
            });
        }

        // Re-append the preserved log-only events: plugin `Unknown` events and
        // orphaned `CompactionStart` markers. They are appended LAST so their
        // relative order is unchanged and they sit after any reconstructed state
        // events (their position in the log never matters to derivation, but
        // keeping them contiguous at the tail preserves the append-only narrative
        // for any downstream reader).
        for event in preserved_orphan_starts {
            map.push_event(event);
        }
        for event in preserved_unknown {
            map.push_event(event);
        }

        self.event_map = map;
        // The log was reconstructed, not appended to — a tail-delta journal
        // entry could not capture it, so force a full snapshot on the next save.
        self.mark_events_full_dirty();
    }

    pub fn record_swarm_status_event(&mut self, members: Vec<crate::protocol::SwarmMemberStatus>) {
        let kind = StoredReplayEventKind::SwarmStatus { members };
        if self
            .replay_events
            .last()
            .is_some_and(|last| last.kind == kind)
        {
            return;
        }
        let event = StoredReplayEvent {
            timestamp: Utc::now(),
            kind,
        };
        // Route through record_replay_event so the swarm status is captured in
        // the event log (derive_replay_events is authoritative in-process).
        self.record_replay_event(&event);
    }

    pub fn record_swarm_plan_event(
        &mut self,
        swarm_id: String,
        version: u64,
        items: Vec<crate::plan::PlanItem>,
        participants: Vec<String>,
        reason: Option<String>,
    ) {
        let kind = StoredReplayEventKind::SwarmPlan {
            swarm_id,
            version,
            items,
            participants,
            reason,
        };
        if self
            .replay_events
            .last()
            .is_some_and(|last| last.kind == kind)
        {
            return;
        }
        let event = StoredReplayEvent {
            timestamp: Utc::now(),
            kind,
        };
        // Route through record_replay_event so the swarm plan is captured in
        // the event log (derive_replay_events is authoritative in-process).
        self.record_replay_event(&event);
    }

    pub fn provider_messages(&mut self) -> &[Message] {
        let needs_full_rebuild = self.provider_messages_cache_mode == PersistVectorMode::Full
            || self.provider_messages_cache_len > self.messages.len();

        if needs_full_rebuild {
            self.provider_messages_cache.clear();
            self.provider_message_prefix_hashes_cache.clear();
            self.provider_messages_cache.reserve(self.messages.len());
            self.provider_message_prefix_hashes_cache
                .reserve(self.messages.len());
            for index in 0..self.messages.len() {
                let message = self.messages[index].to_message();
                self.push_provider_message_cache_entry(message);
            }
            self.provider_messages_cache_len = self.messages.len();
            self.provider_messages_cache_mode = PersistVectorMode::Clean;
            return &self.provider_messages_cache;
        }

        if self.provider_messages_cache_mode == PersistVectorMode::Append
            && self.provider_messages_cache_len < self.messages.len()
        {
            let appended_len = self.messages.len() - self.provider_messages_cache_len;
            self.provider_messages_cache.reserve(appended_len);
            self.provider_message_prefix_hashes_cache
                .reserve(appended_len);
            for index in self.provider_messages_cache_len..self.messages.len() {
                let message = self.messages[index].to_message();
                self.push_provider_message_cache_entry(message);
            }
            self.provider_messages_cache_len = self.messages.len();
            self.provider_messages_cache_mode = PersistVectorMode::Clean;
        }

        &self.provider_messages_cache
    }

    pub fn provider_message_prefix_hashes(&mut self) -> &[u64] {
        let _ = self.provider_messages();
        &self.provider_message_prefix_hashes_cache
    }

    pub fn messages_for_provider_uncached(&self) -> Vec<Message> {
        stored_messages_to_messages(&self.messages)
    }

    pub fn messages_for_provider(&mut self) -> Vec<Message> {
        self.provider_messages().to_vec()
    }

    /// Drop heavyweight transcript vectors after remote startup has rendered the
    /// optimistic local history. The authoritative transcript comes from the
    /// server once the connection is established, so keeping another owned copy
    /// in the client only inflates memory during idle remote sessions.
    pub fn strip_transcript_for_remote_client(&mut self) {
        // Emit ClearAll so the event log reflects the strip and replay is consistent.
        // (The messages are already emitted as events during load; this ClearAll
        // ensures the log agrees with the cleared legacy vectors.)
        if !self.messages.is_empty() {
            self.clear_messages();
        }
        // Even when `self.messages` was already empty, drop any compaction state:
        // a compacted remote transcript rendered locally must not leave stale
        // compaction behind in the surviving event log (rebuild_event_map below
        // would otherwise re-record a SetCompaction event).
        self.compaction = None;
        self.env_snapshots.clear();
        self.memory_injections.clear();
        self.replay_events.clear();
        // `clear_messages()` only reconciles the messages/compaction event state.
        // The memory-injection and replay-event vectors were cleared directly
        // above, so rebuild the whole log from the now-empty legacy state to keep
        // the derived views (derive_memory_injections/derive_replay_events) in sync.
        self.rebuild_event_map();
        self.rebuild_memory_profile_cache();
        self.reset_provider_messages_cache();
        self.reset_persist_state(true);
    }

    /// Remove all ToolUse content blocks from a specific message.
    /// Used when tool calls are discarded (e.g. due to truncated output / max_tokens).
    pub fn remove_tool_use_blocks(&mut self, message_id: &str) {
        let mut removed = false;
        for msg in &mut self.messages {
            if msg.id == *message_id {
                let before = msg.content.len();
                msg.content
                    .retain(|block| !matches!(block, ContentBlock::ToolUse { .. }));
                removed = before != msg.content.len();
                if removed {
                    self.mark_memory_profile_dirty();
                    self.mark_messages_full_dirty();
                }
                break;
            }
        }
        // Only emit a replacement event if a block was actually removed, so we
        // do not pollute the log when the message has no ToolUse blocks to drop.
        if removed {
            self.record_transcript_replacement();
        }
    }
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            *s = crate::message::redact_secrets(s);
        }
        serde_json::Value::Array(values) => {
            for entry in values {
                redact_json_value(entry);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if is_sensitive_json_key(key) {
                    *entry = serde_json::Value::String("[REDACTED_SECRET]".to_string());
                } else {
                    redact_json_value(entry);
                }
            }
        }
        _ => {}
    }
}

fn is_sensitive_json_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    normalized.contains("apikey")
        || normalized.ends_with("token")
        || normalized.ends_with("secret")
        || normalized.contains("password")
        || matches!(
            normalized.as_str(),
            "authorization" | "cookie" | "setcookie" | "privatekey" | "clientsecret"
        )
}

#[derive(Debug, Deserialize)]
struct RemoteStartupSessionSnapshot {
    id: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    custom_title: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    messages: Vec<StoredMessage>,
    #[serde(default)]
    compaction: Option<StoredCompactionState>,
    #[serde(default)]
    provider_session_id: Option<String>,
    #[serde(default)]
    provider_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    route_api_method: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    subagent_model: Option<String>,
    #[serde(default)]
    improve_mode: Option<SessionImproveMode>,
    #[serde(default)]
    autoreview_enabled: Option<bool>,
    #[serde(default)]
    autojudge_enabled: Option<bool>,
    #[serde(default)]
    is_canary: bool,
    #[serde(default)]
    testing_build: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    short_name: Option<String>,
    #[serde(default)]
    status: SessionStatus,
    #[serde(default)]
    last_pid: Option<u32>,
    #[serde(default)]
    last_active_at: Option<DateTime<Utc>>,
    #[serde(default)]
    is_debug: bool,
    #[serde(default)]
    saved: bool,
    #[serde(default)]
    save_label: Option<String>,
}

#[cfg(test)]
#[path = "session_tests/mod.rs"]
mod tests;
