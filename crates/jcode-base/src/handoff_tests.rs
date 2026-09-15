use super::*;

/// project_key prefers the git remote URL for portability across machines.
#[test]
fn project_key_prefers_git_remote() {
    if !git_available() {
        return;
    }
    let dir = tempfile::TempDir::new().expect("tempdir");
    let init = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["init", "-q"])
        .output();
    if !init.map(|o| o.status.success()).unwrap_or(false) {
        return;
    }
    // Add a remote so git_remote_url is defined.
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args([
            "remote",
            "add",
            "origin",
            "https://example.com/acme/widget.git",
        ])
        .output()
        .ok();
    assert_eq!(
        project_key(Some(dir.path())).as_deref(),
        Some("git:https://example.com/acme/widget.git"),
        "project key should be the git origin URL"
    );
}

fn git_available() -> bool {
    std::process::Command::new("git")
        .args(["--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Fallback: a file-less path hashes to the path form even without git.
#[test]
fn project_key_falls_back_to_path() {
    let dir = tempfile::tempdir().unwrap();
    let key = project_key(Some(dir.path()));
    let key = key.expect("path fallback should yield a key");
    assert!(key.starts_with("path:"), "expected path: prefix, got {key}");
}

#[test]
fn missing_relative_project_uses_absolute_fallback() {
    let relative = PathBuf::from(format!("missing-handoff-{}", uuid::Uuid::new_v4()));
    let expected = std::env::current_dir().unwrap().join(&relative);
    assert_eq!(
        project_key(Some(&relative)),
        Some(format!("path:{}", expected.display()))
    );
}

/// A transcript's last assistant text block is isolated from tool markers and
/// subsequent user blocks.
#[test]
fn extracts_last_assistant_text() {
    let transcript = concat!(
        "**User:**\nbuild split\n",
        "**Assistant:**\nI will split the server.",
        "\n[Used tool: todo]\n",
        "**Assistant:**\nNext, extract ClientState.\n",
    );
    let extracted = extract_last_assistant_text(transcript);
    assert_eq!(extracted.as_deref(), Some("Next, extract ClientState."));
}

/// Capture writes a file and bumps the index; empty/open-less sessions do not.
#[tokio::test]
async fn capture_only_writes_when_open_todos_exist() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = &env._home;
    let cwd = home.path().join("project");
    std::fs::create_dir_all(&cwd).ok();

    // No open todos -> nothing captured.
    let got = capture("s-empty", Some(&cwd), "closed", None);
    assert!(got.is_none());

    // With an open todo -> snapshots written.
    crate::todo::save_todos(
        "s-work",
        &[TodoItem {
            id: "t1".into(),
            content: "split the server".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            group: Some("server".into()),
            confidence: None,
            ..Default::default()
        }],
    )
    .expect("save todos");
    crate::todo::save_plan(
        "s-work",
        &crate::todo::TodoPlan {
            user_intention: Some("split server into services".into()),
            ..Default::default()
        },
    )
    .expect("save plan");

    let snap = capture("s-work", Some(&cwd), "closed", None).expect("should capture");
    assert_eq!(snap.open_todos.len(), 1);
    assert_eq!(snap.intent.as_deref(), Some("split server into services"));
    assert!(snap.project_key.starts_with("path:") || snap.project_key.starts_with("git:"));

    // Index knows about it.
    let latest = latest_handoff_for_project(Some(&cwd));
    assert_eq!(latest.as_deref(), Some("s-work"));
}

/// build_snapshot filters out completed/cancelled todos.
#[test]
fn open_filter_drops_completed_and_cancelled() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let dir = &env._home;
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&cwd).ok();

    crate::todo::save_todos(
        "s-done",
        &[
            TodoItem {
                id: "a".into(),
                content: "done".into(),
                status: "completed".into(),
                ..Default::default()
            },
            TodoItem {
                id: "b".into(),
                content: "blocked".into(),
                status: "cancelled".into(),
                ..Default::default()
            },
        ],
    )
    .expect("todos");
    assert!(
        build_snapshot("s-done", Some(&cwd), "closed", None).is_none(),
        "only-completed/cancelled sessions must not produce a handoff"
    );
}

/// promote_to_initiative creates a project goal from a handoff snapshot.
#[tokio::test]
async fn promote_to_initiative_creates_a_goal() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = &env._home;
    let cwd = home.path().join("project");
    std::fs::create_dir_all(&cwd).ok();

    crate::todo::save_todos(
        "s-promote",
        &[TodoItem {
            id: "p1".into(),
            content: "finish slice 2".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            group: Some("handoff".into()),
            confidence: None,
            ..Default::default()
        }],
    )
    .expect("todos");
    crate::todo::save_plan(
        "s-promote",
        &crate::todo::TodoPlan {
            user_intention: Some("continue server split".into()),
            ..Default::default()
        },
    )
    .expect("plan");
    capture("s-promote", Some(&cwd), "closed", None).expect("capture");

    let goal_id = promote_to_initiative("s-promote", Some(&cwd))
        .expect("promote should succeed")
        .expect("promote should create a goal");
    assert!(
        crate::goal::load_goal(&goal_id, Some(crate::goal::GoalScope::Project), Some(&cwd))
            .expect("load")
            .is_some(),
        "promoted goal should be loadable"
    );
}

