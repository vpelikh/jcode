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
        msg.content.contains("Review loop active at lens: Correctness"),
        "active status must show the lens, got {:?}",
        msg.content
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
