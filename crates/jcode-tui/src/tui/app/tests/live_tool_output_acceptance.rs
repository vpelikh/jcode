// Real-App acceptance test for live tool output: applies the actual wire
// ServerEvent against a real App and renders the real frame, then asserts the
// running command's output is visible on screen and disappears on the sentinel.

fn live_output_frame_text(app: &App, terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
    // Full-frame draws publish shared process-global render state; hold the
    // shared render-state lock so the full suite does not interleave frames.
    let _lock = crate::tui::ui::render_state_test_lock();
    terminal
        .draw(|frame| crate::tui::ui::draw(frame, app))
        .expect("draw should not panic");
    let buffer = terminal.backend().buffer();
    let width = buffer.area.width;
    (0..buffer.area.height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn real_app_renders_live_tool_output_from_server_event() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let backend = ratatui::backend::TestBackend::new(90, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let mut remote = crate::tui::backend::RemoteConnection::dummy();
        let mut state = super::remote::RemoteRunState::default();

        // A bash tool call starts and enters execution, so the app is in the
        // running-tool state the live region belongs to.
        for event in [
            crate::protocol::ServerEvent::ToolStart {
                id: "call-live".to_string(),
                name: "bash".to_string(),
            },
            crate::protocol::ServerEvent::ToolExec {
                id: "call-live".to_string(),
                name: "bash".to_string(),
            },
        ] {
            let _ = rt
                .block_on(super::remote::handle_remote_event(
                    &mut app,
                    &mut terminal,
                    &mut remote,
                    &mut state,
                    crate::tui::backend::RemoteRead::Event(event),
                ))
                .expect("event should apply");
        }

        // The live region honors display.show_bash_output; enable it (the test
        // override defaults off).
        crate::tui::ui::tools_ui::tests_show_bash_output_override::set(true);

        // Live output chunk arrives from the wire.
        let needs_redraw = app.apply_tool_output_chunk(
            "call-live",
            "bash",
            "downloading crates...\nfinished 42 crates",
            false,
            false,
            false,
            false,
        );
        assert!(needs_redraw, "a live chunk should request a redraw");

        let text = live_output_frame_text(&app, &mut terminal);
        assert!(
            text.contains("bash output"),
            "live region header should render in the real frame:\n{text}"
        );
        assert!(
            text.contains("downloading crates...") && text.contains("finished 42 crates"),
            "streamed lines should render in the real frame:\n{text}"
        );

        // Terminal sentinel clears the region.
        let cleared =
            app.apply_tool_output_chunk("call-live", "bash", "", false, false, false, true);
        assert!(cleared, "the done sentinel should change the view");
        let text_after = live_output_frame_text(&app, &mut terminal);
        assert!(
            !text_after.contains("bash output"),
            "live region should disappear after the done sentinel:\n{text_after}"
        );
    });
}

#[test]
fn live_output_disables_the_one_cell_spinner_fast_path() {
    // The one-cell spinner repaint only patches the status cell, so it cannot
    // reflect a live region appearing/disappearing above it. While live output is
    // visible the fast path must stand down and force full redraws.
    let mut app = create_test_app();
    app.is_processing = true;
    app.status = ProcessingStatus::RunningTool("bash".to_string());
    app.processing_started = Some(Instant::now());
    // The region (and thus the reason to stand down) only exists while bash
    // output display is enabled.
    crate::tui::ui::tools_ui::tests_show_bash_output_override::set(true);

    // Baseline: with a plain running tool and no live output, the fast path is
    // available (status uses the primary spinner).
    assert!(
        super::run_shell::status_spinner_only_symbol(&app).is_some(),
        "running bash should allow the single-cell spinner fast path"
    );

    app.apply_tool_output_chunk("call-live", "bash", "working", false, false, false, false);
    assert!(
        super::run_shell::status_spinner_only_symbol(&app).is_none(),
        "the fast path must yield while live tool output is visible"
    );

    // With bash output disabled the region is never visible, so the fast path
    // must stay available even though a live view is buffered.
    crate::tui::ui::tools_ui::tests_show_bash_output_override::set(false);
    app.apply_tool_output_chunk("call-live", "bash", "working", false, false, false, false);
    assert!(
        super::run_shell::status_spinner_only_symbol(&app).is_some(),
        "the fast path must stay available when bash output is disabled"
    );
    crate::tui::ui::tools_ui::tests_show_bash_output_override::set(true);
    app.apply_tool_output_chunk("call-live", "bash", "", false, false, false, true);

    // Clear it again: the fast path returns.
    app.apply_tool_output_chunk("call-live", "bash", "", false, false, false, true);
    assert!(
        super::run_shell::status_spinner_only_symbol(&app).is_some(),
        "the fast path should resume once the live view is cleared"
    );
}

/// A carriage-return progress update must repaint the live region in place, so
/// the on-screen text changes without the region growing a duplicate row.
/// This is the user-visible acceptance path for `replace`/`partial` plumbing.
#[test]
fn real_app_repaints_carriage_return_overwrite_in_place() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let backend = ratatui::backend::TestBackend::new(90, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let mut remote = crate::tui::backend::RemoteConnection::dummy();
        let mut state = super::remote::RemoteRunState::default();

        for event in [
            crate::protocol::ServerEvent::ToolStart {
                id: "call-cr".to_string(),
                name: "bash".to_string(),
            },
            crate::protocol::ServerEvent::ToolExec {
                id: "call-cr".to_string(),
                name: "bash".to_string(),
            },
        ] {
            let _ = rt
                .block_on(super::remote::handle_remote_event(
                    &mut app,
                    &mut terminal,
                    &mut remote,
                    &mut state,
                    crate::tui::backend::RemoteRead::Event(event),
                ))
                .expect("event should apply");
        }
        crate::tui::ui::tools_ui::tests_show_bash_output_override::set(true);

        // First overwrite: "10%".
        app.apply_tool_output_chunk(
            "call-cr",
            "bash",
            "10%",
            false,
            false,
            false,
            false,
        );
        let first = live_output_frame_text(&app, &mut terminal);
        assert!(
            first.contains("10%"),
            "the first progress value must be visible:\n{first}"
        );

        // Second overwrite: "20%" replaces it in place.
        app.apply_tool_output_chunk(
            "call-cr",
            "bash",
            "20%",
            false,
            true,
            false,
            false,
        );
        let second = live_output_frame_text(&app, &mut terminal);
        assert!(
            second.contains("20%"),
            "the overwritten progress value must be visible:\n{second}"
        );
        assert!(
            !second.contains("10%"),
            "the overwritten value must not remain on screen:\n{second}"
        );
    });
}