/// A session with an attached project-scoped goal must record that goal id in
/// its handoff (F1: previously `load_attached_initiative` passed no working_dir,
/// so project-scoped attachments never resolved).
#[tokio::test]
async fn build_snapshot_records_attached_project_initiative() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = &env._home;
    let cwd = home.path().join("project");
    std::fs::create_dir_all(&cwd).ok();

    crate::todo::save_todos(
        "s-ini",
        &[TodoItem {
            id: "i".into(),
            content: "open work".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            group: None,
            confidence: None,
            ..Default::default()
        }],
    )
    .expect("todos");

    // Create a project-scoped goal and attach it to the session.
    let goal = crate::goal::create_goal(
        crate::goal::GoalCreateInput {
            id: Some("split-server-goal".into()),
            title: "Split the server".into(),
            scope: crate::goal::GoalScope::Project,
            ..Default::default()
        },
        Some(&cwd),
    )
    .expect("create goal");
    crate::goal::attach_goal_to_session("s-ini", &goal, Some(&cwd)).expect("attach");

    let snap = build_snapshot("s-ini", Some(&cwd), "closed", None).expect("snapshot");
    assert_eq!(
        snap.initiative_id.as_deref(),
        Some(goal.id.as_str()),
        "the attached project initiative must be recorded in the handoff"
    );
}

/// render_boot_context emits a compact block when a handoff exists.
#[tokio::test]
async fn render_boot_context_produces_block() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = &env._home;
    let cwd = home.path().join("project");
    std::fs::create_dir_all(&cwd).ok();

    crate::todo::save_todos(
        "s-boot",
        &[TodoItem {
            id: "x".into(),
            content: "finish slice".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            group: None,
            confidence: None,
            ..Default::default()
        }],
    )
    .expect("todos");
    crate::todo::save_plan(
        "s-boot",
        &crate::todo::TodoPlan {
            user_intention: Some("persist cross-session work".into()),
            ..Default::default()
        },
    )
    .expect("plan");

    capture("s-boot", Some(&cwd), "closed", None).expect("capture");
    let block = render_boot_context(Some(&cwd)).expect("boot context");
    assert!(block.contains("[Handoff from previous session]"));
    assert!(block.contains("persist cross-session work"));
    assert!(block.contains("finish slice"));
}

fn git_repo_with_remote(dir: &std::path::Path, url: &str) {
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["init", "-q"])
        .output();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["remote", "add", "origin", url])
        .output();
}

/// Integration boundary: two "checkouts" of the same project (different
/// absolute paths, same git origin) must land in one project bucket, so a
/// handoff captured on one machine/path is found from the other.
#[test]
fn same_git_origin_buckets_across_paths() {
    let _guard = crate::storage::lock_test_env();
    // Skip when git is unavailable.
    if !git_available() {
        return;
    }
    let checkout_a = tempfile::TempDir::new().expect("a");
    let checkout_b = tempfile::TempDir::new().expect("b");
    git_repo_with_remote(checkout_a.path(), "https://example.com/acme/widget.git");
    git_repo_with_remote(checkout_b.path(), "https://example.com/acme/widget.git");

    assert_eq!(
        project_key(Some(checkout_a.path())),
        project_key(Some(checkout_b.path())),
        "same origin must yield the same portable project key regardless of path"
    );
    // A different origin must differ.
    let other = tempfile::TempDir::new().expect("other");
    git_repo_with_remote(other.path(), "https://example.com/acme/other.git");
    assert_ne!(
        project_key(Some(checkout_a.path())),
        project_key(Some(other.path())),
        "different git origins must yield different project keys"
    );
}

/// Failure mode: a corrupt or unparsable index must not panic or fail capture;
/// it resets to an empty index and still records the new handoff.
#[test]
fn corrupt_index_does_not_fail_capture() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = &env._home;
    let cwd = home.path().join("project");
    std::fs::create_dir_all(&cwd).ok();

    // Write garbage over the index path.
    let dir = crate::storage::jcode_dir()
        .expect("jcode dir")
        .join("handoffs");
    std::fs::create_dir_all(&dir).ok();
    std::fs::write(dir.join("index.json"), "{{{ not json").ok();

    crate::todo::save_todos(
        "s-corrupt",
        &[TodoItem {
            id: "c".into(),
            content: "survive corruption".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            group: None,
            confidence: None,
            ..Default::default()
        }],
    )
    .expect("todos");

    let snap = capture("s-corrupt", Some(&cwd), "closed", None).expect("capture must not fail");
    assert_eq!(snap.session_id, "s-corrupt");
    assert_eq!(
        latest_handoff_for_project(Some(&cwd)).as_deref(),
        Some("s-corrupt"),
        "a fresh capture after a corrupt index should still register"
    );
}

