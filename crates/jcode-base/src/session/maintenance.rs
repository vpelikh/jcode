//! Background maintenance for the on-disk session store.
//!
//! Two independent, best-effort passes run off the startup path:
//!
//! 1. **Backup prune.** Session transcripts (`<id>.json`) are kept forever, but
//!    the atomic-write layer also leaves a single rolling `<id>.bak` next to each
//!    file as a crash-recovery copy (see `jcode_storage::write_bytes_inner`). That
//!    backup is only ever consulted when the primary `.json` is found to be
//!    corrupt on the very next read. For sessions not touched in weeks the primary
//!    is stable, so the stale `.bak` is pure disk overhead (these accumulate into
//!    gigabytes over time), so prune `.bak` files older than a conservative
//!    window. This never touches the `.json` transcripts, so no session data is
//!    lost; at worst a very old, already-stable session loses its redundant
//!    recovery copy.
//!
//! 2. **Oversized-session sweep.** Sessions bloated by the historical event-log
//!    duplication (see [`shrink_oversized_sessions`]) are compacted on disk so an
//!    abandoned one stops wasting disk and load time. Guarded by pid liveness,
//!    file age, and the reload marker; never deletes a transcript.

use crate::storage;
use chrono::{DateTime, Duration, Local};
use std::path::Path;

/// Backups older than this are considered safe to remove. Chosen conservatively
/// so any realistic "crashed mid-write, reopened later" scenario still has its
/// recovery copy.
const BACKUP_RETENTION_DAYS: i64 = 30;

/// Minimum interval between prune passes across all jcode processes.
///
/// The prune walks the entire sessions directory (easily 100k+ entries on a
/// long-lived install), which profiles as the single largest CPU cost of TUI
/// startup when it runs unconditionally. Backups only need to be reclaimed
/// eventually, so one pass per interval per machine is plenty; a marker file's
/// mtime coordinates that across concurrently spawned processes.
const PRUNE_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Remove stale `<id>.bak` files from the sessions directory.
///
/// Best-effort: any I/O error is ignored so this can run on a background thread
/// at startup without ever affecting launch. Skips cheaply (one stat) unless
/// the machine-wide prune interval has elapsed, so spawning many jcode
/// processes at once does not trigger many full directory walks.
pub fn prune_old_session_backups() {
    if let Ok(base) = storage::jcode_dir() {
        let sessions_dir = base.join("sessions");
        if !claim_prune_slot(&base) {
            return;
        }
        prune_old_session_backups_in(&sessions_dir, Local::now());
    }
}

/// Returns true when this process should run the prune pass now, updating the
/// marker so other processes (and future spawns) skip until the next interval.
///
/// The marker touch happens before the walk, so a burst of simultaneous spawns
/// resolves to at most a couple of walkers (racing between the stat and the
/// touch) instead of one per process, and steady-state spawns do a single stat.
fn claim_prune_slot(base: &Path) -> bool {
    let marker = base.join("sessions-bak-prune.stamp");
    if let Ok(metadata) = std::fs::metadata(&marker)
        && let Ok(modified) = metadata.modified()
        && let Ok(age) = std::time::SystemTime::now().duration_since(modified)
        && age.as_secs() < PRUNE_INTERVAL_SECS
    {
        return false;
    }
    // Touch (create or refresh) the marker to claim the slot.
    std::fs::write(&marker, b"").is_ok()
}

