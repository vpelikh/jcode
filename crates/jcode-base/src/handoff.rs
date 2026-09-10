//! Automatic per-session handoff capture.
//!
//! When a session with unfinished (open) todos ends, [`capture`] writes a small
//! structured snapshot of "where I stopped" that a later session in the same
//! project can boot from — without needing to re-read the whole transcript or
//! have the user re-explain the context.
//!
//! Design decisions:
//!
//! - **Mechanical, not LLM.** The snapshot is assembled from state that already
//!   exists (the `todo` plan + list + session context). It deliberately does not
//!   summarize the whole transcript; that is the memory extractor's heavier job.
//! - **Only when there is open work.** A session with no non-terminal todos has
//!   nothing to hand off and would only create noise.
//! - **Keyed by portable project identity.** The handoff must survive a change
//!   of working-dir path (e.g. a different checkout directory or a remote
//!   handoff). We key by the git remote URL when available, falling back to the
//!   absolute working dir, so the same project lands in one bucket across
//!   machines.
//!
//! Handoffs are *transient per-session scratch*, distinct from curated
//! `initiative` goals ([`crate::goal`]). A handoff can be promoted into a goal
//! once it proves durable, but the two stores are intentionally separate.

use crate::todo::{TodoItem, load_plan, load_todos};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A single session's handoff snapshot, written on session close.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffSnapshot {
    pub session_id: String,
    /// Portable identity for the project (git remote URL or absolute working dir).
    pub project_key: String,
    pub ended_at: DateTime<Utc>,
    /// Why the session ended: "closed", "crashed", or "reloading".
    pub disposition: String,
    pub working_dir: Option<String>,
    /// The user's intent from the todo plan (`TodoPlan.user_intention`).
    pub intent: Option<String>,
    /// Live work items at close, in order.
    pub open_todos: Vec<HandoffTodo>,
    /// Tail text of the last assistant message, if any.
    pub last_assistant_text: Option<String>,
    /// Durable initiative linked to this work, if one was attached.
    pub initiative_id: Option<String>,
}

/// A compact view of an open todo for a handoff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffTodo {
    pub id: String,
    pub content: String,
    pub status: String,
    pub group: Option<String>,
    pub confidence: Option<String>,
}

/// The index of most-recent handoffs per project, used at boot and by the
/// picker. Only the latest unfinished handoff per project is kept hot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HandoffIndex {
    /// project_key -> latest handoff info.
    pub latest: Vec<IndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub project_key: String,
    pub session_id: String,
    pub ended_at: DateTime<Utc>,
    pub summary: Option<String>,
}

const MAX_INDEX_ENTRIES: usize = 64;

/// Compute a portable project identity from a working directory.
///
/// Prefers the git remote URL (stable across clones/machines) and falls back to
/// the absolute working directory. Returns `None` when neither yields anything
/// meaningful.
pub fn project_key(working_dir: Option<&Path>) -> Option<String> {
    let dir = working_dir?;
    // The git remote URL is the most portable identity across machines.
    if let Some(url) = git_remote_url(dir) {
        return Some(format!("git:{}", url));
    }
    // Fall back to the absolute path (only stable if the same path is reused).
    let abs = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    Some(format!("path:{}", abs.display()))
}

/// Build a handoff snapshot for a closing session, sans writing to disk.
///
/// Returns `None` when there is no open work worth handing off, i.e. every
/// stored todo is `completed`/`cancelled` (or there are no todos at all).
pub fn build_snapshot(
    session_id: &str,
    working_dir: Option<&Path>,
    disposition: &str,
    transcript_for_extraction: Option<&str>,
) -> Option<HandoffSnapshot> {
    let plan = load_plan(session_id).unwrap_or_default();
    let todos = load_todos(session_id).unwrap_or_default();
    let open: Vec<TodoItem> = todos
        .into_iter()
        .filter(|t| t.status != "completed" && t.status != "cancelled")
        .collect();
    if open.is_empty() {
        return None;
    }

    let project_key = project_key(working_dir)?;

    let open_todos = open
        .into_iter()
        .map(|t| HandoffTodo {
            id: t.id,
            content: t.content,
            status: t.status,
            group: t.group,
            confidence: t.confidence.as_ref().map(|c| c.as_str().to_string()),
        })
        .collect();

    let last_assistant_text =
        transcript_for_extraction.and_then(extract_last_assistant_text);

    Some(HandoffSnapshot {
        session_id: session_id.to_string(),
        project_key,
        ended_at: Utc::now(),
        disposition: disposition.to_string(),
        working_dir: working_dir.map(|p| p.display().to_string()),
        intent: plan.user_intention,
        open_todos,
        last_assistant_text,
        initiative_id: load_attached_initiative(session_id),
    })
}

