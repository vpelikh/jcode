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
//! - **Only when there is something to hand off.** A snapshot is written only
//!   when the session has open (non-terminal) todos or an explicit continuation
//!   task; otherwise it would only create noise.
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
    /// Why the snapshot was written: "closed", "crashed", or "reloading" for a
    /// disconnect capture, or "saved" for an explicit on-demand save.
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
    /// An explicit continuation task/prompt captured with the handoff (e.g.
    /// "review this branch's changes"). Rendered prominently in the boot
    /// context so the resumed session boots *ready to do this task*, not just
    /// aware of the old context. Set by an explicit save; a disconnect capture
    /// carries a previously saved prompt forward rather than dropping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_prompt: Option<String>,
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
    build_snapshot_with_prompt(
        session_id,
        working_dir,
        disposition,
        transcript_for_extraction,
        None,
    )
}

/// Build a handoff snapshot that may carry an explicit continuation prompt.
///
/// When `continuation_prompt` is a non-empty task, a snapshot is produced even
/// when there is no open todo work: the whole point of the save is to hand the
/// *task* forward ("review this branch's changes"), which is itself the work.
/// Without a prompt, an empty todo list yields `None` exactly as `build_snapshot`
/// does.
fn build_snapshot_with_prompt(
    session_id: &str,
    working_dir: Option<&Path>,
    disposition: &str,
    transcript_for_extraction: Option<&str>,
    continuation_prompt: Option<&str>,
) -> Option<HandoffSnapshot> {
    let plan = load_plan(session_id).unwrap_or_default();
    let todos = load_todos(session_id).unwrap_or_default();
    let open: Vec<TodoItem> = todos
        .into_iter()
        .filter(|t| t.status != "completed" && t.status != "cancelled")
        .collect();
    let continuation_prompt = continuation_prompt
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(normalize_continuation_prompt);
    if open.is_empty() && continuation_prompt.is_none() {
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
        continuation_prompt,
    })
}

