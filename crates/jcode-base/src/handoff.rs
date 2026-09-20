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
use std::collections::{HashMap, HashSet};
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

/// Default maximum number of archived snapshot files kept per project on disk,
/// beyond the single live handoff the index retains. Older snapshots beyond
/// this are pruned. The `MAX_INDEX_ENTRIES` cap bounds the index; this bounds
/// the archived *files* that `list_all_handoffs` surfaces, so the picker does
/// not grow without bound as sessions accumulate.
const MAX_ARCHIVED_SNAPSHOTS_PER_PROJECT: usize = 16;

/// Default maximum age of archived snapshot files, in days. Snapshots older
/// than this are pruned even when they are under the per-project count cap.
const MAX_ARCHIVED_SNAPSHOT_AGE_DAYS: i64 = 30;

/// Compute a portable project identity from a working directory.
///
/// Prefers the git remote URL (stable across clones/machines) and falls back to
/// the absolute working directory. Returns `None` when neither yields anything
/// meaningful.
///
/// Resolve the remote on each call: a long-lived server can observe a checkout
/// being initialized or its origin changing while it is running.
pub fn project_key(working_dir: Option<&Path>) -> Option<String> {
    let dir = working_dir?;
    let absolute = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(dir)
    };
    let canonical = absolute.canonicalize().unwrap_or(absolute);
    Some(match git_remote_url(&canonical) {
        Some(url) => format!("git:{}", url),
        None => format!("path:{}", canonical.display()),
    })
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

    let last_assistant_text = transcript_for_extraction
        .and_then(extract_last_assistant_text)
        .map(|text| truncate(&text, 4096));

    Some(HandoffSnapshot {
        session_id: session_id.to_string(),
        project_key,
        ended_at: Utc::now(),
        disposition: disposition.to_string(),
        working_dir: working_dir.map(|p| p.display().to_string()),
        intent: plan.user_intention,
        open_todos,
        last_assistant_text,
        initiative_id: load_attached_initiative(session_id, working_dir),
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
    // Serialize capture including its read of todo state, not merely the rename.
    let _lock = match lock_store() {
        Ok(lock) => lock,
        Err(error) => {
            crate::logging::warn(&format!("[handoff] cannot lock store: {error}"));
            return None;
        }
    };
    let Some(snapshot) = build_snapshot(
        session_id,
        working_dir,
        disposition,
        transcript_for_extraction,
    ) else {
        // A read failure is not evidence of completed work. Only retire a
        // snapshot when the persisted todo list was successfully read.
        if let Ok(todos) = load_todos(session_id)
            && todos
                .iter()
                .all(|t| t.status == "completed" || t.status == "cancelled")
            && let Err(err) = retire_session(session_id)
        {
            crate::logging::warn(&format!(
                "[handoff] failed to retire {}: {}",
                session_id, err
            ));
        }
        return None;
    };
    match write_snapshot_locked(&snapshot) {
        Ok(()) => {
            prune_archived_snapshots();
            Some(snapshot)
        }
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
#[cfg(test)]
fn write_snapshot(snapshot: &HandoffSnapshot) -> Result<()> {
    let _lock = lock_store()?;
    write_snapshot_locked(snapshot)
}

fn lock_store() -> Result<std::fs::File> {
    let dir = handoffs_dir()?;
    crate::storage::ensure_dir(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(".lock"))?;
    file.lock()?;
    Ok(file)
}

fn retire_session(session_id: &str) -> Result<()> {
    let mut index = load_index();
    index.latest.retain(|entry| entry.session_id != session_id);
    crate::storage::write_json_fast(&index_path()?, &index)?;
    // Keep the archived snapshot available for explicit promotion, but it is
    // no longer eligible for automatic injection.
    Ok(())
}

fn write_snapshot_locked(snapshot: &HandoffSnapshot) -> Result<()> {
    if load_snapshot(&snapshot.session_id).is_some_and(|old| old.ended_at > snapshot.ended_at) {
        return Ok(());
    }
    let dir = handoffs_dir()?;
    crate::storage::write_json_fast(&file_path(&dir, &snapshot.session_id)?, snapshot)?;
    upsert_index(snapshot)
}

/// Path to a single per-session handoff file.
fn file_path(dir: &Path, session_id: &str) -> Result<PathBuf> {
    // Reject rather than replace: lossy sanitization aliases unrelated sessions.
    // ASCII validation also avoids case-folding/Unicode normalization aliases.
    anyhow::ensure!(
        !session_id.is_empty()
            && !session_id.eq_ignore_ascii_case("index")
            && session_id.len() <= 200
            && session_id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_'),
        "invalid handoff session id"
    );
    Ok(dir.join(format!("{}.json", session_id)))
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
    // A session may move between projects. Its single snapshot file no longer
    // represents the old project's entry.
    index
        .latest
        .retain(|e| e.session_id != snapshot.session_id || e.project_key == snapshot.project_key);
    if index
        .latest
        .iter()
        .any(|e| e.project_key == snapshot.project_key && e.ended_at > snapshot.ended_at)
    {
        crate::storage::write_json_fast(&index_path()?, &index)?;
        return Ok(());
    }
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
    index
        .latest
        .retain(|e| e.project_key != snapshot.project_key);
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
    crate::storage::read_json::<HandoffSnapshot>(&path)
        .ok()
        .filter(|snapshot| snapshot.session_id == session_id)
}

/// The most recent handoff session id for a project directory, if one exists.
pub fn latest_handoff_for_project(working_dir: Option<&Path>) -> Option<String> {
    let key = project_key(working_dir)?;
    let index = load_index();
    index
        .latest
        .into_iter()
        .find(|e| e.project_key == key)
        .map(|e| e.session_id)
}

/// The most recent handoff for a working dir as a compact markdown block, for
/// first-message injection at session start. Returns `None` when there is
/// nothing to show.
pub fn render_boot_context(working_dir: Option<&Path>) -> Option<String> {
    let key = project_key(working_dir)?;
    let session_id = load_index()
        .latest
        .into_iter()
        .find(|e| e.project_key == key)?
        .session_id;
    let snapshot = load_snapshot(&session_id)?;
    if snapshot.project_key != key {
        return None;
    }
    render_snapshot(&snapshot)
}

/// List the saved handoffs available for manual selection, one per project.
///
/// Returns the latest unfinished handoff per project from the index, newest
/// first. These are the rows the `/handoff` picker offers, keyed by
/// `session_id`, so manual resume can override automatic latest-for-project
/// injection without regressing it.
pub fn list_saved_handoffs() -> Vec<IndexEntry> {
    let mut entries: Vec<IndexEntry> = load_index().latest;
    entries.sort_by(|a, b| b.ended_at.cmp(&a.ended_at));
    entries
}

/// List every persisted handoff snapshot, including archived ones that are no
/// longer the latest for their project (and so absent from the index).
///
/// Unlike [`list_saved_handoffs`], this scans the snapshot directory directly so
/// an older snapshot that was superseded by a newer handoff in the same project
/// is still discoverable for manual selection. Returns newest first. Snapshot
/// files that fail to load (corrupt or identity-mismatched) are skipped; the
/// directory-scoped scan is bounded by the handoffs directory size.
pub fn list_all_handoffs() -> Vec<HandoffSnapshot> {
    let Ok(dir) = handoffs_dir() else {
        return Vec::new();
    };
    let mut snapshots = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return snapshots;
    };
    let mut seen = HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        // `index.json` is the index, not a snapshot; `.lock` is not json.
        if file_name == "index.json" {
            continue;
        }
        let session_id = file_name
            .strip_suffix(".json")
            .unwrap_or_default()
            .to_string();
        if session_id.is_empty() || !seen.insert(session_id.clone()) {
            continue;
        }
        if let Some(snapshot) = load_snapshot(&session_id) {
            snapshots.push(snapshot);
        }
    }
    snapshots.sort_by(|a, b| b.ended_at.cmp(&a.ended_at));
    snapshots
}

/// Serialize a saved handoff snapshot as an opaque portable payload.
///
/// This is the transfer half of the remote-adoption flow: a snapshot exported
/// on one host can be shipped (transcript of the handoff store, SSH, etc.) to
/// another host and re-adopted there with [`import_handoff`]. Fails with
/// `None` when the session id is unknown.
pub fn export_handoff(session_id: &str) -> Option<String> {
    let snapshot = load_snapshot(session_id)?;
    serde_json::to_string(&snapshot).ok()
}

/// Adopt a portable handoff payload into this host's store.
///
/// The remote-adoption counterpart to [`export_handoff`]: parses a payload
/// formerly produced there, rekeys it to the current working directory's
/// project (so it is found by this host's automatic first-message injection
/// and the `/handoff` picker), writes it as a snapshot file, and *explicitly
/// registers it with the project in the index*. Unlike a blanket on-disk scan,
/// adoption is a deliberate act: the imported snapshot is intentionally live,
/// so it cannot silently resurrect a retired handoff.
///
/// When the target project already has a *newer* local handoff, that one stays
/// as the project's latest entry (upsert keeps the newer timestamp) and the
/// import is retained as an archived, still-manually-selectable snapshot. Only
/// when the imported snapshot is the newest for the project does it become the
/// automatic latest-for-project pick.
///
/// Returns the adopted session id (a fresh UUID that does not collide with an
/// existing snapshot), or `None` when the payload is malformed or the working
/// directory has no resolvable project key.
pub fn import_handoff(
    payload: &str,
    working_dir: Option<&Path>,
    disposition: &str,
) -> Option<String> {
    let project = project_key(working_dir)?;
    let mut snapshot: HandoffSnapshot = serde_json::from_str(payload).ok()?;
    // Re-key to this host's project identity so lookup and injection find it.
    snapshot.project_key = project.clone();
    snapshot.disposition = disposition.to_string();

    let _lock = lock_store().ok()?;
    let dir = handoffs_dir().ok()?;
    // A fresh session id avoids overwriting a local snapshot for the same id.
    let session_id = format!("import-{}", uuid::Uuid::new_v4());
    snapshot.session_id = session_id.clone();
    crate::storage::write_json_fast(&file_path(&dir, &session_id).ok()?, &snapshot).ok()?;
    upsert_index(&snapshot).ok()?;
    Some(session_id)
}

/// Prune archived handoff snapshot files so the store does not grow without
/// bound as sessions accumulate.
///
/// The index already caps live per-project entries at `MAX_INDEX_ENTRIES`, but
/// the archived snapshot files it drops still persist on disk and are surfaced
/// by [`list_all_handoffs`]. This enforces a retention policy over those
/// files:
///
/// - Snapshots beyond `MAX_ARCHIVED_SNAPSHOTS_PER_PROJECT` for a single project
///   are removed (oldest first), keeping the picker's per-project archive
///   bounded.
/// - Snapshots older than `MAX_ARCHIVED_SNAPSHOT_AGE_DAYS` are removed
///   regardless of count.
///
/// Live handoffs — the latest per project, as held by the index — are never
/// pruned; only superseded archived snapshots are eligible. This runs after a
/// successful [`capture`] write. Failures to read or delete individual files
/// are ignored (best-effort), and a missing or unreadable store is a no-op.
pub fn prune_archived_snapshots() {
    let Ok(index) = load_index_opt() else {
        return;
    };
    // Live handoffs: the latest per project recorded by the index.
    let mut live: HashMap<String, String> = HashMap::new();
    for entry in &index.latest {
        live
            .entry(entry.project_key.clone())
            .or_insert_with(|| entry.session_id.clone());
    }

    let Ok(dir) = handoffs_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    // Collect candidate snapshots grouped by project, excluding live handoffs.
    let mut by_project: HashMap<String, Vec<HandoffSnapshot>> = HashMap::new();
    let mut seen = HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if file_name == "index.json" {
            continue;
        }
        let session_id = file_name
            .strip_suffix(".json")
            .unwrap_or_default()
            .to_string();
        if session_id.is_empty() || !seen.insert(session_id.clone()) {
            continue;
        }
        if live.values().any(|id| id == &session_id) {
            continue;
        }
        if let Some(snapshot) = load_snapshot(&session_id) {
            by_project
                .entry(snapshot.project_key.clone())
                .or_default()
                .push(snapshot);
        }
    }

    let now = Utc::now();
    let age_limit = chrono::Duration::days(MAX_ARCHIVED_SNAPSHOT_AGE_DAYS);
    for snapshots in by_project.values_mut() {
        // Oldest first so we trim the least-recently-finished first.
        snapshots.sort_by(|a, b| a.ended_at.cmp(&b.ended_at));
        for (offset, snapshot) in snapshots.iter().enumerate() {
            let too_old = now - snapshot.ended_at > age_limit;
            let over_cap = snapshots.len() - offset > MAX_ARCHIVED_SNAPSHOTS_PER_PROJECT;
            if !too_old && !over_cap {
                break;
            }
            delete_snapshot(&snapshot.session_id);
        }
    }
}