/// Persist a handoff snapshot for a closing session.
///
/// Writes a per-session file and bumps a per-project index. Returns `None` when
/// there is nothing to hand off, or logs and returns `None` on write failure.
pub fn capture(
    session_id: &str,
    working_dir: Option<&Path>,
    disposition: &str,
    transcript_for_extraction: Option<&str>,
) -> Option<HandoffSnapshot> {
    let snapshot = build_snapshot(session_id, working_dir, disposition, transcript_for_extraction)?;
    match write_snapshot(&snapshot) {
        Ok(()) => Some(snapshot),
        Err(err) => {
            crate::logging::warn(&format!(
                "[handoff] failed to persist handoff session={} error={}",
                session_id, err
            ));
            None
        }
    }
}

/// Write the snapshot file and update the per-project index.
fn write_snapshot(snapshot: &HandoffSnapshot) -> Result<()> {
    let dir = handoffs_dir()?;
    crate::storage::write_json_fast(&file_path(&dir, &snapshot.session_id)?, snapshot)?;
    upsert_index(snapshot)
}

/// Path to a single per-session handoff file.
fn file_path(_dir: &Path, session_id: &str) -> Result<PathBuf> {
    // The caller passes `handoffs_dir`; sanitize the session id before use.
    let safe = session_id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>();
    Ok(_dir.join(format!("{}.json", safe)))
}

fn handoffs_dir() -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("handoffs"))
}

fn index_path() -> Result<PathBuf> {
    Ok(handoffs_dir()?.join("index.json"))
}

/// Load the handoff index, returning an empty index if missing or corrupt.
fn load_index() -> HandoffIndex {
    let Ok(path) = index_path() else {
        return HandoffIndex::default();
    };
    if !path.exists() {
        return HandoffIndex::default();
    }
    crate::storage::read_json::<HandoffIndex>(&path).unwrap_or_default()
}

/// Record `snapshot` as the latest handoff for its project.
fn upsert_index(snapshot: &HandoffSnapshot) -> Result<()> {
    let mut index = load_index();
    let entry = IndexEntry {
        project_key: snapshot.project_key.clone(),
        session_id: snapshot.session_id.clone(),
        ended_at: snapshot.ended_at,
        summary: Some(
            snapshot
                .intent
                .clone()
                .unwrap_or_else(|| "<no intent>".to_string()),
        ),
    };
    index.latest.retain(|e| e.project_key != snapshot.project_key);
    index.latest.push(entry);
    if index.latest.len() > MAX_INDEX_ENTRIES {
        index.latest.sort_by(|a, b| b.ended_at.cmp(&a.ended_at));
        index.latest.truncate(MAX_INDEX_ENTRIES);
    }
    let path = index_path()?;
    crate::storage::write_json_fast(&path, &index)?;
    Ok(())
}

/// Load a named handoff snapshot by its session id.
pub fn load_snapshot(session_id: &str) -> Option<HandoffSnapshot> {
    let dir = handoffs_dir().ok()?;
    let path = file_path(&dir, session_id).ok()?;
    if !path.exists() {
        return None;
    }
    crate::storage::read_json::<HandoffSnapshot>(&path).ok()
}

/// The most recent handoff session id for a project directory, if one exists.
pub fn latest_handoff_for_project(working_dir: Option<&Path>) -> Option<String> {
    let key = project_key(working_dir)?;
    let index = load_index();
    index.latest.into_iter().find(|e| e.project_key == key).map(|e| e.session_id)
}

/// The most recent handoff for a working dir as a compact markdown block, for
/// system-prompt injection at session start. Returns `None` when there is
/// nothing to show.
pub fn render_boot_context(working_dir: Option<&Path>) -> Option<String> {
    let session_id = latest_handoff_for_project(working_dir)?;
    let snapshot = load_snapshot(&session_id)?;
    let mut out = String::from("[Handoff from previous session]");
    if let Some(intent) = &snapshot.intent {
        out.push_str(&format!("\nIntent: {}", intent));
    }
    if !snapshot.open_todos.is_empty() {
        out.push_str("\nOpen work:");
        for t in &snapshot.open_todos {
            out.push_str(&format!("\n- [{}] {}", t.status, t.content));
        }
    }
    if let Some(text) = &snapshot.last_assistant_text {
        out.push_str(&format!("\nLast assistant message: {}", truncate(text, 200)));
    }
    if let Some(id) = &snapshot.initiative_id {
        out.push_str(&format!("\nLinked initiative: {}", id));
    }
    Some(out)
}

