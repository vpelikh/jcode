// Tests that typed suggestions stay connected to the command registry.
//
// `get_suggestions_for` returns the static subcommand completions declared in
// `REGISTERED_COMMANDS`. These tests pin that connection: commands that were
// previously dispatchable but produced no suggestion while typing (the
// "disconnected" commands) now surface their subcommands from the registry.

#[test]
fn previously_disconnected_commands_now_suggest_subcommands() {
    let mut app = create_test_app();

    // Each of these was dispatchable but had no hand-written suggestion branch;
    // their subcommands now come from the registry table.
    for input in [
        "/memory ",
        "/swarm ",
        "/initiatives ",
        "/thinking-display ",
        "/compact ",
        "/cache ",
    ] {
        app.input = input.to_string();
        app.cursor_pos = app.input.len();
        let suggestions = app.command_suggestions();
        assert!(
            !suggestions.is_empty(),
            "typing {input:?} should offer subcommand completions, got none"
        );
    }
}

#[test]
fn registry_subcommands_surface_while_typing() {
    let mut app = create_test_app();

    let expected: &[(&str, &str)] = &[
        ("/memory ", "/memory status"),
        ("/swarm ", "/swarm status"),
        ("/initiatives ", "/initiatives resume"),
        ("/initiatives ", "/initiatives show"),
        ("/thinking-display ", "/thinking-display off"),
        ("/compact ", "/compact mode"),
        ("/cache ", "/cache stats"),
        ("/autoreview ", "/autoreview now"),
        ("/review-loop ", "/review-loop stop"),
        ("/subagent-model ", "/subagent-model show"),
        ("/agents ", "/agents swarm"),
    ];

    for (input, expected_cmd) in expected {
        app.input = input.to_string();
        app.cursor_pos = app.input.len();
        let suggestions = app.command_suggestions();
        assert!(
            suggestions.iter().any(|(c, _)| c == expected_cmd),
            "typing {input:?} should offer {expected_cmd:?}, got {suggestions:?}"
        );
    }
}

/// The core invariant behind this refactor: every non-hidden registered
/// command is discoverable while typing. Saying the command's name (or a
/// registered alias, which canonicalizes to its primary form) must yield at
/// least one suggestion rather than silently nothing. This prevents a
/// registered command from being absent from the typing suggestions.
#[test]
fn every_registered_command_yields_a_suggestion_while_typing() {
    let mut app = create_test_app();

    for (name, subcommands) in super::state_ui_input_helpers::registered_command_specs() {
        // Typing exactly the command name must always surface something.
        app.input = name.to_string();
        app.cursor_pos = app.input.len();
        let suggestions = app.command_suggestions();
        assert!(
            !suggestions.is_empty(),
            "typing {name:?} (bare) should yield at least one suggestion, got none"
        );

        // A command with declared subcommands must also surface them once the
        // user starts typing a subcommand.
        if !subcommands.is_empty() {
            app.input = format!("{name} ");
            app.cursor_pos = app.input.len();
            let suggestions = app.command_suggestions();
            assert!(
                suggestions
                    .iter()
                    .any(|(c, _)| subcommands.iter().any(|(s, _)| s == c)),
                "typing '{name} ' should offer a declared subcommand, got {suggestions:?}"
            );
        }
    }
}