/// Core of [`prune_old_session_backups`], parameterized on the directory and
/// "now" for unit testing.
fn prune_old_session_backups_in(sessions_dir: &Path, now: DateTime<Local>) {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return;
    };
    let cutoff = now - Duration::days(BACKUP_RETENTION_DAYS);
    for entry in entries.flatten() {
        let path = entry.path();
        // Only prune the atomic-write backup files; never the .json transcripts
        // or anything else (journals, tmp files, etc.).
        if path.extension().map(|e| e == "bak").unwrap_or(false)
            && let Ok(metadata) = entry.metadata()
            && metadata.is_file()
            && let Ok(modified) = metadata.modified()
        {
            let modified: DateTime<Local> = modified.into();
            if modified < cutoff {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// A session file at least this large is a candidate for the oversized-session
/// sweep. Chosen well above any normal transcript so the sweep only ever looks
/// at the pathological files the event-log duplication produced.
const OVERSIZED_SESSION_BYTES: u64 = 64 * 1024 * 1024;

/// Minimum interval between oversized-session sweeps across all processes.
const OVERSIZED_SWEEP_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Minimum age of a session file before it is swept, so the sweep never touches
/// a session that is currently being written.
const OVERSIZED_SESSION_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Shrink abandoned oversized session files left behind by the historical
/// event-log duplication bug.
///
/// Before the compaction fix, a long tool-heavy session could reach hundreds of
/// MB (or GB) because its append-only event log held a full-transcript
/// `ReplaceMessages` clone per per-step prune. Those files shrink on their next
/// save, but an abandoned session is never saved again, so it stays huge and
/// stays slow to load. This best-effort sweep finds large, old, not-currently-
/// active `.json` session files, loads each and rewrites it through the normal
/// compaction-aware save path.
///
/// Safety guards, all required:
/// - rate-limited to one pass per `OVERSIZED_SWEEP_INTERVAL_SECS` per machine;
/// - skips any session whose owning process is still alive (pid liveness is
///   checked, so a crashed session's leftover marker does not block the sweep);
/// - skips files modified within `OVERSIZED_SESSION_MIN_AGE`;
/// - skips the whole pass while a *fresh* server reload marker
///   (`<runtime_dir>/jcode.reload`) is present, since a reload handoff rewrites
///   sessions as they restart (a stale marker left by a completed reload does not
///   block the sweep);
/// - never deletes anything; a file that fails to load is left untouched.
///
/// Best-effort: any error is ignored so this can never affect startup.
pub fn shrink_oversized_sessions() {
    let Ok(base) = storage::jcode_dir() else {
        return;
    };
    let sessions_dir = base.join("sessions");
    // Check the reload marker BEFORE claiming the once-per-day slot: a reload
    // handoff in flight is a transient skip, and stamping the slot here would
    // suppress the sweep for a full day. Only claim once we are actually going
    // to walk the directory.
    if reload_marker_present() {
        return;
    }
    if !claim_sweep_slot(&base) {
        return;
    }
    shrink_oversized_sessions_in(
        &sessions_dir,
        OVERSIZED_SESSION_BYTES,
        OVERSIZED_SESSION_MIN_AGE,
        std::time::SystemTime::now(),
    );
}

/// How long a reload marker is honored before it is treated as stale.
///
/// A completed reload's marker (`SocketReady`) is deliberately not cleared in
/// normal operation (see the server's `reload_suppresses_idle_shutdown`), so a
/// bare existence check would let a lingering marker suppress the sweep on every
/// run forever. A genuine handoff finishes in well under this window; past it the
/// marker is stale and must not block maintenance.
const RELOAD_MARKER_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Whether a server reload handoff is currently in flight. The reload marker is a
/// single global file; a handoff rewrites sessions as they restart, so the sweep
/// must not race it. A marker older than [`RELOAD_MARKER_STALE_AFTER`] is treated
/// as stale (a completed reload leaves its marker behind on purpose).
fn reload_marker_present() -> bool {
    let marker = storage::runtime_dir().join("jcode.reload");
    let Ok(metadata) = std::fs::metadata(&marker) else {
        return false;
    };
    match metadata.modified() {
        Ok(modified) => std::time::SystemTime::now()
            .duration_since(modified)
            .map(|age| age < RELOAD_MARKER_STALE_AFTER)
            // Unreadable/clock-skewed mtime: assume active (safer to skip).
            .unwrap_or(true),
        Err(_) => true,
    }
}

/// Rate-limit the sweep independently of the `.bak` prune interval.
fn claim_sweep_slot(base: &Path) -> bool {
    let marker = base.join("sessions-shrink.stamp");
    if let Ok(metadata) = std::fs::metadata(&marker)
        && let Ok(modified) = metadata.modified()
        && let Ok(age) = std::time::SystemTime::now().duration_since(modified)
        && age.as_secs() < OVERSIZED_SWEEP_INTERVAL_SECS
    {
        return false;
    }
    std::fs::write(&marker, b"").is_ok()
}

/// Whether `session_id` currently has a live owning process.
///
/// `active_session_ids` returns raw marker filenames, and a marker can outlive
/// its owner (a crashed server leaves one behind), so it must not be used to
/// decide liveness; `session_presence` checks PID liveness, as the storage
/// module documents.
fn session_has_live_owner(session_id: &str) -> bool {
    storage::session_presence()
        .into_iter()
        .any(|presence| presence.session_id == session_id)
}

/// Core of [`shrink_oversized_sessions`], parameterized for unit testing.
///
/// `min_bytes`, `min_age`, and `now` are injectable so tests can exercise the
/// sweep without writing multi-hundred-MB fixtures. The load and save still go
/// through the real [`crate::session::Session`] API, which targets the
/// configured `JCODE_HOME`; tests that exercise the rewrite point `JCODE_HOME` at
/// a temp home whose `sessions` subdir is `sessions_dir`.
fn shrink_oversized_sessions_in(
    sessions_dir: &Path,
    min_bytes: u64,
    min_age: std::time::Duration,
    now: std::time::SystemTime,
) {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return;
    };
    // Skip the whole pass while a server reload is in flight. The reload marker
    // is a single global file, not a per-session one, and a reload handoff
    // rewrites sessions as they restart; avoid racing it. Re-checked here (not
    // only in the caller) so the guard holds even if a reload starts between
    // claiming the slot and walking the directory.
    if reload_marker_present() {
        return;
    }
    // Skip sessions whose owning process is genuinely alive. `active_session_ids`
    // returns raw marker filenames, and a marker can outlive its owner (a
    // crashed server leaves one behind), so filtering on it alone would skip
    // exactly the abandoned sessions this sweep exists to reclaim.
    // `session_presence` checks PID liveness the way the module documents.
    let active: std::collections::HashSet<String> = storage::session_presence()
        .into_iter()
        .map(|presence| presence.session_id)
        .collect();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e != "json").unwrap_or(true) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() || metadata.len() < min_bytes {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now
            .duration_since(modified)
            .map(|age| age < min_age)
            .unwrap_or(true)
        {
            continue;
        }
        // The file stem is the session id; skip sessions a client owns.
        let Some(session_id) = path.file_stem().map(|s| s.to_string_lossy().to_string()) else {
            continue;
        };
        if active.contains(&session_id) {
            continue;
        }
        let Ok(mut session) = crate::session::Session::load(&session_id) else {
            continue;
        };
        // `save` writes to `session_path(self.id)`, while this loop loaded by
        // filename stem. Normally they match; if a file's internal id diverged
        // from its name, saving would create a second file and leave the original
        // bloated, so skip such anomalies rather than duplicating the session.
        if session.id != session_id {
            continue;
        }
        // The user may have opened the session (or it may have been saved) between
        // the initial active-client check and now; the sweep loop can run for a
        // while. Re-check both before the write: skip if the session has since
        // gained a live owner, and only rewrite when the on-disk file is
        // byte-identical in the ways we can cheaply observe (same size AND same
        // mtime), so we never clobber a concurrent save. The save itself runs the
        // normal compaction path.
        if session_has_live_owner(&session_id) {
            continue;
        }
        // A reload can also start mid-sweep; stop the whole pass rather than
        // rewriting files while a handoff is restarting sessions.
        if reload_marker_present() {
            return;
        }
        match std::fs::metadata(&path) {
            Ok(current)
                if current.len() == metadata.len() && current.modified().ok() == Some(modified) =>
            {
                // Save without re-stamping activity: the sweep is storage
                // hygiene, not usage, so the session must not jump to the top of
                // the recent-session list.
                if session.save_preserving_activity().is_ok() {
                    // Only a checkpoint (full snapshot rewrite) shrinks the
                    // primary and rotates the previous file to `<id>.bak`. The
                    // save can instead append a journal delta, leaving the
                    // primary untouched; in that case any existing `.bak` is a
                    // prior legitimate recovery copy and must be kept. Gate the
                    // cleanup/log on the primary actually shrinking.
                    let size_after = std::fs::metadata(&path)
                        .map(|m| m.len())
                        .unwrap_or(u64::MAX);
                    if size_after < metadata.len() {
                        // The atomic-write layer preserved the pre-sweep file as
                        // `<id>.bak` — a hard link to the bloated inode we just
                        // replaced. Dropping it makes the reclaim immediate
                        // rather than waiting weeks for the `.bak` prune.
                        // Best-effort: a failure only costs disk.
                        let _ = std::fs::remove_file(path.with_extension("bak"));
                        crate::logging::info(&format!(
                            "oversized-session sweep: compacted {} ({} -> {} bytes)",
                            session_id,
                            metadata.len(),
                            size_after
                        ));
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use std::time::{Duration as StdDuration, SystemTime};

    #[test]
    fn claim_prune_slot_rate_limits_within_interval_and_reclaims_after() {
        let dir = std::env::temp_dir().join(format!(
            "jcode-bak-claim-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("create temp dir");

        // First claim wins and creates the marker.
        assert!(claim_prune_slot(&dir), "first claim should win");
        let marker = dir.join("sessions-bak-prune.stamp");
        assert!(marker.exists(), "marker should be created");

        // A concurrent/subsequent spawn within the interval is rejected.
        assert!(
            !claim_prune_slot(&dir),
            "second claim within interval should be skipped"
        );

        // Once the marker is older than the interval the slot opens again.
        let old = SystemTime::now() - StdDuration::from_secs(PRUNE_INTERVAL_SECS + 60);
        File::options()
            .write(true)
            .open(&marker)
            .and_then(|f| f.set_modified(old))
            .expect("age the marker");
        assert!(
            claim_prune_slot(&dir),
            "claim should succeed after the interval elapses"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prunes_only_old_bak_files() {
        let dir = std::env::temp_dir().join(format!(
            "jcode-bak-prune-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("create temp dir");

        let write = |name: &str, age_days: u64| {
            let path = dir.join(name);
            let mut f = File::create(&path).expect("create");
            f.write_all(b"{}").ok();
            if age_days > 0 {
                let mtime = SystemTime::now() - StdDuration::from_secs(age_days * 24 * 60 * 60);
                f.set_modified(mtime).expect("set mtime");
            }
            path
        };

        // 60-day-old backup: should be pruned.
        let old_bak = write("session_old.bak", 60);
        // 5-day-old backup: within window, should survive.
        let recent_bak = write("session_recent.bak", 5);
        // Transcripts must never be removed, regardless of age.
        let old_json = write("session_old.json", 60);
        let recent_json = write("session_recent.json", 0);
        // Other artifacts must be left alone.
        let journal = write("session_old.journal.jsonl", 60);

        prune_old_session_backups_in(&dir, Local::now());

        assert!(!old_bak.exists(), "old .bak should be pruned");
        assert!(recent_bak.exists(), "recent .bak must survive");
        assert!(
            old_json.exists(),
            "old .json transcript must never be removed"
        );
        assert!(recent_json.exists(), "recent .json transcript must survive");
        assert!(journal.exists(), "journals are out of scope");

        fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod shrink_tests {
    use super::*;
    use crate::storage;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration as StdDuration, SystemTime};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jcode-shrink-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn backdate(path: &std::path::Path, secs: u64) {
        let old = SystemTime::now() - StdDuration::from_secs(secs);
        fs::File::options()
            .write(true)
            .open(path)
            .expect("open to backdate")
            .set_modified(old)
            .expect("set mtime");
    }

    /// Sets an env var for the duration of a test and restores it on drop.
    struct EnvVarGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let prev = std::env::var_os(key);
            crate::env::set_var(key, value);
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(prev) = &self.prev {
                crate::env::set_var(self.key, prev);
            } else {
                crate::env::remove_var(self.key);
            }
        }
    }

    fn write_oversized_session(sessions: &std::path::Path, session_id: &str) {
        let mut session =
            crate::session::Session::create_with_id(session_id.to_string(), None, None);
        // ~1 MiB per message; 20 messages plus 8 full replacements => a large,
        // redundant event log.
        let chunk = "s".repeat(1024 * 1024);
        for i in 0..20 {
            session.append_stored_message(jcode_session_types::StoredMessage {
                id: format!("m{i}"),
                role: jcode_message_types::Role::User,
                content: vec![jcode_message_types::ContentBlock::Text {
                    text: format!("{chunk} {i}"),
                    cache_control: None,
                }],
                display_role: None,
                timestamp: None,
                tool_duration_ms: None,
                token_usage: None,
            });
        }
        for _ in 0..8 {
            session.replace_messages(session.messages.clone());
        }
        let path = sessions.join(format!("{session_id}.json"));
        fs::write(&path, serde_json::to_vec(&session).unwrap()).expect("write session");
        backdate(&path, 7200);
    }

    /// A sub-floor file is never a candidate.
    #[test]
    fn sweep_ignores_files_below_size_floor() {
        let sessions = temp_dir("small");
        let small = sessions.join("session_small.json");
        fs::write(&small, b"{}").expect("write");
        backdate(&small, 7200);

        shrink_oversized_sessions_in(
            &sessions,
            1024,
            StdDuration::from_secs(60),
            SystemTime::now(),
        );
        assert!(small.exists(), "a sub-floor file must be left untouched");
        fs::remove_dir_all(&sessions).ok();
    }

    /// A large but recently modified file (a live save) is skipped.
    #[test]
    fn sweep_skips_recently_modified_files() {
        let sessions = temp_dir("recent");
        let big = sessions.join("session_recent_big.json");
        fs::write(&big, vec![b'x'; 4096]).expect("write");

        shrink_oversized_sessions_in(
            &sessions,
            1024,
            StdDuration::from_secs(3600),
            SystemTime::now(),
        );
        assert_eq!(
            fs::metadata(&big).expect("stat").len(),
            4096,
            "a recently modified file must not be rewritten"
        );
        fs::remove_dir_all(&sessions).ok();
    }

    /// A large, old, inactive file is loaded and compacted through the real save
    /// path, shrinking it on disk.
    #[test]
    fn sweep_compacts_large_old_inactive_file() {
        let _env_lock = crate::storage::lock_test_env();
        // Point JCODE_HOME at a temp home whose `sessions` dir is the sweep dir,
        // so `Session::load`/`save` operate on the fixture.
        let home = temp_dir("compact-home");
        let sessions = home.join("sessions");
        fs::create_dir_all(&sessions).expect("sessions dir");
        let _home = EnvVarGuard::set("JCODE_HOME", home.as_os_str());

        let session_id = "session_sweep_target";
        write_oversized_session(&sessions, session_id);
        let path = sessions.join(format!("{session_id}.json"));
        let size_before = fs::metadata(&path).unwrap().len();
        // Capture the pre-sweep activity clock: the sweep must NOT bump it (a
        // maintenance rewrite is not user activity).
        let updated_at_before = crate::session::Session::load(session_id)
            .unwrap()
            .updated_at;

        shrink_oversized_sessions_in(
            &sessions,
            1024 * 1024,
            StdDuration::from_secs(60),
            SystemTime::now(),
        );

        let size_after = fs::metadata(&path).unwrap().len();
        assert!(
            size_after < size_before,
            "an oversized old inactive session must be compacted ({size_before} -> {size_after})"
        );
        // The transcript must be preserved exactly across the sweep.
        let loaded = crate::session::Session::load(session_id).expect("reload");
        assert_eq!(loaded.messages.len(), 20, "transcript preserved");
        assert_eq!(
            loaded.updated_at, updated_at_before,
            "the sweep must not re-stamp an abandoned session's updated_at"
        );
        // The atomic-write layer would have hard-linked the bloated pre-sweep
        // file to `<id>.bak`; the sweep must drop that redundant copy so the
        // reclaim is real rather than waiting weeks for the `.bak` prune.
        let bak = sessions.join(format!("{session_id}.bak"));
        assert!(
            !bak.exists(),
            "the sweep must remove the redundant pre-sweep backup it created"
        );
        let _ = storage::jcode_dir();
        fs::remove_dir_all(&home).ok();
    }

    /// A session whose owning process is genuinely alive is skipped, but one
    /// whose active-pid marker is stale (owner died) must still be swept. A
    /// crash leaves the marker behind, so filtering on raw marker filenames
    /// would skip exactly the abandoned sessions the sweep exists to reclaim.
    #[test]
    fn sweep_skips_live_owner_but_reclaims_stale_marker() {
        let _env_lock = crate::storage::lock_test_env();
        let home = temp_dir("live-marker-home");
        let sessions = home.join("sessions");
        fs::create_dir_all(&sessions).expect("sessions dir");
        let _home = EnvVarGuard::set("JCODE_HOME", home.as_os_str());

        // One session owned by this live process, one owned by an impossible PID.
        let live_id = "session_live_owner";
        let dead_id = "session_stale_owner";
        write_oversized_session(&sessions, live_id);
        write_oversized_session(&sessions, dead_id);
        crate::storage::register_active_pid(live_id, std::process::id());
        // PID 0 is never a running process, so this marker is treated as stale.
        crate::storage::register_active_pid(dead_id, 0);

        let live_path = sessions.join(format!("{live_id}.json"));
        let dead_path = sessions.join(format!("{dead_id}.json"));
        let live_before = fs::metadata(&live_path).unwrap().len();
        let dead_before = fs::metadata(&dead_path).unwrap().len();

        shrink_oversized_sessions_in(
            &sessions,
            1024 * 1024,
            StdDuration::from_secs(60),
            SystemTime::now(),
        );

        assert_eq!(
            fs::metadata(&live_path).unwrap().len(),
            live_before,
            "a session with a live owner must not be rewritten"
        );
        assert!(
            fs::metadata(&dead_path).unwrap().len() < dead_before,
            "a session whose owner has died (stale marker) must still be reclaimed"
        );

        crate::storage::unregister_active_pid(live_id);
        let _ = storage::jcode_dir();
        fs::remove_dir_all(&home).ok();
    }

    /// While a reload handoff is in flight the sweep must skip entirely: it must
    /// neither rewrite the file nor claim the once-per-day slot (so the sweep can
    /// still run once the reload finishes).
    #[test]
    fn sweep_skips_while_reload_marker_present_and_preserves_slot() {
        let _env_lock = crate::storage::lock_test_env();
        let home = temp_dir("reload-home");
        let sessions = home.join("sessions");
        fs::create_dir_all(&sessions).expect("sessions dir");
        let _home = EnvVarGuard::set("JCODE_HOME", home.as_os_str());

        // Point the runtime dir at a temp dir and drop a reload marker there.
        let runtime = temp_dir("reload-runtime");
        let _runtime = EnvVarGuard::set("JCODE_RUNTIME_DIR", runtime.as_os_str());
        fs::write(runtime.join("jcode.reload"), b"{}").expect("reload marker");

        let session_id = "session_during_reload";
        write_oversized_session(&sessions, session_id);
        let path = sessions.join(format!("{session_id}.json"));
        let size_before = fs::metadata(&path).unwrap().len();

        // The public entry point: it must bail on the reload marker before
        // claiming the daily slot.
        shrink_oversized_sessions();

        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            size_before,
            "the sweep must not rewrite a session while a reload is in flight"
        );
        assert!(
            !home.join("sessions-shrink.stamp").exists(),
            "a reload skip must not burn the once-per-day slot"
        );

        // With the marker gone, the sweep proceeds and claims the slot.
        fs::remove_file(runtime.join("jcode.reload")).expect("clear marker");
        shrink_oversized_sessions();
        assert!(
            home.join("sessions-shrink.stamp").exists(),
            "the sweep should claim the slot once the reload is done"
        );

        // A *stale* lingering marker (a completed reload leaves `SocketReady`
        // behind on purpose) must not block the sweep forever.
        fs::remove_file(home.join("sessions-shrink.stamp")).ok();
        let stale_marker = runtime.join("jcode.reload");
        fs::write(&stale_marker, b"{}").expect("stale marker");
        let old = SystemTime::now() - StdDuration::from_secs(60 * 60);
        fs::File::options()
            .write(true)
            .open(&stale_marker)
            .expect("open stale marker")
            .set_modified(old)
            .expect("age the marker");
        assert!(
            !reload_marker_present(),
            "a marker older than RELOAD_MARKER_STALE_AFTER must read as stale"
        );

        let _ = storage::jcode_dir();
        fs::remove_dir_all(&home).ok();
        fs::remove_dir_all(&runtime).ok();
    }
}