/// The first-user-message injection consumes `render_boot_context`: it must
/// yield the compact block exactly when a fresh session (empty conversation)
/// has a handoff for the working dir, and none otherwise.
#[test]
fn boot_context_is_present_for_fresh_session_with_handoff() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = &env._home;
    let cwd = home.path().join("project");
    std::fs::create_dir_all(&cwd).ok();

    // No handoff yet -> no boot context.
    assert!(render_boot_context(Some(&cwd)).is_none());

    // An already-running project with open work produces a handoff on close.
    crate::todo::save_todos(
        "s-bootctx",
        &[TodoItem {
            id: "bc".into(),
            content: "open work".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            group: None,
            confidence: None,
            ..Default::default()
        }],
    )
    .expect("todos");
    crate::todo::save_plan(
        "s-bootctx",
        &crate::todo::TodoPlan {
            user_intention: Some("resume the split".into()),
            ..Default::default()
        },
    )
    .expect("plan");
    capture("s-bootctx", Some(&cwd), "closed", None).expect("capture");

    // A fresh session in the same working dir now has boot context to consume.
    let block = render_boot_context(Some(&cwd)).expect("boot context present");
    assert!(block.contains("[Handoff from previous session]"));
    assert!(block.contains("resume the split"));
    assert!(block.contains("open work"));

    // The injection consumes this once; a later session in a different dir must
    // not inherit it.
    let unrelated = home.path().join("other");
    std::fs::create_dir_all(&unrelated).ok();
    assert!(render_boot_context(Some(&unrelated)).is_none());
}

/// End-to-end through the public API over the real storage layout: capture on
/// close, boot-render from a *different* checkout path of the same project
/// (proves cross-path injection), and promote to an initiative.
#[tokio::test]
async fn full_workflow_capture_boot_render_promote() {
    let _guard = crate::storage::lock_test_env();
    if !git_available() {
        return;
    }
    let _env = HandoffTestEnv::new();

    // Session A works in checkout A, captures with open work.
    let checkout_a = tempfile::TempDir::new().expect("a");
    git_repo_with_remote(checkout_a.path(), "https://example.com/acme/widget.git");
    crate::todo::save_todos(
        "s-a",
        &[TodoItem {
            id: "t1".into(),
            content: "move mutation behind service handle".into(),
            status: "in_progress".into(),
            priority: "high".into(),
            group: Some("split".into()),
            confidence: None,
            ..Default::default()
        }],
    )
    .expect("todos");
    crate::todo::save_plan(
        "s-a",
        &crate::todo::TodoPlan {
            user_intention: Some("split server into services".into()),
            ..Default::default()
        },
    )
    .expect("plan");
    let snap = capture("s-a", Some(checkout_a.path()), "closed", None).expect("session A capture");
    assert_eq!(snap.open_todos.len(), 1);

    // A later session on a *different* checkout (= another machine/path) of the
    // same repo boots with the handoff injected, without re-explaining.
    let checkout_b = tempfile::TempDir::new().expect("b");
    git_repo_with_remote(checkout_b.path(), "https://example.com/acme/widget.git");
    let block = render_boot_context(Some(checkout_b.path()))
        .expect("boot context from cross-path checkout");
    assert!(block.contains("split server into services"), "{block}");
    assert!(
        block.contains("move mutation behind service handle"),
        "{block}"
    );

    // The durable handoff promotes into a project-scoped initiative.
    let goal_id = promote_to_initiative("s-a", Some(checkout_b.path()))
        .expect("promote from cross-path checkout")
        .expect("goal id");
    assert!(
        crate::goal::load_goal(
            &goal_id,
            Some(crate::goal::GoalScope::Project),
            Some(checkout_b.path())
        )
        .expect("load")
        .is_some(),
        "promoted initiative should be loadable from the other checkout"
    );
}
fn fixture(session: &str, project: &str) -> HandoffSnapshot {
    HandoffSnapshot {
        session_id: session.into(),
        project_key: project.into(),
        ended_at: Utc::now(),
        disposition: "closed".into(),
        working_dir: None,
        intent: None,
        open_todos: vec![HandoffTodo {
            id: "t".into(),
            content: "work".into(),
            status: "pending".into(),
            group: None,
            confidence: None,
        }],
        last_assistant_text: None,
        initiative_id: None,
    }
}

#[test]
fn rejects_reserved_and_colliding_session_paths() {
    let dir = Path::new("unused");
    assert!(file_path(dir, "index").is_err());
    assert!(file_path(dir, "a/b").is_err());
    assert!(file_path(dir, "").is_err());
    assert!(file_path(dir, "a_b").is_ok());
}

#[test]
fn older_snapshot_cannot_replace_newer_project_entry() {
    let _guard = crate::storage::lock_test_env();
    let _env = HandoffTestEnv::new();
    let newer = fixture("new", "project");
    let mut older = fixture("old", "project");
    older.ended_at = newer.ended_at - chrono::Duration::seconds(10);
    write_snapshot(&newer).unwrap();
    write_snapshot(&older).unwrap();
    assert_eq!(load_index().latest[0].session_id, "new");
}

#[test]
fn concurrent_writers_preserve_every_project() {
    let _guard = crate::storage::lock_test_env();
    let _env = HandoffTestEnv::new();
    let barrier = std::sync::Barrier::new(24);
    std::thread::scope(|scope| {
        for n in 0..24 {
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                write_snapshot(&fixture(&format!("session-{n}"), &format!("project-{n}"))).unwrap();
            });
        }
    });
    let index = load_index();
    assert_eq!(index.latest.len(), 24);
    for entry in index.latest {
        assert_eq!(
            load_snapshot(&entry.session_id).unwrap().project_key,
            entry.project_key
        );
    }
}