/// Decide whether to inject a handoff into the system prompt at session start.
///
/// Mirrors the gate used by `Agent::build_system_prompt_split`: a handoff is
/// injected only at the very start of a fresh conversation (no visible
/// messages yet), so an already-running session does not re-announce it every
/// turn. Exposed as a pure function so the injection decision is testable in
/// isolation without constructing an `Agent`.
pub fn should_inject(fresh_conversation: bool, working_dir: Option<&Path>) -> bool {
    fresh_conversation && render_boot_context(working_dir).is_some()
}

/// Promote a handoff snapshot into a durable project-scoped `initiative` goal.
///
/// This is the bridge from transient handoff scratch to the curated `goals/`
/// store: once a handoff proves durable across sessions (a big plan the user
/// intends to track), it graduates into an initiative. The snapshot's intent
/// becomes the goal title/description, open todos become initial next steps,
/// and the snapshot is recorded as a first checkpoint.
///
/// Returns the created goal's id, or an error if creation fails.
pub fn promote_to_initiative(
    session_id: &str,
    working_dir: Option<&Path>,
) -> anyhow::Result<Option<String>> {
    let Some(snapshot) = load_snapshot(session_id) else {
        return Ok(None);
    };
    let title = snapshot
        .intent
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("Continue from session {}", session_id));
    let next_steps: Vec<String> = snapshot
        .open_todos
        .iter()
        .map(|t| t.content.clone())
        .collect();
    let description = snapshot
        .last_assistant_text
        .clone()
        .filter(|s| !s.trim().is_empty());
    let goal = crate::goal::create_goal(
        crate::goal::GoalCreateInput {
            id: None,
            title: title.clone(),
            scope: crate::goal::GoalScope::Project,
            description,
            why: Some(format!(
                "Promoted from handoff of session {} ({})",
                snapshot.session_id, snapshot.ended_at
            )),
            success_criteria: Vec::new(),
            milestones: Vec::new(),
            next_steps,
            blockers: Vec::new(),
            current_milestone_id: None,
            progress_percent: Some(0),
        },
        working_dir,
    )?;
    // Record the handoff itself as the opening checkpoint.
    let _ = crate::goal::update_goal(
        &goal.id,
        Some(crate::goal::GoalScope::Project),
        working_dir,
        crate::goal::GoalUpdateInput {
            checkpoint_summary: Some(format!(
                "Adopted handoff from session {} with {} open item(s).",
                snapshot.session_id,
                snapshot.open_todos.len()
            )),
            ..Default::default()
        },
    );
    Ok(Some(goal.id))
}

/// Get the git remote `origin` URL for a directory, if any. Shells out to git,
/// which is acceptable here: capture runs once per session close.
fn git_remote_url(dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!url.is_empty()).then_some(url)
}

/// Truncate a string to a byte cap, splitting on a char boundary.
fn truncate(s: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if out.len() + ch.len_utf8() > max_bytes {
            break;
        }
        out.push(ch);
    }
    out
}

/// Extract the last assistant text block from a transcript-for-extraction.
/// The transcript format is `**Assistant:**\n<text>` blocks.
fn extract_last_assistant_text(transcript: &str) -> Option<String> {
    let mut last: Option<String> = None;
    let mut current_block: String = String::new();
    let mut in_assistant = false;
    for line in transcript.lines() {
        if line == "**Assistant:**" {
            // Commit any previous block then start a new one.
            let candidate = std::mem::take(&mut current_block);
            if in_assistant && !candidate.trim().is_empty() {
                last = Some(candidate);
            }
            in_assistant = true;
            current_block = String::new();
        } else if line == "**User:**" {
            let candidate = std::mem::take(&mut current_block);
            if in_assistant && !candidate.trim().is_empty() {
                last = Some(candidate);
            }
            in_assistant = false;
        } else if in_assistant && line.starts_with("[Used tool:") {
            let candidate = std::mem::take(&mut current_block);
            if !candidate.trim().is_empty() {
                last = Some(candidate);
            }
        } else if in_assistant && !line.is_empty() {
            if !current_block.is_empty() {
                current_block.push('\n');
            }
            current_block.push_str(line.trim_end());
        }
    }
    if in_assistant && !current_block.trim().is_empty() {
        last = Some(current_block);
    }
    last.filter(|s| !s.trim().is_empty())
}

/// Load the initiative id attached to a session, if any.
fn load_attached_initiative(session_id: &str) -> Option<String> {
    crate::goal::load_attached_goal(session_id, None)
        .ok()
        .flatten()
        .map(|g| g.id)
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod tests;