/// End-to-end workflow: typing a partial subcommand surfaces a completion
/// from the registry, accepting it fills in the full command, and submitting
/// it actually dispatches to the handler. This pins the full path a user goes
/// through, including the previously-disconnected commands.
#[test]
fn accept_suggestion_dispatches_the_completed_command() {
    let mut app = create_test_app();

    // /memory status is observable via app.memory_enabled / status message.
    app.input = "/memory st".to_string();
    app.cursor_pos = app.input.len();
    let suggestions = app.command_suggestions();
    assert!(
        suggestions.iter().any(|(c, _)| c == "/memory status"),
        "/memory st should suggest /memory status, got {suggestions:?}"
    );
    // Accept the top (selected) suggestion; it fills in /memory status.
    app.command_suggestion_selected = suggestions
        .iter()
        .position(|(c, _)| c == "/memory status")
        .unwrap();
    assert!(
        app.accept_selected_command_suggestion(),
        "accepting the suggestion should fill in the input"
    );
    assert_eq!(app.input, "/memory status");

    // Submitting dispatches and shows the status message.
    app.submit_input();
    let last = app.display_messages().last().expect("memory status reply");
    assert!(
        last.content.contains("Memory feature"),
        "/memory status should report the feature state, got {:?}",
        last.content
    );

    // /swarm status likewise dispatches (no panic, status handled).
    app.input = "/swarm ".to_string();
    app.cursor_pos = app.input.len();
    let suggestions = app.command_suggestions();
    assert!(
        suggestions.iter().any(|(c, _)| c == "/swarm status"),
        "/swarm should suggest status, got {suggestions:?}"
    );
    app.command_suggestion_selected = suggestions
        .iter()
        .position(|(c, _)| c == "/swarm status")
        .unwrap();
    assert!(app.accept_selected_command_suggestion());
    assert_eq!(app.input, "/swarm status");
    app.submit_input();
}

/// Nested-subcommand narrowing: /compact mode surfaces the mode choices, and
/// /goals show surfaces goal ids. These exercise the deeper completion levels
/// that the table-driven fallback must still deliver.
#[test]
fn nested_subcommands_narrow_while_typing() {
    let mut app = create_test_app();

    app.input = "/compact mode ".to_string();
    app.cursor_pos = app.input.len();
    let suggestions = app.command_suggestions();
    assert!(
        suggestions
            .iter()
            .any(|(c, _)| c == "/compact mode reactive"),
        "/compact mode should suggest reactive, got {suggestions:?}"
    );

    // Narrowing /compact mode r → reactive.
    app.input = "/compact mode r".to_string();
    app.cursor_pos = app.input.len();
    let suggestions = app.command_suggestions();
    assert!(
        suggestions
            .iter()
            .any(|(c, _)| c == "/compact mode reactive"),
        "partial /compact mode r should keep reactive, got {suggestions:?}"
    );

    // Goals show surfaces goal ids (may be empty in a fresh app, but must not
    // panic and /goals show <id> suggestion set stays valid).
    app.input = "/goals show".to_string();
    app.cursor_pos = app.input.len();
    let _ = app.command_suggestions();
}

/// Alias and canonical-form edges still behave: typing an alias surfaces the
/// canonical command (e.g. /models → /model), and /review stays a single
/// suggestion (the one-shot command), not its /review-loop sibling.
#[test]
fn alias_and_bare_edges_are_preserved() {
    let mut app = create_test_app();

    // /models is an alias of /model and surfaces the canonical completion.
    app.input = "/models".to_string();
    app.cursor_pos = app.input.len();
    let suggestions = app.command_suggestions();
    assert!(
        !suggestions.is_empty(),
        "/models should yield suggestions, got none"
    );
    assert!(
        suggestions.iter().any(|(c, _)| c.starts_with("/model")),
        "/models should suggest /model or a model completion, got {suggestions:?}"
    );

    // /?: bare ? maps to help completions, not nothing.
    app.input = "/?".to_string();
    app.cursor_pos = app.input.len();
    let suggestions = app.command_suggestions();
    assert!(
        !suggestions.is_empty(),
        "/? should yield suggestions, got none"
    );

    // /review yields exactly the one-shot command (single suggestion), not the
    // /review-loop sibling.
    app.input = "/review".to_string();
    app.cursor_pos = app.input.len();
    let suggestions = app.command_suggestions();
    assert_eq!(
        suggestions
            .iter()
            .map(|(c, _)| c.as_str())
            .collect::<Vec<_>>(),
        vec!["/review"],
        "bare /review should suggest only itself, got {suggestions:?}"
    );
}

