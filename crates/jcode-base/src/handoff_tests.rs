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