/// Persist a handoff snapshot for a closing session.
///
/// Writes a per-session file and bumps a per-project index. Returns `None` when
/// there is nothing to hand off, or logs and returns `None` on write failure.
///
/// A continuation prompt previously saved for this session (via
/// [`save_now_with_prompt`] or the `handoff` tool) is carried forward: it names
/// what the next session should do, so this rebuild neither drops it nor retires
/// a prompt-only handoff as if the work were complete.
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
    // A continuation prompt set by an explicit save (`/handoffsave <task>` or the
    // `handoff` tool) is a user instruction with a lifetime longer than the
    // session: it names what the *next* session should do. Read it before
    // building so this disconnect capture neither drops it nor mistakes a
    // prompt-only handoff (no open todos) for completed work.
    let previous_prompt = load_snapshot(session_id).and_then(|s| s.continuation_prompt);
    let Some(snapshot) = build_snapshot_with_prompt(
        session_id,
        working_dir,
        disposition,
        transcript_for_extraction,
        previous_prompt.as_deref(),
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
            // Already holding the store lock from the top of `capture`, so run
            // the unlocked body directly (avoiding a self-deadlock).
            prune_archived_snapshots_locked();
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

/// Explicitly persist a handoff snapshot for a live session, on demand.
///
/// This is the manual counterpart to [`capture`]: it runs while the session is
/// still open (via the `/handoffsave` command) instead of only at disconnect, so
/// the user can checkpoint "where I am" without ending the session. The snapshot
/// is recorded with `disposition == "saved"`.
///
/// Unlike [`capture`], a session with no open work is **not** retired: a manual
/// save is purely additive and must never clear an existing index entry. Returns
/// `None` when there is nothing to save (no open todos and no prompt, new or
/// previously saved) or when the project identity cannot be resolved. A prompt
/// already saved for this session is carried forward (see
/// [`save_now_with_prompt`]).
pub fn save_now(
    session_id: &str,
    working_dir: Option<&Path>,
    transcript_for_extraction: Option<&str>,
) -> Option<HandoffSnapshot> {
    save_now_with_prompt(session_id, working_dir, transcript_for_extraction, None)
}

/// Explicitly persist a handoff snapshot carrying an optional continuation
/// prompt/task for the session that resumes it.
///
/// Like [`save_now`], the snapshot is recorded with `disposition == "saved"` and
/// is purely additive. A non-empty `continuation_prompt` ("review this branch's
/// changes") is stored on the snapshot and rendered at the top of the boot
/// context, so a later session in the same project boots *ready to perform that
/// task*. When a prompt is present a snapshot is written even for a session with
/// no open todos, because the task itself is the work being handed off.
///
/// A save without a prompt does not clear an existing one for the same session:
/// it carries the previously saved task forward, so a bare `/handoffsave` (after
/// setting a task earlier) never silently loses it.
pub fn save_now_with_prompt(
    session_id: &str,
    working_dir: Option<&Path>,
    transcript_for_extraction: Option<&str>,
    continuation_prompt: Option<&str>,
) -> Option<HandoffSnapshot> {
    // Serialize the read of todo state and the index write, as `capture` does.
    let _lock = match lock_store() {
        Ok(lock) => lock,
        Err(error) => {
            crate::logging::warn(&format!("[handoff] cannot lock store for save: {error}"));
            return None;
        }
    };
    let incoming = continuation_prompt
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let carried_prompt = if incoming.is_some() {
        None
    } else {
        load_snapshot(session_id).and_then(|s| s.continuation_prompt)
    };
    let prompt = incoming.or(carried_prompt.as_deref());
    let snapshot = build_snapshot_with_prompt(
        session_id,
        working_dir,
        "saved",
        transcript_for_extraction,
        prompt,
    )?;
    match write_snapshot_locked(&snapshot) {
        Ok(()) => {
            // Already holding the store lock, so run the unlocked body directly.
            prune_archived_snapshots_locked();
            Some(snapshot)
        }
        Err(err) => {
            crate::logging::warn(&format!(
                "[handoff] failed to save handoff session={} error={}",
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
    let (_, snapshot) = latest_snapshot_for_project(working_dir)?;
    render_snapshot(&snapshot)
}

/// Resolve the latest handoff snapshot (and its session id) for a project, if
/// any.
///
/// Reads `project_key` -> latest index entry -> snapshot and validates project
/// identity, without consuming anything (the handoff stays eligible for
/// automatic injection). Returns `None` when there is no handoff, it fails the
/// identity check, or the snapshot cannot be loaded.
fn latest_snapshot_for_project(
    working_dir: Option<&Path>,
) -> Option<(String, HandoffSnapshot)> {
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
    Some((session_id, snapshot))
}

/// The most recent handoff for a working dir as a compact markdown block for
/// first-message injection, **and consume it** so it does not re-inject on a
/// later session in the same project.
///
/// Automatic first-message injection is a one-shot: after the handoff is
/// rendered for a fresh conversation, it must no longer be the project's
/// "latest unfinished handoff", otherwise every new session in the project
/// would re-surface the same stale snapshot. This retires the resolved entry
/// from the index once it has been rendered, while keeping the archived
/// snapshot available for explicit promotion via `/handoffres`.
pub fn render_boot_context_and_consume(working_dir: Option<&Path>) -> Option<String> {
    // Resolve + render first, without locking: a lock failure must not prevent
    // the fresh conversation from booting with its handoff context, matching
    // `render_boot_context` (read-only, lock-free). Consuming (retiring) is a
    // best-effort extra that only happens when we hold the store lock.
    let (session_id, snapshot) = latest_snapshot_for_project(working_dir)?;
    let rendered = render_snapshot(&snapshot)?;
    // Take the store lock so the retire is atomic with a concurrent writer
    // (`capture`, `import_handoff`, `prune_archived_snapshots`), all of which
    // take the lock for index writes. A lock failure is logged and the handoff
    // is simply left in place (it may surface once more on the next boot)
    // rather than silently dropping the rendered context.
    match lock_store() {
        Ok(_lock) => {
            if let Err(err) = retire_session(&session_id) {
                crate::logging::warn(&format!(
                    "[handoff] failed to consume auto-injected {}: {}",
                    session_id, err
                ));
            }
        }
        Err(error) => {
            crate::logging::warn(&format!(
                "[handoff] cannot lock store to consume auto-injected {}: {}",
                session_id, error
            ));
        }
    }
    Some(rendered)
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
/// Returns the adopted session id (a readable `import-<source>` that stays
/// recognizable for `/handoffres`, with a short disambiguating suffix only on
/// collision), or `None` when the payload is malformed or the working
/// directory has no resolvable project key.
pub fn import_handoff(
    payload: &str,
    working_dir: Option<&Path>,
    disposition: &str,
) -> Option<String> {
    let project = project_key(working_dir)?;
    let mut snapshot: HandoffSnapshot = serde_json::from_str(payload).ok()?;
    // Normalize an incoming task to this host's cap so a foreign payload can
    // never inject an over-cap (or whitespace-only) prompt into the boot context.
    snapshot.continuation_prompt = snapshot
        .continuation_prompt
        .map(|p| normalize_continuation_prompt(&p))
        .filter(|p| !p.trim().is_empty());
    // A handoff is only meaningful when it carries unfinished work *or* an
    // explicit continuation task. Reject a payload with neither, matching
    // capture/build_snapshot's contract, so an empty snapshot cannot be adopted
    // as a "live" handoff that auto-injects nothing useful.
    if snapshot.open_todos.is_empty() && snapshot.continuation_prompt.is_none() {
        return None;
    }
    // Re-key to this host's project identity so lookup and injection find it.
    snapshot.project_key = project.clone();
    snapshot.disposition = disposition.to_string();

    let _lock = lock_store().ok()?;
    let dir = handoffs_dir().ok()?;
    // A fresh, human-meaningful session id avoids overwriting a local snapshot
    // for the same id while staying recognizable: `import-<source>[-<suffix>]`
    // mirrors the source session so `/handoffres <id>` is not an opaque UUID.
    let source = snapshot.session_id.clone();
    let base = format!("import-{}", sanitize_import_source(&source));
    // Walk up to a bounded number of candidates so a persistent collision
    // (a previous import or a local session sharing the stem) still mints a
    // fresh, distinct id rather than silently overwriting an existing file.
    let mut candidate = base.clone();
    for _ in 0..8 {
        if file_path(&dir, &candidate).is_ok() && load_snapshot(&candidate).is_none() {
            break;
        }
        let short = uuid::Uuid::new_v4().simple().to_string();
        let suffix = if short.len() > 8 {
            &short[..8]
        } else {
            short.as_str()
        };
        candidate = format!("{}-{}", base, suffix);
    }
    // If every suffix still collided (effectively impossible), fall back to the
    // full UUID which is guaranteed not to exist, so we never overwrite a file.
    if load_snapshot(&candidate).is_some() {
        candidate = format!("{}-{}", base, uuid::Uuid::new_v4().simple());
    }
    snapshot.session_id = candidate;
    let session_id = snapshot.session_id.clone();
    crate::storage::write_json_fast(&file_path(&dir, &session_id).ok()?, &snapshot).ok()?;
    upsert_index(&snapshot).ok()?;
    // Keep the archive bounded during import-heavy flows, consistent with
    // capture (which also prunes after a write). The lock is already held.
    prune_archived_snapshots_locked();
    Some(session_id)
}

/// Make a session id safe to embed in an `import-<source>` filename. The
/// regular filenames are already restricted to `[a-z0-9_-]`, so out-of-band
/// payloads could contain anything; normalize to lowercase and drop characters
/// that `file_path` rejects so the readable stem never aliases another session
/// while staying close to the original for recognizability.
fn sanitize_import_source(source: &str) -> String {
    let mut out = String::new();
    for ch in source.to_ascii_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_' {
            out.push(ch);
        }
        // Truncate so the readable stem stays bounded even for pathological
        // sources, and to leave room for an optional disambiguator suffix.
        if out.len() >= 96 {
            break;
        }
    }
    if out.is_empty() {
        out.push_str("handoff");
    }
    out
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
/// pruned; only superseded archived snapshots are eligible. Runs after a
/// successful [`capture`] write and on a host startup sweep (see
/// [`sweep_stale_handoffs`]). Removals and a per-run summary are logged.
/// Failures to read or delete individual files are ignored (best-effort), and
/// a missing or unreadable store is a no-op.
///
/// Acquires the store lock so it is safe to call concurrently with captures.
/// Internal callers that already hold the lock use [`prune_archived_snapshots_locked`].
pub fn prune_archived_snapshots() {
    let _lock = match lock_store() {
        Ok(lock) => lock,
        Err(error) => {
            crate::logging::warn(&format!(
                "[handoff] cannot lock store for prune: {error}"
            ));
            return;
        }
    };
    prune_archived_snapshots_locked();
}

/// The pruning body, run with the store lock already held (by [`capture`] via
/// `prune_archived_snapshots_locked` or by [`prune_archived_snapshots`]).
fn prune_archived_snapshots_locked() {
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
    let mut removed = 0usize;
    for snapshots in by_project.values_mut() {
        // Oldest first so we trim the least-recently-finished first.
        snapshots.sort_by(|a, b| a.ended_at.cmp(&b.ended_at));
        for (offset, snapshot) in snapshots.iter().enumerate() {
            let too_old = now - snapshot.ended_at > age_limit;
            let over_cap = snapshots.len() - offset > MAX_ARCHIVED_SNAPSHOTS_PER_PROJECT;
            if !too_old && !over_cap {
                break;
            }
            if delete_snapshot(&snapshot.session_id) {
                crate::logging::debug(&format!(
                    "[handoff] pruned archived snapshot {} (project {}) ended {} reason={}",
                    snapshot.session_id,
                    snapshot.project_key,
                    snapshot.ended_at,
                    if too_old { "age" } else { "count-cap" },
                ));
                removed += 1;
            }
        }
    }
    if removed > 0 {
        crate::logging::info(&format!(
            "[handoff] pruned {removed} archived handoff snapshot(s)"
        ));
    }
    // Reconcile the index: drop any latest-entry whose snapshot file no longer
    // exists (e.g. deleted out-of-band), so `list_saved_handoffs` never shows a
    // dangling row pointing at a missing file. Runs under the store lock.
    reconcile_dangling_index_entries();
}

/// Drop index `latest` entries whose snapshot file is missing on disk.
///
/// Normally the index and the snapshot directory stay in sync (pruning never
/// deletes a live/latest file, and retire keeps the archived file). But a file
/// can disappear out-of-band (manual delete, earlier process crash between
/// index and file write, a concurrent sweep). Without reconciliation
/// `list_saved_handoffs` surface a row whose `/handoffres` finds nothing.
fn reconcile_dangling_index_entries() {
    let mut index = load_index();
    let before = index.latest.len();
    index
        .latest
        .retain(|entry| load_snapshot(&entry.session_id).is_some());
    if index.latest.len() != before
        && let Ok(path) = index_path()
    {
        let _ = crate::storage::write_json_fast(&path, &index);
    }
}

/// Run the archived-snapshot retention sweep once at host startup.
///
/// Fixes the event-driven gap in [`prune_archived_snapshots`]: that path only
/// runs after a [`capture`] write, so between captures stale archived files can
/// accumulate indefinitely. Calling this once during server/agent boot turns
/// the retention policy into a startup invariant rather than something that
/// only fires opportunistically. Best-effort; a missing store is a no-op.
pub fn sweep_stale_handoffs() {
    prune_archived_snapshots();
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

/// Outcome of [`clear_continuation_prompt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClearTaskOutcome {
    /// Whether a continuation task was present and has now been cleared.
    pub had_task: bool,
    /// Whether the snapshot was removed entirely (it had no open work left).
    pub removed: bool,
}

/// Clear a previously saved continuation prompt for a session.
///
/// Returns `Some(outcome)` when a snapshot exists for the session, or `None`
/// when there is no such snapshot. When the snapshot had open work it is
/// rewritten without the prompt; a snapshot that becomes empty (no open work, no
/// prompt) is removed entirely so it cannot linger as a misleading live handoff.
pub fn clear_continuation_prompt(session_id: &str) -> Option<ClearTaskOutcome> {
    let _lock = lock_store().ok()?;
    let mut snapshot = load_snapshot(session_id)?;
    let had_task = snapshot
        .continuation_prompt
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty());
    if !had_task {
        return Some(ClearTaskOutcome {
            had_task: false,
            removed: false,
        });
    }
    snapshot.continuation_prompt = None;
    if snapshot.open_todos.is_empty() {
        // Nothing left to hand off: drop the file and its index entry so it is
        // not injected as an empty handoff.
        let _ = delete_snapshot(session_id);
        let _ = retire_session(session_id);
        return Some(ClearTaskOutcome {
            had_task: true,
            removed: true,
        });
    }
    match write_snapshot_locked(&snapshot) {
        Ok(()) => Some(ClearTaskOutcome {
            had_task: true,
            removed: false,
        }),
        Err(err) => {
            crate::logging::warn(&format!(
                "[handoff] failed to clear continuation prompt session={} error={}",
                session_id, err
            ));
            None
        }
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
    // The explicit continuation task is the reason the resumed session exists,
    // so it leads the block, ahead of the historical context. Phrasing it as a
    // directive tells the model to perform the task, not merely to be aware of
    // it.
    if let Some(prompt) = &snapshot.continuation_prompt {
        let prompt = prompt.trim();
        if !prompt.is_empty() {
            // The stored value is already normalized to the cap, carrying its own
            // "truncated" marker on its own line when it was cut, so rendering it
            // as-is shows the task (and the marker) without a second notice.
            out.push_str(&format!("\nContinue with this task: {prompt}"));
        }
    }
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
    let continuation = snapshot
        .continuation_prompt
        .clone()
        .filter(|s| !s.trim().is_empty());
    // A goal's id is the slug of its title, which becomes a filename; bound a
    // task-derived title (a continuation prompt can be up to 4096 bytes) so the
    // slug stays within filesystem name limits and promotion cannot fail.
    let continuation_title = continuation
        .as_deref()
        .map(|c| truncate(c, 200));
    let title = snapshot
        .intent
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| continuation_title.clone())
        .unwrap_or_else(|| format!("Continue from session {}", session_id));
    // The explicit continuation task is the most important thing to carry over,
    // so it leads the next steps (a prompt-only handoff would otherwise promote
    // to a goal with no work at all).
    let mut next_steps: Vec<String> = continuation.clone().into_iter().collect();
    next_steps.extend(snapshot.open_todos.iter().map(|t| t.content.clone()));
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

const MAX_PROMPT_BYTES: usize = 4096;

/// Marker appended to a stored continuation prompt that had to be cut. Detected
/// on re-normalization so repeated captures never stack/gargle the notice.
const PROMPT_TRUNCATION_MARKER: &str = "[continuation prompt truncated";

/// Truncate to `max_bytes` on a char boundary, reporting whether any content was
/// dropped so callers can append an explicit notice instead of silently cutting.
fn truncate_notifying(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_string(), false);
    }
    (truncate(s, max_bytes), true)
}

/// Normalize a user-supplied continuation prompt to the stored cap.
///
/// Over-cap tasks are cut with a visible marker, but the result stays within
/// `MAX_PROMPT_BYTES` (the marker's length is reserved up front). Re-normalizing
/// an already-marked prompt is a no-op, so carrying a prompt forward across
/// captures does not stack or garble the notice.
fn normalize_continuation_prompt(p: &str) -> String {
    let p = p.trim();
    if p.len() <= MAX_PROMPT_BYTES {
        return p.to_string();
    }
    if p.contains(PROMPT_TRUNCATION_MARKER) {
        // Already marked yet still over the cap (e.g. a hand-crafted or imported
        // payload). Hard-cut without stacking a second marker.
        return truncate(p, MAX_PROMPT_BYTES);
    }
    let marker = format!("\n{PROMPT_TRUNCATION_MARKER} to {MAX_PROMPT_BYTES} bytes]");
    let budget = MAX_PROMPT_BYTES.saturating_sub(marker.len());
    let (text, truncated) = truncate_notifying(p, budget);
    if truncated {
        format!("{text}{marker}")
    } else {
        text
    }
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
