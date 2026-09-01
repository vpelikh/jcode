// Tests for `/review-loop` typing suggestions and help.
//
// `/review-loop` was previously dispatchable but produced no suggestion while
// typing (it was absent from the suggestion registry). These pin the observable
// UX: typing `/review-loop` must offer `status` / `start` / `stop` completions
// and `/help review-loop` must show them, matching the existing `/autoreview`
// and `/judge` style. (Registry presence is asserted in
// `state_ui_input_helpers.rs` alongside the other registrations.)

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
    for expected in ["/review-loop start", "/review-loop stop", "/review-loop status"] {
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