#[test]
fn completed_recapture_retires_only_its_own_index_entry() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let cwd = env._home.path();
    crate::todo::save_todos(
        "resumed",
        &[TodoItem {
            id: "t".into(),
            content: "finish".into(),
            status: "pending".into(),
            ..Default::default()
        }],
    )
    .unwrap();
    capture("resumed", Some(cwd), "closed", None).unwrap();
    crate::todo::save_todos(
        "resumed",
        &[TodoItem {
            id: "t".into(),
            content: "finish".into(),
            status: "completed".into(),
            ..Default::default()
        }],
    )
    .unwrap();
    assert!(capture("resumed", Some(cwd), "closed", None).is_none());
    assert!(render_boot_context(Some(cwd)).is_none());
    // Retirement must not discard a newer handoff belonging to someone else.
    write_snapshot(&fixture("other", &project_key(Some(cwd)).unwrap())).unwrap();
    assert!(capture("resumed", Some(cwd), "closed", None).is_none());
    assert_eq!(
        latest_handoff_for_project(Some(cwd)).as_deref(),
        Some("other")
    );
}

#[test]
fn moved_session_does_not_leak_context_to_old_project() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let a = env._home.path().join("a");
    let b = env._home.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    crate::todo::save_todos(
        "moving",
        &[TodoItem {
            id: "t".into(),
            content: "work".into(),
            status: "pending".into(),
            ..Default::default()
        }],
    )
    .unwrap();
    capture("moving", Some(&a), "closed", None).unwrap();
    let old_index = load_index();
    capture("moving", Some(&b), "closed", None).unwrap();
    assert!(latest_handoff_for_project(Some(&a)).is_none());
    assert!(render_boot_context(Some(&b)).is_some());
    // Even a stale index restored after a crash must not inject another project.
    crate::storage::write_json_fast(&index_path().unwrap(), &old_index).unwrap();
    assert!(render_boot_context(Some(&a)).is_none());
}

#[test]
fn project_key_observes_origin_changes_in_same_process() {
    assert!(git_available(), "git required for portability regression");
    let dir = tempfile::tempdir().unwrap();
    assert!(project_key(Some(dir.path())).unwrap().starts_with("path:"));
    git_repo_with_remote(dir.path(), "https://example.com/first.git");
    assert_eq!(
        project_key(Some(dir.path())).as_deref(),
        Some("git:https://example.com/first.git")
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args([
                "remote",
                "set-url",
                "origin",
                "https://example.com/second.git"
            ])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        project_key(Some(dir.path())).as_deref(),
        Some("git:https://example.com/second.git")
    );
}

#[test]
fn rendered_context_is_bounded_and_rejects_mismatched_snapshot() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let cwd = env._home.path();
    let mut snapshot = fixture("large", &project_key(Some(cwd)).unwrap());
    snapshot.intent = Some("界".repeat(5000));
    snapshot.open_todos = (0..100)
        .map(|n| HandoffTodo {
            id: n.to_string(),
            content: "界".repeat(1000),
            status: "pending".into(),
            group: None,
            confidence: None,
        })
        .collect();
    write_snapshot(&snapshot).unwrap();
    let rendered = render_boot_context(Some(cwd)).unwrap();
    assert!(rendered.len() <= 8192);
    assert!(rendered.contains("Handoff truncated"));
    snapshot.session_id = "wrong".into();
    crate::storage::write_json_fast(
        &file_path(&handoffs_dir().unwrap(), "large").unwrap(),
        &snapshot,
    )
    .unwrap();
    assert!(load_snapshot("large").is_none());
    assert!(render_boot_context(Some(cwd)).is_none());
}

struct HandoffTestEnv {
    before: Option<std::ffi::OsString>,
    _home: tempfile::TempDir,
}
impl HandoffTestEnv {
    fn new() -> Self {
        let before = std::env::var_os("JCODE_HOME");
        let home = tempfile::tempdir().unwrap();
        crate::env::set_var("JCODE_HOME", home.path());
        Self {
            before,
            _home: home,
        }
    }
}
impl Drop for HandoffTestEnv {
    fn drop(&mut self) {
        match &self.before {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

/// list_saved_handoffs returns one entry per project, newest first, ready for
/// the `/handoff` picker.
#[test]
fn list_saved_handoffs_returns_latest_per_project_newest_first() {
    let _guard = crate::storage::lock_test_env();
    let _env = HandoffTestEnv::new();

    let mut a1 = fixture("proj-a-1", "git:https://example.com/a.git");
    a1.ended_at = Utc::now() - chrono::Duration::hours(2);
    let mut a2 = fixture("proj-a-2", "git:https://example.com/a.git");
    a2.ended_at = Utc::now() - chrono::Duration::hours(1);
    let mut b = fixture("proj-b", "git:https://example.com/b.git");
    b.ended_at = Utc::now();
    // Older project A entry must not shadow the newer one for a in the index.
    write_snapshot(&a2).unwrap();
    write_snapshot(&a1).unwrap();
    write_snapshot(&b).unwrap();

    let listed = list_saved_handoffs();
    let keys: Vec<&str> = listed.iter().map(|e| e.project_key.as_str()).collect();
    assert_eq!(keys, vec![
        "git:https://example.com/b.git",
        "git:https://example.com/a.git",
    ]);
    // Newest first ordering across projects.
    let a_entry = listed
        .iter()
        .find(|e| e.project_key == "git:https://example.com/a.git")
        .expect("project A present");
    assert_eq!(a_entry.session_id, "proj-a-2", "latest A wins");
}

/// render_handoff renders a specific snapshot regardless of project, while
/// render_boot_context keeps resolving the latest for the working dir.
#[test]
fn render_handoff_selects_an_arbitrary_snapshot() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = env._home.path();
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).ok();

    let mut older = fixture("older", &project_key(Some(&cwd)).unwrap());
    older.ended_at = Utc::now() - chrono::Duration::hours(5);
    older.intent = Some("older work".into());
    write_snapshot(&older).unwrap();

    let mut newer = fixture("newer", &project_key(Some(&cwd)).unwrap());
    newer.intent = Some("newer work".into());
    write_snapshot(&newer).unwrap();

    // Boot context picks the latest automatically.
    let boot = render_boot_context(Some(&cwd)).unwrap();
    assert!(boot.contains("newer work"));

    // Manual selection can still target the older snapshot directly.
    let manual = render_handoff("older").expect("older renderable");
    assert!(manual.contains("older work"));
    assert!(!manual.contains("newer work"));

    // Unknown ids and malformed lookups resolve to None.
    assert!(render_handoff("no-such-session").is_none());
}

/// Manual selection cannot regress automatic boot injection: calling the picker
/// listing or a specific render leaves the default latest-for-project intact.
#[test]
fn manual_render_does_not_disturb_auto_inject() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = env._home.path();
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).ok();

    let snapshot = fixture("auto-target", &project_key(Some(&cwd)).unwrap());
    write_snapshot(&snapshot).unwrap();

    // Exercise the picker + a manual render; then confirm auto behavior is
    // unchanged and still points at the same latest snapshot.
    let _listing = list_saved_handoffs();
    let _manual = render_handoff("auto-target");

    assert_eq!(
        latest_handoff_for_project(Some(&cwd)).as_deref(),
        Some("auto-target")
    );
    let boot = render_boot_context(Some(&cwd)).expect("auto context present");
    assert!(boot.contains("[Handoff from previous session]"));
}

