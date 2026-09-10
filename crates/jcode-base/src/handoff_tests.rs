use super::*;

/// project_key prefers the git remote URL for portability across machines.
#[test]
fn project_key_prefers_git_remote() {
    let timeout = std::time::Duration::from_secs(3);
    if std::process::Command::new("git")
        .args(["--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
        != true
    {
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
        .args(["remote", "add", "origin", "https://example.com/acme/widget.git"])
        .output()
        .ok();
    let key = project_key(Some(dir.path()));
    // Give git a moment to have written the config synchronously (it has).
    assert_eq!(
        key.as_deref(),
        Some("git:https://example.com/acme/widget.git"),
        "project key should be the git origin URL"
    );
    let _ = timeout;
}

/// Fallback: a file-less path hashes to the path form even without git.
#[test]
fn project_key_falls_back_to_path() {
    let dir = std::env::temp_dir().join(format!("jcode-handoff-u{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let key = project_key(Some(&dir));
    let key = key.expect("path fallback should yield a key");
    assert!(key.starts_with("path:"), "expected path: prefix, got {key}");
    let _ = std::fs::remove_dir_all(&dir);
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
    let home = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", home.path());
    let cwd = std::env::temp_dir().join("jcode-capture-test");
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

    crate::env::remove_var("JCODE_HOME");
}

/// build_snapshot filters out completed/cancelled todos.
#[test]
fn open_filter_drops_completed_and_cancelled() {
    let _guard = crate::storage::lock_test_env();
    let dir = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir.path());
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
    crate::env::remove_var("JCODE_HOME");
}

/// promote_to_initiative creates a project goal from a handoff snapshot.
#[tokio::test]
async fn promote_to_initiative_creates_a_goal() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", home.path());
    let cwd = std::env::temp_dir().join("jcode-promote-test");
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

    crate::env::remove_var("JCODE_HOME");
}

/// render_boot_context emits a compact block when a handoff exists.
#[tokio::test]
async fn render_boot_context_produces_block() {
    let _guard = crate::storage::lock_test_env();
    let home = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", home.path());
    let cwd = std::env::temp_dir().join("jcode-boot-test");
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
    crate::env::remove_var("JCODE_HOME");
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
    if !std::process::Command::new("git")
        .args(["--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
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
    let home = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", home.path());
    let cwd = std::env::temp_dir().join("jcode-corrupt-index");
    std::fs::create_dir_all(&cwd).ok();

    // Write garbage over the index path.
    let dir = crate::storage::jcode_dir().expect("jcode dir").join("handoffs");
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
    crate::env::remove_var("JCODE_HOME");
}