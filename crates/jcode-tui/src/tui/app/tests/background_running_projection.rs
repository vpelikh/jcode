// End-to-end integration of takeaway #10's session/background-running
// projection: a real background task spawned via the manager's public `spawn`
// must show up as the focused session's `background_info.running_count`
// through the real `TuiState::info_widget_data` public interface, and must NOT
// leak into a different session's count.

#[test]
fn focused_session_background_count_reflects_only_that_sessions_live_tasks() {
    with_temp_jcode_home(|| {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let manager = crate::background::global();

        // A real background task that stays alive (pending) so it is Running in
        // the global manager and its status file. `spawn` takes a closure over
        // `Result<TaskResult>`, so no un-reachable ToolOutput type is needed.
        let spawn_stay_alive = |tool: &'static str, session: &'static str| {
            let info = rt.block_on(manager.spawn(tool, session, |_output_path| async move {
                std::future::pending::<Result<crate::background::TaskResult>>().await
            }));
            info
        };
        let a = spawn_stay_alive("bash", "it-proj-session-A");
        let b = spawn_stay_alive("read", "it-proj-session-B");

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

        // Clean up so the global singleton does not leak tasks across tests.
        rt.block_on(async {
            let _ = manager.cancel(&a.task_id).await;
            let _ = manager.cancel(&b.task_id).await;
        });
    });
}