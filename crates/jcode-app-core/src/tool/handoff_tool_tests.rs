#![cfg_attr(test, allow(clippy::await_holding_lock))]

use super::*;

/// The `handoff` tool saves a snapshot carrying the continuation prompt, and a
/// later boot renders that task so the resumed session is ready to do it.
#[tokio::test]
async fn handoff_tool_saves_continuation_prompt_and_boot_renders_it() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("repo");
    std::fs::create_dir_all(&project).expect("project dir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let tool = HandoffTool::new();
    let ctx = ToolContext {
        session_id: "ses_handoff_tool".to_string(),
        message_id: "msg1".to_string(),
        tool_call_id: "tool1".to_string(),
        working_dir: Some(project.clone()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
    };

    let out = tool
        .execute(
            json!({"action": "save", "prompt": "review this branch's changes"}),
            ctx.clone(),
        )
        .await
        .expect("handoff tool runs");
    assert!(
        out.output.contains("review this branch's changes"),
        "tool output should echo the continuation task, got: {}",
        out.output
    );

    let snapshot = crate::handoff::load_snapshot("ses_handoff_tool").expect("snapshot saved");
    assert_eq!(snapshot.disposition, "saved");
    assert_eq!(
        snapshot.continuation_prompt.as_deref(),
        Some("review this branch's changes")
    );

    // A later session in the same project boots with the task.
    let boot = crate::handoff::render_boot_context(Some(&project)).expect("boot context");
    assert!(
        boot.contains("Continue with this task: review this branch's changes"),
        "boot context should carry the continuation task, got: {boot}"
    );

    match prev_home {
        Some(prev) => crate::env::set_var("JCODE_HOME", prev),
        None => crate::env::remove_var("JCODE_HOME"),
    }
}

/// Without open todos and without a prompt, the tool reports nothing to save
/// rather than writing a useless empty snapshot.
#[tokio::test]
async fn handoff_tool_reports_nothing_to_save_without_work_or_prompt() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("repo");
    std::fs::create_dir_all(&project).expect("project dir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let tool = HandoffTool::new();
    let ctx = ToolContext {
        session_id: "ses_handoff_empty".to_string(),
        message_id: "msg1".to_string(),
        tool_call_id: "tool1".to_string(),
        working_dir: Some(project.clone()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
    };

    let out = tool
        .execute(json!({"action": "save"}), ctx)
        .await
        .expect("handoff tool runs");
    assert!(
        out.output.contains("Nothing to save"),
        "empty save should report nothing to save, got: {}",
        out.output
    );
    assert!(crate::handoff::load_snapshot("ses_handoff_empty").is_none());

    match prev_home {
        Some(prev) => crate::env::set_var("JCODE_HOME", prev),
        None => crate::env::remove_var("JCODE_HOME"),
    }
}

/// `action: clear` drops the saved task and reports it.
#[tokio::test]
async fn handoff_tool_clears_the_saved_task() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("repo");
    std::fs::create_dir_all(&project).expect("project dir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let tool = HandoffTool::new();
    let ctx = ToolContext {
        session_id: "ses_handoff_clear".to_string(),
        message_id: "msg1".to_string(),
        tool_call_id: "tool1".to_string(),
        working_dir: Some(project.clone()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
    };

    tool.execute(
        json!({"action": "save", "prompt": "review this branch"}),
        ctx.clone(),
    )
    .await
    .expect("save");
    assert!(
        crate::handoff::load_snapshot("ses_handoff_clear")
            .expect("saved")
            .continuation_prompt
            .is_some()
    );

    let out = tool
        .execute(json!({"action": "clear"}), ctx.clone())
        .await
        .expect("clear runs");
    assert!(
        out.output.contains("Cleared the saved continuation task"),
        "clear should report the task was cleared, got: {}",
        out.output
    );
    // A prompt-only snapshot is removed entirely by the clear.
    assert!(crate::handoff::load_snapshot("ses_handoff_clear").is_none());

    // Clearing again is a no-op, not an error.
    let again = tool
        .execute(json!({"action": "clear"}), ctx)
        .await
        .expect("clear again runs");
    assert!(again.output.contains("No saved handoff"));

    match prev_home {
        Some(prev) => crate::env::set_var("JCODE_HOME", prev),
        None => crate::env::remove_var("JCODE_HOME"),
    }
}

/// An unknown action is rejected rather than silently treated as `save`.
#[tokio::test]
async fn handoff_tool_rejects_unsupported_action() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let tool = HandoffTool::new();
    let ctx = ToolContext {
        session_id: "ses_handoff_bad".to_string(),
        message_id: "msg1".to_string(),
        tool_call_id: "tool1".to_string(),
        working_dir: Some(temp.path().to_path_buf()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
    };

    let err = tool
        .execute(json!({"action": "delete"}), ctx)
        .await
        .expect_err("unsupported action should error");
    assert!(
        err.to_string().contains("supported actions are `save` and `clear`"),
        "got: {err}"
    );

    match prev_home {
        Some(prev) => crate::env::set_var("JCODE_HOME", prev),
        None => crate::env::remove_var("JCODE_HOME"),
    }
}
