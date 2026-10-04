// Tests for `/review-loop` typing suggestions, help, and dispatch.
//
// `/review-loop` was previously dispatchable but produced no suggestion while
// typing (it was absent from the suggestion registry). These pin the observable
// UX: typing `/review-loop` must offer `status` / `start` / `stop` completions
// (including partial-input narrowing), `/help review-loop` (and the `/?`
// alias / case-insensitive forms) must show them, and submitting
// `/review-loop start|stop` must actually drive the local review-loop handler.
// (Registry presence is asserted in `state_ui_input_helpers.rs` alongside the
// other registrations.)

#[test]
fn typing_review_loop_suggests_subcommands() {
    let mut app = create_test_app();

    // Bare `/review-loop` offers all three subcommands.
    app.input = "/review-loop".to_string();
    app.cursor_pos = app.input.len();
    let suggestions = app.command_suggestions();
    let commands: std::collections::HashSet<&str> = suggestions
        .iter()
        .map(|(cmd, _)| cmd.as_str())
        .collect();
    for expected in [
        "/review-loop start",
        "/review-loop run",
        "/review-loop stop",
        "/review-loop status",
    ] {
        assert!(
            commands.contains(expected),
            "bare /review-loop must offer {expected}, got {:?}",
            suggestions
        );
    }

    // A typed subcommand narrows to the matching completion (same as /autoreview).
    app.input = "/review-loop status".to_string();
    app.cursor_pos = app.input.len();
    let suggestions = app.command_suggestions();
    assert_eq!(
        suggestions,
        vec![("/review-loop status".to_string(), "Show current review loop status")],
        "typing /review-loop status must narrow to the matching completion, got {:?}",
        suggestions
    );
}

#[test]
fn review_loop_suggestions_do_not_collide_with_one_shot_review() {
    // `/review-loop` must not be swallowed by the `/review` prefix handlers that
    // offer the "Launch a one-shot review" suggestion, and vice versa.
    let mut app = create_test_app();

    app.input = "/review".to_string();
    app.cursor_pos = app.input.len();
    let review_suggestions = app.command_suggestions();
    assert!(
        review_suggestions
            .iter()
            .any(|(cmd, _)| cmd == "/review"),
        "typing /review must still offer the one-shot /review completion, got {:?}",
        review_suggestions
    );

    app.input = "/review-loop".to_string();
    app.cursor_pos = app.input.len();
    let loop_suggestions = app.command_suggestions();
    assert!(
        loop_suggestions
            .iter()
            .any(|(cmd, _)| cmd == "/review-loop start"),
        "typing /review-loop must offer loop subcommands, got {:?}",
        loop_suggestions
    );
}

#[test]
fn help_review_loop_topic_shows_loop_details() {
    // `/review-loop` is a public registered command, so it is both suggested by
    // the `/help <topic>` completions and listed in the palette. Selecting it
    // must not fall through to an "Unknown command" error, so it needs a help
    // topic.
    let mut app = create_test_app();
    app.input = "/help review-loop".to_string();
    app.submit_input();

    let msg = app
        .display_messages()
        .last()
        .expect("missing help response");
    assert_eq!(msg.role, "system");
    assert!(msg.content.contains("/review-loop"));
    assert!(msg.content.contains("/review-loop status"));
    assert!(msg.content.contains("/review-loop stop"));
}

#[test]
fn review_loop_start_command_seeds_loop() {
    // Submitting `/review-loop start` must dispatch to the local command handler
    // and seed a runnable review loop on the session (first lens set, not
    // finished). This pins the end-to-end wiring that the suggestion + help
    // surfaces advertise.
    let mut app = create_test_app();
    app.input = "/review-loop start".to_string();
    app.submit_input();

    let state = app
        .session
        .review_loop
        .as_ref()
        .expect("submitting /review-loop start must seed session.review_loop");
    assert!(!state.finished);
    assert_eq!(
        state.current_lens,
        Some(jcode_session_types::ReviewLens::Correctness),
        "seeded loop must begin at the first lens"
    );
}

#[test]
fn review_loop_stop_marks_finished() {
    let mut app = create_test_app();
    // Seed a running loop first.
    app.input = "/review-loop start".to_string();
    app.submit_input();
    assert!(
        app.session.review_loop.as_ref().is_some_and(|s| !s.finished),
        "review loop should be active after start"
    );

    // Stop it.
    app.input = "/review-loop stop".to_string();
    app.submit_input();
    let state = app
        .session
        .review_loop
        .as_ref()
        .expect("loop state still present after stop");
    assert!(state.finished);
    assert_eq!(state.finish_reason.as_deref(), Some("user_stopped"));
}

