// Tests for buffering live tool output into the app's rolling tail.

fn test_app() -> App {
    App::new_for_remote(None)
}

#[test]
fn tool_output_chunk_accumulates_lines_in_one_view() {
    let mut app = test_app();
    assert!(app.apply_tool_output_chunk("call-1", "bash", "one\ntwo", false, false));
    let view = app.live_tool_output().expect("view");
    assert_eq!(view.tool_name, "bash");
    assert_eq!(view.tool_call_id, "call-1");
    assert_eq!(view.lines.len(), 2);
    assert_eq!(view.lines[0].text, "one");
    assert_eq!(view.lines[1].text, "two");
    assert!(!view.lines[1].stderr);
}

#[test]
fn tool_output_chunk_marks_stderr_lines() {
    let mut app = test_app();
    app.apply_tool_output_chunk("call-1", "bash", "err", true, false);
    let view = app.live_tool_output().expect("view");
    assert!(view.lines[0].stderr);
}

#[test]
fn tool_output_chunk_caps_tail_and_tracks_truncation() {
    let mut app = test_app();
    for i in 0..20 {
        app.apply_tool_output_chunk("call-1", "bash", &format!("line-{i}"), false, false);
    }
    let view = app.live_tool_output().expect("view");
    assert_eq!(
        view.lines.len(),
        crate::tui::app::live_tool_output::LIVE_OUTPUT_MAX_LINES
    );
    assert_eq!(
        view.truncated,
        20 - crate::tui::app::live_tool_output::LIVE_OUTPUT_MAX_LINES
    );
    // The most recent line must survive.
    assert_eq!(view.lines.last().unwrap().text, "line-19");
}

#[test]
fn tool_output_done_sentinel_clears_matching_call() {
    let mut app = test_app();
    app.apply_tool_output_chunk("call-1", "bash", "hello", false, false);
    assert!(app.live_tool_output().is_some());
    assert!(app.apply_tool_output_chunk("call-1", "bash", "", false, true));
    assert!(app.live_tool_output().is_none());
}

#[test]
fn tool_output_done_sentinel_ignores_other_call() {
    let mut app = test_app();
    app.apply_tool_output_chunk("call-2", "bash", "hello", false, false);
    // A late sentinel for a different call must not clear the current view.
    assert!(!app.apply_tool_output_chunk("call-1", "bash", "", false, true));
    assert!(app.live_tool_output().is_some());
}

#[test]
fn tool_output_chunk_from_new_call_replaces_tail() {
    let mut app = test_app();
    app.apply_tool_output_chunk("call-1", "bash", "old", false, false);
    app.apply_tool_output_chunk("call-2", "bash", "new", false, false);
    let view = app.live_tool_output().expect("view");
    assert_eq!(view.tool_call_id, "call-2");
    assert_eq!(view.lines.len(), 1);
    assert_eq!(view.lines[0].text, "new");
}

#[test]
fn empty_chunk_is_ignored() {
    let mut app = test_app();
    assert!(!app.apply_tool_output_chunk("call-1", "bash", "", false, false));
    assert!(app.live_tool_output().is_none());
}
#[test]
fn clearing_streaming_render_state_clears_live_output() {
    // Turn cleanup (completion and interrupt both route through
    // clear_streaming_render_state) must drop any live tool-output region.
    let mut app = test_app();
    app.apply_tool_output_chunk("call-1", "bash", "working", false, false);
    assert!(app.live_tool_output().is_some());

    app.clear_streaming_render_state();
    assert!(
        app.live_tool_output().is_none(),
        "clearing streaming render state must clear the live tool-output view"
    );
}

#[test]
fn clearing_for_a_finished_call_drops_only_that_region() {
    // Backstop for a dropped done sentinel: completing a tool clears its region.
    let mut app = test_app();
    app.apply_tool_output_chunk("call-1", "bash", "working", false, false);
    assert!(app.live_tool_output().is_some());

    // A completion for a different call must not clear it.
    app.clear_live_tool_output_for("call-other");
    assert!(
        app.live_tool_output().is_some(),
        "an unrelated completion must not clear the live region"
    );

    // The matching completion clears it.
    app.clear_live_tool_output_for("call-1");
    assert!(
        app.live_tool_output().is_none(),
        "completing the owning call must clear its live region"
    );
}

#[test]
fn oversized_line_is_stored_as_one_line_without_spurious_rows() {
    // When the publisher truncates an oversized line it must still arrive as a
    // single logical line; otherwise the region shows a bogus extra row (e.g.
    // an orphaned ellipsis) and the tail count is wrong.
    let mut app = test_app();
    // Simulate one chunk carrying exactly one (already truncated) line.
    let one_line = format!("{}…", "x".repeat(100));
    app.apply_tool_output_chunk("call-1", "bash", &one_line, false, false);
    let view = app.live_tool_output().expect("view");
    assert_eq!(
        view.lines.len(),
        1,
        "a single chunk line must produce exactly one stored line"
    );
    assert_eq!(view.lines[0].text, one_line);
}

/// The local (in-process) bus route is the default client mode and a separate
/// path from the remote wire handler, so it must be covered directly: a chunk
/// for the viewed session is applied, and one for another session is ignored.
#[test]
fn local_bus_route_applies_only_the_viewed_session() {
    let mut app = create_test_app();
    // Local mode: `active_client_session_id` is this app's own session id.
    let session_id = app.session.id.clone();
    assert!(!app.is_remote, "this test covers the local bus route");

    let other = crate::bus::ToolOutputChunk {
        session_id: "some-other-session".to_string(),
        tool_call_id: "foreign-call".to_string(),
        tool_name: "bash".to_string(),
        text: "foreign output".to_string(),
        stderr: false,
        done: false,
    };
    assert!(
        !crate::tui::app::local::handle_bus_event(
            &mut app,
            Ok(crate::bus::BusEvent::ToolOutputChunk(other))
        ),
        "a chunk for another session must not change the view"
    );
    assert!(app.live_tool_output().is_none());

    let mine = crate::bus::ToolOutputChunk {
        session_id: session_id.clone(),
        tool_call_id: "local-call".to_string(),
        tool_name: "bash".to_string(),
        text: "local output".to_string(),
        stderr: false,
        done: false,
    };
    assert!(
        crate::tui::app::local::handle_bus_event(
            &mut app,
            Ok(crate::bus::BusEvent::ToolOutputChunk(mine))
        ),
        "a chunk for the viewed session must repaint"
    );
    let view = app.live_tool_output().expect("view");
    assert_eq!(view.tool_call_id, "local-call");
    assert_eq!(view.lines[0].text, "local output");

    // The done sentinel for the viewed session clears it via the same route.
    let done = crate::bus::ToolOutputChunk {
        session_id,
        tool_call_id: "local-call".to_string(),
        tool_name: "bash".to_string(),
        text: String::new(),
        stderr: false,
        done: true,
    };
    assert!(crate::tui::app::local::handle_bus_event(
        &mut app,
        Ok(crate::bus::BusEvent::ToolOutputChunk(done))
    ));
    assert!(app.live_tool_output().is_none());
}