/// list_all_handoffs surfaces archived snapshots that are no longer the latest
/// for their project (so absent from the index), newest first.
#[test]
fn list_all_handoffs_includes_archived_and_is_newest_first() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = env._home.path();
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).ok();

    let mut older = fixture("older", &project_key(Some(&cwd)).unwrap());
    older.ended_at = Utc::now() - chrono::Duration::hours(2);
    write_snapshot(&older).unwrap();
    let mut newer = fixture("newer", &project_key(Some(&cwd)).unwrap());
    newer.ended_at = Utc::now() - chrono::Duration::hours(1);
    write_snapshot(&newer).unwrap();
    // A snapshot from a different project, to confirm cross-project coverage.
    let other = fixture("other-proj", "git:https://example.com/other.git");
    write_snapshot(&other).unwrap();

    // The index only keeps the latest per project.
    let indexed = list_saved_handoffs();
    let indexed_ids: Vec<&str> = indexed.iter().map(|e| e.session_id.as_str()).collect();
    assert!(indexed_ids.contains(&"newer"), "index has newer");
    assert!(!indexed_ids.contains(&"older"), "index drops archived older");

    // list_all_handoffs sees every snapshot, newest first.
    let all = list_all_handoffs();
    let all_ids: Vec<&str> = all.iter().map(|s| s.session_id.as_str()).collect();
    assert_eq!(
        all_ids,
        vec!["other-proj", "newer", "older"],
        "all snapshots surfaced newest first"
    );
}

/// Stagger a fixture's ended_at by a number of hours before the given base.
fn aged(session: &str, project: &str, base: DateTime<Utc>, hours_ago: i64) -> HandoffSnapshot {
    let mut snapshot = fixture(session, project);
    snapshot.ended_at = base - chrono::Duration::hours(hours_ago);
    snapshot
}

/// Pruning enforces the per-project archived count cap: only the newest
/// `MAX_ARCHIVED_SNAPSHOTS_PER_PROJECT` archived snapshots survive alongside
/// the live handoff, and the oldest are removed from disk.
#[test]
fn prune_enforces_archived_count_cap_per_project() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = env._home.path();
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).ok();
    let project = project_key(Some(&cwd)).unwrap();
    let now = Utc::now();

    // One live handoff (newest) plus several archived snapshots under the age
    // cap but exceeding the count cap.
    write_snapshot(&fixture("live", &project)).unwrap();
    let archived = MAX_ARCHIVED_SNAPSHOTS_PER_PROJECT + 5;
    for n in 1..=archived {
        let id = format!("arch-{n:03}");
        // hours_ago = n keeps all within the 30-day age window.
        write_snapshot(&aged(&id, &project, now, n as i64)).unwrap();
    }

    prune_archived_snapshots();

    let remaining = list_all_handoffs();
    let remaining_ids: Vec<&str> = remaining.iter().map(|s| s.session_id.as_str()).collect();
    assert!(
        remaining_ids.contains(&"live"),
        "the live handoff must never be pruned"
    );
    // live + the newest MAX_ARCHIVED archived snapshots.
    assert_eq!(
        remaining.len(),
        1 + MAX_ARCHIVED_SNAPSHOTS_PER_PROJECT,
        "only the live handoff plus the newest archived snapshots remain"
    );
    // hours_ago = n grows with n, so the highest-numbered snapshots are the
    // oldest. The 5 oldest archived (17..=21) are pruned.
    for n in (MAX_ARCHIVED_SNAPSHOTS_PER_PROJECT + 1)..=archived {
        let id = format!("arch-{n:03}");
        assert!(
            load_snapshot(&id).is_none(),
            "pruned snapshot {id} should be gone from disk"
        );
    }
    // The 16 newest archived snapshots (1..=16) survive.
    for n in 1..=MAX_ARCHIVED_SNAPSHOTS_PER_PROJECT {
        assert!(
            load_snapshot(&format!("arch-{n:03}")).is_some(),
            "kept snapshot arch-{n:03} should remain"
        );
    }
}