#[test]
fn help_review_loop_aliases_and_case_insensitive() {
    // `/help` shorthand `/?` and upper-cased topics must resolve the review-loop
    // help arm the same way (`/? review-loop`, `/help REVIEW-LOOP`), instead of
    // falling through to an "Unknown command" error.
    let mut app = create_test_app();
    for input in ["/? review-loop", "/help REVIEW-LOOP", "/? REVIEW-LOOP"] {
        app.input = input.to_string();
        app.submit_input();
        let msg = app.display_messages().last().expect("help response");
        assert_eq!(msg.role, "system", "input {input} must show help not an error");
        assert!(
            !msg.content.contains("Unknown command"),
            "input {input} must not be Unknown command, got {:?}",
            msg.content
        );
        assert!(msg.content.contains("/review-loop"));
    }
}

#[test]
fn review_loop_suggestion_partial_narrowing() {
    // Typing a partial subcommand (e.g. `/review-loop s`) must narrow to the
    // matching completions (start/stop), not drop to a single stale suggestion.
    let mut app = create_test_app();
    app.input = "/review-loop s".to_string();
    app.cursor_pos = app.input.len();
    let suggestions = app.command_suggestions();
    let cmds: Vec<&str> = suggestions.iter().map(|(c, _)| c.as_str()).collect();
    assert!(
        cmds.contains(&"/review-loop start"),
        "partial 's' must offer start, got {:?}", cmds
    );
    assert!(
        cmds.contains(&"/review-loop stop"),
        "partial 's' must offer stop, got {:?}", cmds
    );
}

#[test]
fn review_loop_status_reports_active_and_no_loop() {
    // `/review-loop status` must report gracefully both when no loop exists and
    // when one is active (showing the current lens), never an error.
    let mut app = create_test_app();

    // No loop yet: status reports there is none.
    app.input = "/review-loop status".to_string();
    app.submit_input();
    let msg = app.display_messages().last().expect("status response");
    assert!(
        msg.content.contains("No review loop"),
        "no-loop status must say none, got {:?}",
        msg.content
    );

    // Start, then status shows the active loop / first lens.
    app.input = "/review-loop start".to_string();
    app.submit_input();
    app.input = "/review-loop status".to_string();
    app.submit_input();
    let msg = app.display_messages().last().expect("status response");
    assert!(
        msg.content.contains("Review loop active: lens 1/6 · Correctness · review pass"),
        "active status must show the lens progress, got {:?}",
        msg.content
    );
}

// The durable status line must expose loop progress while a loop is active
// (not only a 3s-transient notice) and clear once it finishes, so the user can
// always tell the loop is running and how far it has gotten. This asserts the
// *rendered* notification row, so it would catch the segment being present in
// the data but gated out of the row by `has_notification`.
#[test]
fn review_loop_status_is_exposed_durably_while_active() {
    use crate::tui::TuiState;

    let mut app = create_test_app();

    // No loop: no durable status segment, and no notification row reserved.
    assert!(
        TuiState::review_loop_status(&app).is_none(),
        "no active loop must expose no status"
    );
    assert!(
        !TuiState::has_notification(&app),
        "no active loop must not reserve the notification row"
    );

    // Start a loop: the durable status shows the first lens progress AND the
    // row is reserved/rendered.
    app.input = "/review-loop start".to_string();
    app.submit_input();
    // Drop the transient "started" notice so this test exercises the *durable*
    // segment alone (the notice alone would reserve the row for its first 3s
    // and mask whether the durable path is wired).
    app.status_notice = None;
    let status = TuiState::review_loop_status(&app).expect("active loop status");
    assert!(
        status.contains("lens 1/6 · Correctness · review pass"),
        "durable status must show progress, got {status:?}"
    );
    assert!(
        TuiState::status_notice(&app).is_none(),
        "the transient notice must be gone for this assertion to be meaningful"
    );
    assert!(
        TuiState::has_notification(&app),
        "an active loop alone must reserve the notification row"
    );
    let rendered = crate::tui::ui::notification_row_text_for_tests(&app);
    assert!(
        rendered.contains("review") && rendered.contains("lens 1/6"),
        "the rendered notification row must contain the progress segment, got {rendered:?}"
    );

    // Finished loop: the durable status clears and the row is freed.
    app.session.review_loop.as_mut().unwrap().finish_with("converged");
    assert!(
        TuiState::review_loop_status(&app).is_none(),
        "a finished loop must not keep the status segment"
    );
}

// Replay and video-export restore a saved session's `review_loop` (possibly
// unfinished) but never run the loop, so the durable segment must not render a
// fake "reviewing" status for the whole playback.
#[test]
fn review_loop_status_is_suppressed_in_replay() {
    use crate::tui::TuiState;

    let mut app = create_test_app();
    let mut state = jcode_session_types::ReviewLoopState::new();
    super::review_loop::enter_review_loop(&mut state);
    app.session.review_loop = Some(state);

    // Sanity: it renders while not replaying.
    assert!(
        TuiState::review_loop_status(&app).is_some(),
        "an unfinished loop must render a status when not replaying"
    );

    app.is_replay = true;
    assert!(
        TuiState::review_loop_status(&app).is_none(),
        "replay must not render the durable review segment"
    );
    assert!(
        !TuiState::has_notification(&app),
        "replay must not reserve the notification row for a restored loop"
    );
}