/// Load the handoff index, surfacing IO/path failures as `Err` so callers can
/// detect that pruning has nothing to operate on. Returns `Ok(empty)` for a
/// missing or corrupt index, matching [`load_index`]'s resilience.
fn load_index_opt() -> Result<HandoffIndex> {
    let path = index_path()?;
    if !path.exists() {
        return Ok(HandoffIndex::default());
    }
    Ok(crate::storage::read_json::<HandoffIndex>(&path).unwrap_or_default())
}

/// Best-effort delete of a snapshot file by session id. Returns `true` when a
/// file was removed, `false` when nothing existed or deletion failed.
fn delete_snapshot(session_id: &str) -> bool {
    let Ok(dir) = handoffs_dir() else {
        return false;
    };
    let Ok(path) = file_path(&dir, session_id) else {
        return false;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(_err) => false,
    }
}

/// Render a specific handoff snapshot by id as a compact markdown block, for
/// a manually selected resume. Returns `None` when no such snapshot exists or
/// its identity check fails.
pub fn render_handoff(session_id: &str) -> Option<String> {
    let snapshot = load_snapshot(session_id)?;
    render_snapshot(&snapshot)
}

/// Render a snapshot as a compact markdown block for first-message injection.
///
/// Shared by automatic latest-for-project injection ([`render_boot_context`])
/// and manual selection ([`render_handoff`]) so the rendered shape is identical
/// and bounded regardless of how the handoff was chosen.
fn render_snapshot(snapshot: &HandoffSnapshot) -> Option<String> {
    let mut out = String::from("[Handoff from previous session]");
    if let Some(intent) = &snapshot.intent {
        out.push_str(&format!("\nIntent: {}", truncate(intent, 2048)));
    }
    if !snapshot.open_todos.is_empty() {
        out.push_str("\nOpen work:");
        for t in snapshot.open_todos.iter().take(32) {
            out.push_str(&format!(
                "\n- [{}] {}",
                truncate(&t.status, 32),
                truncate(&t.content, 512)
            ));
        }
        if snapshot.open_todos.len() > 32 {
            out.push_str("\n[Additional work omitted; see the saved handoff.]");
        }
    }
    if let Some(text) = &snapshot.last_assistant_text {
        out.push_str(&format!(
            "\nLast assistant message: {}",
            truncate(text, 200)
        ));
    }
    if let Some(id) = &snapshot.initiative_id {
        out.push_str(&format!("\nLinked initiative: {}", truncate(id, 200)));
    }
    if out.len() > 8192 {
        out = truncate(&out, 8100);
        out.push_str("\n[Handoff truncated; see the saved handoff.]");
    }
    Some(out)
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
    crate::goal::update_goal(
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
    )?;
    Ok(Some(goal.id))
}

/// Get the git remote `origin` URL for a directory, if any, using local git
/// configuration. Called during capture and first-message lookup, not later turns.
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
fn load_attached_initiative(session_id: &str, working_dir: Option<&Path>) -> Option<String> {
    crate::goal::load_attached_goal(session_id, working_dir)
        .ok()
        .flatten()
        .map(|g| g.id)
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod tests;