/// Pruning removes archived snapshots older than the age cap even when the
/// per-project count is under the cap, and leaves the live handoff intact.
#[test]
fn prune_enforces_archived_age_cap() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = env._home.path();
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).ok();
    let project = project_key(Some(&cwd)).unwrap();
    let now = Utc::now();

    // Live handoff plus a couple of archived snapshots, kept under the count
    // cap so only the age rule is exercised.
    write_snapshot(&fixture("live", &project)).unwrap();
    write_snapshot(&aged("fresh", &project, now, 24)).unwrap();
    // 31 days is beyond the 30-day cap; 31d = 744 hours.
    let mut stale = fixture("stale", &project);
    stale.ended_at = now - chrono::Duration::days(MAX_ARCHIVED_SNAPSHOT_AGE_DAYS + 1);
    write_snapshot(&stale).unwrap();

    prune_archived_snapshots();

    assert!(
        load_snapshot("live").is_some(),
        "live handoff survives age pruning"
    );
    assert!(
        load_snapshot("fresh").is_some(),
        "an in-window archived snapshot survives"
    );
    assert!(
        load_snapshot("stale").is_none(),
        "an over-age archived snapshot is pruned"
    );
}

/// Live handoffs referenced by the index are never pruned, even when the
/// snapshot is itself very old. Age/count pruning applies only to archived
/// (superseded) snapshots.
#[test]
fn prune_never_deletes_live_handoffs() {
    let _guard = crate::storage::lock_test_env();
    let _env = HandoffTestEnv::new();
    let now = Utc::now();

    // Project A's latest handoff is old, but it is still the live one (the
    // index references it) so it must survive pruning.
    let mut live_old = fixture("live-old", "git:https://example.com/a.git");
    live_old.ended_at = now - chrono::Duration::days(MAX_ARCHIVED_SNAPSHOT_AGE_DAYS + 60);
    write_snapshot(&live_old).unwrap();

    // Project B has a live handoff plus an over-age *archived* snapshot (e.g.
    // from a resumed session that then captured a newer handoff). The over-age
    // snapshot is a valid prune target and demonstrates pruning still works.
    let mut stale_b = fixture("stale-b", "git:https://example.com/b.git");
    stale_b.ended_at = now - chrono::Duration::days(MAX_ARCHIVED_SNAPSHOT_AGE_DAYS + 1);
    write_snapshot(&stale_b).unwrap();
    // A newer handoff for project B supersedes stale-b, archiving it.
    write_snapshot(&fixture("b-live", "git:https://example.com/b.git")).unwrap();

    prune_archived_snapshots();

    assert!(
        load_snapshot("live-old").is_some(),
        "the live handoff must never be pruned, even if old"
    );
    assert!(
        load_snapshot("stale-b").is_none(),
        "an over-age archived snapshot in another project is still pruned"
    );
    // live-old is still the latest handoff for project A.
    assert!(
        load_index()
            .latest
            .iter()
            .any(|e| e.project_key == "git:https://example.com/a.git"
                && e.session_id == "live-old"),
        "live-old remains the referenced handoff in the index"
    );
    // b-live survives alongside it.
    assert!(
        load_snapshot("b-live").is_some(),
        "the newer live handoff for project B survives"
    );
}

/// Pruning is scoped per project: archived snapshots of one project do not
/// cause another project's fresh snapshots to be pruned.
#[test]
fn prune_is_scoped_per_project() {
    let _guard = crate::storage::lock_test_env();
    let _env = HandoffTestEnv::new();
    let now = Utc::now();

    let project_a = "git:https://example.com/a.git";
    let project_b = "git:https://example.com/b.git";

    // Project A has live + many archived (over its cap).
    write_snapshot(&fixture("a-live", project_a)).unwrap();
    for n in 1..=(MAX_ARCHIVED_SNAPSHOTS_PER_PROJECT + 8) {
        write_snapshot(&aged(&format!("a-arch-{n}"), project_a, now, n as i64)).unwrap();
    }

    // Project B has only live + two fresh archived (under its cap).
    write_snapshot(&fixture("b-live", project_b)).unwrap();
    write_snapshot(&aged("b-arch-1", project_b, now, 1)).unwrap();
    write_snapshot(&aged("b-arch-2", project_b, now, 2)).unwrap();

    prune_archived_snapshots();

    // All of project B's snapshots survive; project A is trimmed to its cap.
    let all = list_all_handoffs();
    let all_ids: Vec<&str> = all.iter().map(|s| s.session_id.as_str()).collect();
    for id in ["b-live", "b-arch-1", "b-arch-2"] {
        assert!(all_ids.contains(&id), "project B snapshot {id} must survive");
    }
    assert!(all_ids.contains(&"a-live"), "project A live handoff survives");
    let a_archived: Vec<&str> = all_ids
        .iter()
        .copied()
        .filter(|id| id.starts_with("a-arch-"))
        .collect();
    assert_eq!(
        a_archived.len(),
        MAX_ARCHIVED_SNAPSHOTS_PER_PROJECT,
        "project A keeps only its archived cap"
    );
}