#[test]
fn review_loop_start_restarts_after_finished() {
    // The help advertises "/review-loop start: Start (or restart)". A finished
    // loop must restart from the first lens when start is re-submitted.
    let mut app = create_test_app();
    app.input = "/review-loop start".to_string();
    app.submit_input();

    // Finish it (simulate convergence by marking the persisted state).
    app.session.review_loop.as_mut().unwrap().finish_with("converged");
    assert!(app.session.review_loop.as_ref().unwrap().finished);

    // Restart.
    app.input = "/review-loop start".to_string();
    app.submit_input();
    let state = app.session.review_loop.as_ref().unwrap();
    assert!(!state.finished, "start must restart a finished loop");
    assert_eq!(
        state.current_lens,
        Some(jcode_session_types::ReviewLens::Correctness),
        "restarted loop must begin at the first lens"
    );
}

#[test]
fn review_loop_run_alias_starts_loop() {
    // `/review-loop run` is a silent-but-valid alias for `/review-loop start`
    // (accepted by the handler). Now that it is advertised in suggestions and
    // help, it must actually dispatch and seed a runnable loop.
    let mut app = create_test_app();
    app.input = "/review-loop run".to_string();
    app.submit_input();

    let state = app
        .session
        .review_loop
        .as_ref()
        .expect("submitting /review-loop run must seed session.review_loop");
    assert!(!state.finished);
    assert_eq!(
        state.current_lens,
        Some(jcode_session_types::ReviewLens::Correctness),
        "/review-loop run must begin at the first lens"
    );
}

#[test]
fn review_loop_start_clears_improve_mode() {
    // Mutual exclusion: only one loop-mode per session. Starting the review
    // loop must clear an active improve/refactor mode.
    let mut app = create_test_app();
    app.improve_mode = Some(ImproveMode::ImproveRun);
    app.session.improve_mode = Some(crate::session::SessionImproveMode::ImproveRun);

    app.input = "/review-loop start".to_string();
    app.submit_input();

    assert!(
        app.improve_mode.is_none(),
        "starting review loop must clear app.improve_mode"
    );
    assert!(
        app.session.improve_mode.is_none(),
        "starting review loop must clear session.improve_mode"
    );
    assert!(
        app.session.review_loop.as_ref().is_some_and(|s| !s.finished),
        "review loop must be active after start"
    );
}

#[test]
fn review_loop_manual_start_clears_stale_reviewer() {
    // A manual `/review-loop start` must not keep polling a stale in-flight
    // reviewer id from a previous run/lens. It matches the auto-entry path
    // (maybe_enter_review_loop) which clears active_reviewer_id after seeding.
    let mut app = create_test_app();
    app.input = "/review-loop start".to_string();
    app.submit_input();
    // Simulate an in-flight reviewer from a prior lens.
    app.session.review_loop.as_mut().unwrap().active_reviewer_id =
        Some("stale-reviewer".to_string());

    // Re-start manually.
    app.input = "/review-loop start".to_string();
    app.submit_input();
    let state = app.session.review_loop.as_ref().unwrap();
    assert_eq!(
        state.active_reviewer_id, None,
        "manual start must clear a stale active_reviewer_id"
    );
    assert_eq!(
        state.current_lens,
        Some(jcode_session_types::ReviewLens::Correctness),
        "manual restart must reseed from the first lens"
    );
}

