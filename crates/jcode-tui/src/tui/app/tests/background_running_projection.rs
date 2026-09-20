// End-to-end integration of takeaway #10's session/background-running
// projection: a real background task spawned via the manager's public `spawn`
// must show up as the focused session's `background_info.running_count`
// through the real `TuiState::info_widget_data` public interface, and must NOT
// leak into a different session's count.

use std::path::PathBuf;

/// Drops the background task's on-disk status/output files so a test failure
/// (panic) cannot leak orphaned files into the shared global task dir. The
/// pending tokio tasks themselves are killed when the test's runtime drops.
/// `Drop` runs even on panic, so the files are always cleaned up.
struct TaskFileCleanup {
    files: Vec<PathBuf>,
}

impl TaskFileCleanup {
    fn track(&mut self, info: &crate::background::BackgroundTaskInfo) {
        self.files.push(info.status_file.clone());
        self.files.push(info.output_file.clone());
    }
}

impl Drop for TaskFileCleanup {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = std::fs::remove_file(file);
        }
    }
}

#[test]
fn focused_session_background_count_reflects_only_that_sessions_live_tasks() {
    with_temp_jcode_home(|| {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let manager = crate::background::global();
        let mut cleanup = TaskFileCleanup { files: Vec::new() };

        // A real background task that stays alive (pending) so it is Running in
        // the global manager and its status file. `spawn` takes a closure over
        // `Result<TaskResult>`, so no un-reachable ToolOutput type is needed.
        let spawn_stay_alive = |tool: &'static str, session: &'static str, cleanup: &mut TaskFileCleanup| {
            let info = rt.block_on(manager.spawn(tool, session, |_output_path| async move {
                std::future::pending::<Result<crate::background::TaskResult>>().await
            }));
            cleanup.track(&info);
            info
        };
        let a = spawn_stay_alive("bash", "it-proj-session-A", &mut cleanup);
        let b = spawn_stay_alive("read", "it-proj-session-B", &mut cleanup);

        // A local (non-remote) app with session.id = A.
        let mut app = create_test_app();
        app.is_remote = false;
        app.session.id = "it-proj-session-A".to_string();

        let data = crate::tui::TuiState::info_widget_data(&app);
        let info = data
            .background_info
            .expect("a running task must surface background_info");
        assert_eq!(
            info.running_count, 1,
            "focused session A must show exactly its one live task, not session B's"
        );
        assert_eq!(
            info.running_tasks,
            vec!["bash".to_string()],
            "session A's live task is the spawn tool name"
        );

        // Flip focus to session B: now B's task (and only B's) should show.
        app.session.id = "it-proj-session-B".to_string();
        let data = crate::tui::TuiState::info_widget_data(&app);
        let info = data
            .background_info
            .expect("session B has a live task");
        assert_eq!(
            info.running_count, 1,
            "focused session B must surface exactly its one live task, not session A's"
        );
        assert_eq!(
            info.running_tasks,
            vec!["read".to_string()],
            "session B's live task is the spawn tool name"
        );

        // A session with no background work must show no background_info.
        app.session.id = "it-proj-session-idle".to_string();
        let data = crate::tui::TuiState::info_widget_data(&app);
        assert!(
            data.background_info.is_none(),
            "a session with no live tasks must not show a background indicator"
        );

        // A remote client with no resolved session id (session_id == None) must
        // also show no background indicator — even though global background
        // tasks exist — rather than falling back to a misleading global count.
        app.is_remote = true;
        app.remote_session_id = None;
        let data = crate::tui::TuiState::info_widget_data(&app);
        assert!(
            data.background_info.is_none(),
            "a remote client with no session id must not show a global background count"
        );
        app.is_remote = false;

        // Clean up: cancel the tasks (normal completion path), which also stops
        // the pending futures. The TaskFileCleanup guard additionally removes
        // any on-disk files even if this cleanup or an earlier assert panics.
        let _ = rt.block_on(async {
            let _ = manager.cancel(&a.task_id).await;
            let _ = manager.cancel(&b.task_id).await;
        });
    });
}