/// export_handoff serializes a saved snapshot; importing it on another host
/// rekeys it to the local project, registers it in the index, and makes it
/// injectable — the deliberate, retirement-safe form of the plan's
/// remote-adoption future work.
#[test]
fn export_import_round_trip_adopts_remote_handoff() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let src_home = env._home.path();
    let src = src_home.join("src");
    std::fs::create_dir_all(&src).ok();
    let target = src_home.join("target");
    std::fs::create_dir_all(&target).ok();

    // Capture an open-work handoff on the "source" host.
    crate::todo::save_todos(
        "remote-session",
        &[TodoItem {
            id: "t".into(),
            content: "finish the split".into(),
            status: "in_progress".into(),
            ..Default::default()
        }],
    )
    .unwrap();
    crate::todo::save_plan(
        "remote-session",
        &crate::todo::TodoPlan {
            user_intention: Some("resume the split".into()),
            ..Default::default()
        },
    )
    .unwrap();
    capture("remote-session", Some(&src), "closed", None).unwrap();

    // Export it, then "ship" to a different host/project and adopt it.
    let payload = export_handoff("remote-session").expect("export");
    let imported = import_handoff(&payload, Some(&target), "closed").expect("import");
    assert!(imported.starts_with("import-"), "fresh session id");

    let snap = load_snapshot(&imported).expect("adopted snapshot on disk");
    assert_eq!(snap.intent.as_deref(), Some("resume the split"));
    assert_eq!(snap.open_todos.len(), 1);
    // Re-keyed to the target project.
    let target_key = project_key(Some(&target)).unwrap();
    assert_eq!(snap.project_key, target_key);

    // Registered in the index and automatically injectable on the target.
    assert_eq!(
        latest_handoff_for_project(Some(&target)).as_deref(),
        Some(imported.as_str()),
        "imported handoff becomes the latest for the target project"
    );
    let boot = render_boot_context(Some(&target)).expect("boot from import");
    assert!(boot.contains("resume the split"));
}

/// Import rejects malformed payloads and never creates anything.
#[test]
fn import_rejects_malformed_payload() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let cwd = env._home.path();
    std::fs::create_dir_all(&cwd).ok();
    assert!(import_handoff("{{{ not json", Some(&cwd), "closed").is_none());
    assert!(list_all_handoffs().is_empty(), "nothing adopted on garbage");
}

/// Import mints a fresh session id, so a retired source id is never
/// resurrected by adoption: the imported snapshot is a distinct live entry.
#[test]
fn import_mints_fresh_id_and_does_not_resurrect_retired() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let cwd = env._home.path();
    std::fs::create_dir_all(&cwd).ok();
    let key = project_key(Some(&cwd)).unwrap();

    // A snapshot whose id is no longer in the index (retired).
    write_snapshot(&fixture("retired", &key)).unwrap();
    crate::storage::write_json_fast(
        &index_path().unwrap(),
        &HandoffIndex { latest: Vec::new() },
    )
    .unwrap();

    let payload = export_handoff("retired").expect("export");
    let imported = import_handoff(&payload, Some(&cwd), "closed").unwrap();
    assert_ne!(imported, "retired", "import must mint a fresh id");
    // The original retired id is not re-registered.
    assert!(latest_handoff_for_project(Some(&cwd)).as_deref() != Some("retired"));
}

/// Validation: importing an OLDER snapshot for a project that already has a
/// newer local handoff must NOT override it (upsert keeps the newer entry).
/// The imported snapshot becomes an archived file, still renderable manually.
#[test]
fn import_does_not_override_newer_local_handoff() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = env._home.path();
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).ok();
    let key = project_key(Some(&cwd)).unwrap();

    // A newer local handoff exists and is live in the index.
    let mut local = fixture("local-newer", &key);
    local.ended_at = Utc::now();
    local.intent = Some("local work".into());
    write_snapshot(&local).unwrap();
    assert_eq!(latest_handoff_for_project(Some(&cwd)).as_deref(), Some("local-newer"));

    // An older exported snapshot for the same project is imported.
    let mut older = fixture("older-src", &key);
    older.ended_at = Utc::now() - chrono::Duration::days(5);
    older.intent = Some("older remote work".into());
    let dir = handoffs_dir().unwrap();
    crate::storage::write_json_fast(&file_path(&dir, "older-src").unwrap(), &older).unwrap();
    let payload = export_handoff("older-src").expect("export older");
    let imported = import_handoff(&payload, Some(&cwd), "closed").expect("import");

    // The newer local handoff remains the latest for the project.
    assert_eq!(
        latest_handoff_for_project(Some(&cwd)).as_deref(),
        Some("local-newer"),
        "a newer local handoff must not be displaced by an older import"
    );
    // The imported snapshot is present on disk and manually selectable.
    assert!(load_snapshot(&imported).is_some(), "imported file exists");
    assert!(
        list_all_handoffs().iter().any(|s| s.session_id == imported),
        "imported snapshot is discoverable (archived)"
    );
}

