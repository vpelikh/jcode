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
    for input in ["/memory ", "/swarm ", "/initiatives ", "/thinking-display ", "/compact ", "/cache "]
    {
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