// The auto-seed path (`maybe_enter_review_loop`) is the product entry point when
// review-rounds run by default. The `TestHarness` guard normally skips it in
// unit tests, so this pins the gating decisions directly: default-on config
// seeds the loop for a non-harness, local, non-replay session; the harness and
// remote guards each prevent seeding.
#[test]
fn review_loop_auto_seed_respects_defaults_and_guards() {
    use super::AppRuntimeMode;

    // This test asserts loop_mode is ON, which lives in the process-global
    // config. Pin it explicitly (and serialize with other config-mutating
    // tests) so an ambient/temporary `loop_mode = false` from another test
    // cannot make this spuriously fail.
    let _guard = crate::storage::lock_test_env();
    // RAII restore: a panic mid-test must not leave the override set for
    // sibling tests sharing this process.
    let _loop_mode = EnvGuard::set("JCODE_AUTOREVIEW_LOOP_MODE", "true");
    crate::config::invalidate_config_cache();

    let fresh = || {
        let mut app = create_test_app();
        // Non-harness product path (the loop runs for local sessions).
        app.runtime_mode = AppRuntimeMode::RemoteClient;
        app.is_remote = false;
        app.is_replay = false;
        app.autoreview_enabled = true; // resolved from the enabled default
        app.pending_queued_dispatch = false;
        app.improve_mode = None;
        app.session.review_loop = None;
        app
    };

    // (1) Default-on config seeds the loop for a product local session.
    let mut app = fresh();
    let msgs_before = app.display_messages.len();
    super::commands::maybe_enter_review_loop(&mut app);
    assert!(
        app.session.review_loop.as_ref().is_some_and(|s| !s.finished),
        "default-on config must auto-seed the review loop"
    );
    assert!(
        app.display_messages.len() > msgs_before,
        "seeding must push a review-loop status message"
    );

    // (2) Seeding is once per session: a second call must not restart it.
    let len_before = app.display_messages.len();
    super::commands::maybe_enter_review_loop(&mut app);
    assert_eq!(
        app.display_messages.len(),
        len_before,
        "auto-seed must not run twice for the same session"
    );

    // (3) Harness mode never seeds (keeps tests deterministic).
    let mut app = fresh();
    app.runtime_mode = AppRuntimeMode::TestHarness;
    super::commands::maybe_enter_review_loop(&mut app);
    assert!(
        app.session.review_loop.is_none(),
        "TestHarness runtime must not auto-seed"
    );

    // (4) The normal product TUI is a remote server-client: it MUST auto-seed
    //     too (matching the already-working manual `/review-loop`), so review
    //     rounds run by default in the main client.
    let mut app = fresh();
    app.is_remote = true;
    app.runtime_mode = AppRuntimeMode::RemoteClient;
    super::commands::maybe_enter_review_loop(&mut app);
    assert!(
        app.session.review_loop.as_ref().is_some_and(|s| !s.finished),
        "remote client (normal TUI) must auto-seed the review loop"
    );

    // (5) Replay sessions never auto-seed (deterministic playback).
    let mut app = fresh();
    app.is_replay = true;
    app.runtime_mode = AppRuntimeMode::Replay;
    super::commands::maybe_enter_review_loop(&mut app);
    assert!(
        app.session.review_loop.is_none(),
        "replay session must not auto-seed"
    );
}

// Acceptance: the real config file is the source of truth. A `[autoreview]
// loop_mode = false` on disk must prevent auto-seeding (this is the exact state
// that produced the user's report of "review rounds never run and I see no
// logs"), and adding an env override in the same process must flip it back on.
// Exercised through `maybe_enter_review_loop`, the product entry point.
#[test]
fn review_loop_auto_seed_is_gated_by_the_config_file_loop_mode() {
    use super::AppRuntimeMode;

    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    // RAII restore for both globals: a panic mid-test must not leak a
    // JCODE_HOME pointing at a deleted tempdir or a loop-mode override into
    // sibling tests sharing this process. Drop order (reverse of declaration)
    // restores env before `temp` is removed.
    let _home = EnvGuard::set("JCODE_HOME", temp.path());
    let _loop_mode = EnvGuard::remove("JCODE_AUTOREVIEW_LOOP_MODE");
    crate::config::invalidate_config_cache();

    let config_path = crate::config::Config::path().expect("config path");
    std::fs::create_dir_all(config_path.parent().expect("config parent"))
        .expect("create config parent");
    std::fs::write(&config_path, "[autoreview]\nloop_mode = false\n")
        .expect("write config with loop_mode disabled");
    // The fingerprint only re-stats on a 500ms throttle; sleep past it so the
    // disabled value is definitely observed.
    std::thread::sleep(std::time::Duration::from_millis(600));

    let product_app = || {
        let mut app = create_test_app();
        app.runtime_mode = AppRuntimeMode::RemoteClient;
        app.is_remote = false;
        app.is_replay = false;
        app.autoreview_enabled = true;
        app.pending_queued_dispatch = false;
        app.improve_mode = None;
        app.session.review_loop = None;
        app
    };

    // (1) loop_mode=false on disk: the loop must NOT seed.
    let mut app = product_app();
    super::commands::maybe_enter_review_loop(&mut app);
    assert!(
        app.session.review_loop.is_none(),
        "loop_mode=false in config.toml must prevent auto-seeding"
    );

    // (2) The env override beats the file: seeding now runs.
    crate::env::set_var("JCODE_AUTOREVIEW_LOOP_MODE", "true");
    let mut app = product_app();
    super::commands::maybe_enter_review_loop(&mut app);
    assert!(
        app.session.review_loop.as_ref().is_some_and(|s| !s.finished),
        "JCODE_AUTOREVIEW_LOOP_MODE=true must re-enable auto-seeding despite the file"
    );
}