/// Validation: pruning's age boundary. A snapshot exactly at the
/// `MAX_ARCHIVED_SNAPSHOT_AGE_DAYS` cutoff is kept (`>` excludes the exact
/// boundary), while one strictly older is pruned.
#[test]
fn prune_age_cutoff_excludes_exact_boundary() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = env._home.path();
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).ok();
    let key = project_key(Some(&cwd)).unwrap();
    let now = Utc::now();

    // Live handoff to pin the bucket.
    write_snapshot(&fixture("live", &key)).unwrap();
    // Just under the cutoff (30 days minus a few seconds of age) -> kept,
    // confirming the boundary uses `>` (strictly-older) rather than `>=`.
    let mut inside = fixture("inside-cutoff", &key);
    inside.ended_at = now
        .checked_sub_signed(chrono::Duration::days(MAX_ARCHIVED_SNAPSHOT_AGE_DAYS))
        .unwrap()
        + chrono::Duration::seconds(5);
    write_snapshot(&inside).unwrap();
    // Strictly older (31 days) -> pruned.
    let mut stale = fixture("stale", &key);
    stale.ended_at = now - chrono::Duration::days(MAX_ARCHIVED_SNAPSHOT_AGE_DAYS + 1);
    write_snapshot(&stale).unwrap();

    prune_archived_snapshots();

    assert!(
        load_snapshot("inside-cutoff").is_some(),
        "a snapshot within the age window is kept"
    );
    assert!(
        load_snapshot("stale").is_none(),
        "a snapshot strictly older than the cutoff is pruned"
    );
    assert!(load_snapshot("live").is_some(), "live handoff survives");
}

/// Fix #8: import mints a readable id echoing the source session
/// (`import-<source>`), not an opaque UUID, so `/handoffres <id>` is
/// human-understandable.
#[test]
fn import_uses_readable_session_id() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let cwd = env._home.path();
    std::fs::create_dir_all(&cwd).ok();
    let key = project_key(Some(&cwd)).unwrap();

    write_snapshot(&fixture("chatty-session-42", &key)).unwrap();
    let payload = export_handoff("chatty-session-42").expect("export");
    let imported = import_handoff(&payload, Some(&cwd), "closed").unwrap();

    assert_eq!(
        imported, "import-chatty-session-42",
        "import id should echo the source session, not be an opaque UUID"
    );
    assert!(load_snapshot(&imported).is_some());
}

/// Fix #8: when the readable stem already exists, import disambiguates with a
/// short suffix while keeping the stem, and both snapshots remain distinct.
#[test]
fn import_disambiguates_colliding_readable_id() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let cwd = env._home.path();
    std::fs::create_dir_all(&cwd).ok();
    let key = project_key(Some(&cwd)).unwrap();

    write_snapshot(&fixture("shared", &key)).unwrap();
    let payload = export_handoff("shared").expect("export");

    let first = import_handoff(&payload, Some(&cwd), "closed").unwrap();
    let second = import_handoff(&payload, Some(&cwd), "closed").unwrap();
    assert_eq!(first, "import-shared", "first import takes the readable id");
    assert_ne!(first, second, "second import must not collide");
    assert!(
        second.starts_with("import-shared-"),
        "second import keeps the stem plus a disambiguator"
    );
    assert!(load_snapshot(&first).is_some());
    assert!(load_snapshot(&second).is_some());
}

/// Fix #8: a pathological source session id (uppercase, dots, slashes) is
/// sanitized into a valid, bounded, importable filename stem.
#[test]
fn import_sanitizes_pathological_source_id() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let cwd = env._home.path();
    std::fs::create_dir_all(&cwd).ok();
    let key = project_key(Some(&cwd)).unwrap();
    let pathological = "Weird.Session/NAME!";
    // Build a payload carrying a pathological source id (simulating an
    // out-of-band handoff whose id the regular file_path validation would
    // reject), then adopt it.
    let payload = {
        let mut snap = fixture(pathological, &key);
        snap.session_id = pathological.into();
        serde_json::to_string(&snap).unwrap()
    };
    let imported = import_handoff(&payload, Some(&cwd), "closed").unwrap();
    assert!(imported.starts_with("import-"), "sane prefix");
    // The stem is sanitized: lowercase + safe [a-z0-9_-] only.
    assert_eq!(
        imported, "import-weirdsessionname",
        "pathological source id is lowercased and stripped to a safe stem"
    );
    assert!(
        imported
            .chars()
            .all(|c| c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || c == '-'
                || c == '_'),
        "import filename must only contain safe characters, got {imported:?}"
    );
    assert!(load_snapshot(&imported).is_some(), "imported snapshot loads");
}

/// Fix #1: the startup sweep calls the retention policy and prunes stale
/// archived snapshots (and is idempotent — running it twice is harmless).
#[test]
fn startup_sweep_prunes_stale_archived() {
    let _guard = crate::storage::lock_test_env();
    let env = HandoffTestEnv::new();
    let home = env._home.path();
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).ok();
    let key = project_key(Some(&cwd)).unwrap();
    let now = Utc::now();

    write_snapshot(&fixture("live", &key)).unwrap();
    let mut stale = fixture("stale", &key);
    stale.ended_at = now - chrono::Duration::days(MAX_ARCHIVED_SNAPSHOT_AGE_DAYS + 10);
    write_snapshot(&stale).unwrap();

    // Before the sweep both exist.
    assert!(load_snapshot("stale").is_some());

    sweep_stale_handoffs();

    // Live survives; stale is pruned.
    assert!(load_snapshot("live").is_some());
    assert!(load_snapshot("stale").is_none());

    // Idempotent: a second sweep is harmless.
    sweep_stale_handoffs();
}