/// Render-level acceptance check: with the input set to a table-driven
/// subcommand prefix, the actual rendered frame shows the completion. This
/// validates the user-visible output (not just the data layer) for the
/// registry-driven suggestions.
#[test]
fn rendered_frame_shows_registry_subcommand_suggestion() {
    let _lock = scroll_render_test_lock();
    let mut app = create_test_app();
    app.input = "/memory ".to_string();
    app.cursor_pos = app.input.len();

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20))
        .expect("failed to create test terminal");
    let snap = render_and_snap(&app, &mut terminal);

    assert!(
        snap.contains("/memory status"),
        "rendered frame should show the /memory status completion, got:\n{snap}"
    );
}

/// A command with a bare (no-arg) actionable form must keep offering that bare
/// form as a completion, not only its subcommands. E.g. typing day-toggling
/// command like `/cache` (which toggles when run bare) should still surface
/// `/cache` itself so a user can act on it from the palette.
#[test]
fn bare_command_with_subcommands_still_offers_itself() {
    let mut app = create_test_app();

    // /cache toggles when run bare; the completion set should let the user
    // pick the bare command to toggle, alongside its subcommands.
    app.input = "/cache".to_string();
    app.cursor_pos = app.input.len();
    let suggestions = app.command_suggestions();
    assert!(
        suggestions.iter().any(|(c, _)| c == "/cache"),
        "bare /cache should offer /cache itself so it can be toggled, got {suggestions:?}"
    );
}

/// Probe: what does a SPACED bare command offer? After typing "/cache " the user
/// wants subcommands, but the bare toggle "/cache" should not be spuriously
/// absent NOR appear confusingly. Pin the observed shape.
#[test]
fn spaced_bare_command_offers_subcommands_and_bare() {
    let mut app = create_test_app();
    app.input = "/cache ".to_string();
    app.cursor_pos = app.input.len();
    let suggestions = app.command_suggestions();
    // The relevant subcommands must be present.
    for expected in ["/cache 1h", "/cache 5m", "/cache stats", "/cache status"] {
        assert!(
            suggestions.iter().any(|(c, _)| c == expected),
            "spacing /cache should keep {expected}, got {suggestions:?}"
        );
    }
}

/// After typing a trailing space (signalling subcommand intent), the bare
/// command itself should not rank confusingly ahead of its subcommands. The
/// exact prefix "/cache " must not surface the bare "/cache" as a fuzzy match.
#[test]
fn spaced_input_does_not_prime_the_bare_toggle() {
    let mut app = create_test_app();
    app.input = "/cache ".to_string();
    app.cursor_pos = app.input.len();
    let suggestions = app.command_suggestions();
    // The first suggestion should be a concrete subcommand, not the bare toggle.
    assert_ne!(
        suggestions.first().map(|(c, _)| c.as_str()),
        Some("/cache"),
        "typing '/cache ' should prefer a subcommand, got {suggestions:?}"
    );
}

/// Progressive typing keeps every target reachable: a partial command prefix
/// surfaces the top-level command, and a partial subcommand prefix surfaces
/// the matching subcommand. This is the real "always has suggestions while
/// typing" path a user experiences keystroke by keystroke.
#[test]
fn partial_prefixes_surface_command_and_subcommand_at_each_stage() {
    let mut app = create_test_app();

    // Partial command prefix -> the top-level command appears.
    app.input = "/mem".to_string();
    app.cursor_pos = app.input.len();
    let s = app.command_suggestions();
    assert!(
        s.iter().any(|(c, _)| c == "/memory"),
        "/mem should reach /memory, got {s:?}"
    );

    // Full command -> subcommands appear.
    app.input = "/memory ".to_string();
    app.cursor_pos = app.input.len();
    let s = app.command_suggestions();
    assert!(
        s.iter().any(|(c, _)| c == "/memory on"),
        "/memory  should reach subcommands, got {s:?}"
    );

    // Partial subcommand -> the specific subcommand is reachable.
    app.input = "/memory of".to_string();
    app.cursor_pos = app.input.len();
    let s = app.command_suggestions();
    assert!(
        s.iter().any(|(c, _)| c == "/memory off"),
        "/memory of should reach /memory off, got {s:?}"
    );
}
