//! Tests for the live tool-output region shown while a tool call streams
//! stdout/stderr beneath the running-tool status line.
//!
//! These render the region in isolation (not a whole app frame): the region is
//! the unit under test, and full-frame draws couple to process-global render
//! state that other tests mutate, which made these flake under parallelism.

use super::*;
use ratatui::backend::TestBackend;

/// The live region honors `display.show_bash_output`; tests that render it must
/// opt in, since the test override defaults to off.
fn enable_bash_output() {
    crate::tui::ui::tools_ui::tests_show_bash_output_override::set(true);
}

fn live_state(tool_name: &str, lines: &[(&str, bool)]) -> TestState {
    enable_bash_output();
    TestState {
        live_tool_output: Some(crate::tui::LiveToolOutputView {
            tool_call_id: "call-1".to_string(),
            tool_name: tool_name.to_string(),
            lines: lines
                .iter()
                .map(|(text, stderr)| crate::tui::LiveOutputLine {
                    text: text.to_string(),
                    stderr: *stderr,
                    partial: false,
                })
                .collect(),
            truncated: 0,
        }),
        ..Default::default()
    }
}

#[test]
fn live_output_region_height_is_zero_without_a_view() {
    enable_bash_output();
    let state = TestState::default();
    assert_eq!(crate::tui::ui::input_ui::live_tool_output_height(&state), 0);
}

#[test]
fn live_output_region_height_grows_with_lines() {
    let state = live_state("bash", &[("a", false), ("b", false)]);
    // header + 2 lines
    assert_eq!(crate::tui::ui::input_ui::live_tool_output_height(&state), 3);
}

#[test]
fn live_output_region_height_is_capped() {
    // More retained lines than the region can draw: the height pins to the row
    // cap rather than growing with the line count.
    let lines: Vec<(&str, bool)> = (0..40).map(|_| ("line", false)).collect();
    let state = live_state("bash", &lines);
    assert_eq!(
        crate::tui::ui::input_ui::live_tool_output_height(&state),
        crate::tui::ui::input_ui::LIVE_OUTPUT_REGION_MAX_ROWS as u16
    );
}

#[test]
fn live_output_region_renders_header_and_recent_lines() {
    let state = live_state("bash", &[("first", false), ("second", true)]);
    let rows = render_region_rows(&state, 80, 12);
    let joined = rows.join("\n");
    assert!(
        joined.contains("bash output"),
        "expected header in frame:\n{joined}"
    );
    assert!(
        joined.contains("first") && joined.contains("second"),
        "expected streamed lines in frame:\n{joined}"
    );
}

#[test]
fn live_output_region_absent_when_idle() {
    let state = TestState {
        display_messages: vec![crate::tui::DisplayMessage::assistant("done")],
        ..Default::default()
    };
    let rows = render_region_rows(&state, 80, 12);
    assert!(
        !rows.join("\n").contains("bash output"),
        "idle frame must not show the live region"
    );
}

#[test]
fn live_output_region_strips_ansi_escapes_and_control_chars() {
    // A color-emitting command: ANSI SGR plus a carriage return.
    let state = live_state("bash", &[("\u{1b}[32mok\u{1b}[0m\rbuilt", false)]);
    let rows = render_region_rows(&state, 80, 12);
    let joined = rows.join("\n");
    assert!(
        !joined.contains('\u{1b}'),
        "raw escape bytes must not reach the frame:\n{joined:?}"
    );
    assert!(
        !joined.contains('\r'),
        "carriage returns must not reach the frame:\n{joined:?}"
    );
    assert!(
        joined.contains("ok") && joined.contains("built"),
        "visible text should survive sanitization:\n{joined:?}"
    );
}

#[test]
fn live_output_shows_the_most_recent_lines_when_tail_exceeds_area() {
    // More lines than the region can draw; the most recent lines are the ones
    // that matter for a live view, so the oldest must scroll out.
    let count = crate::tui::ui::input_ui::LIVE_OUTPUT_REGION_MAX_ROWS + 4;
    let lines: Vec<(&str, bool)> = (0..count)
        .map(|i| (Box::leak(format!("row-{i}").into_boxed_str()) as &str, false))
        .collect();
    let state = live_state("bash", &lines);
    // A realistic terminal: tall enough that the capped region and the rest of
    // the chrome fit, so this verifies tail truncation rather than terminal
    // packing.
    let rows = render_region_rows(&state, 80, 24);
    let joined = rows.join("\n");
    let newest = format!("row-{}", count - 1);
    assert!(
        joined.contains(&newest),
        "the newest line must be visible:\n{joined}"
    );
    assert!(
        !joined.contains(" row-0\n") && !joined.contains(" row-0 "),
        "the oldest line should be scrolled out of the bounded region:\n{joined}"
    );
}

#[test]
fn live_output_long_single_line_does_not_break_frame() {
    let long = "x".repeat(500);
    let state = live_state("bash", &[(long.as_str(), false)]);
    // Must not panic and must still render the header.
    let rows = render_region_rows(&state, 60, 12);
    assert!(rows.join("\n").contains("bash output"));
}

#[test]
fn live_output_long_lines_do_not_push_out_the_newest_line() {
    // Three lines each wider than the 20-col area. If they wrap, they consume
    // more rows than the region reserved and the newest line ('newest-marker')
    // is clipped off the bottom. Each source line must map to one row.
    enable_bash_output();
    let wide_a = "a".repeat(60);
    let wide_b = "b".repeat(60);
    let state = live_state(
        "bash",
        &[
            (wide_a.as_str(), false),
            (wide_b.as_str(), false),
            ("newest-marker", false),
        ],
    );
    let rows = render_region_rows(&state, 20, 24);
    let joined = rows.join("\n");
    assert!(
        joined.contains("newest-marker"),
        "the newest line must stay visible even when earlier lines are wide:\n{joined}"
    );
}

#[test]
fn live_output_narrow_terminal_does_not_panic() {
    let state = live_state("bash", &[("hello world", false), ("stderr here", true)]);
    let rows = render_region_rows(&state, 12, 10);
    assert!(!rows.is_empty());
}

/// Render only the live tool-output region into a `width x height` buffer and
/// return its rows. Rendering the region directly (not a whole app frame) keeps
/// these tests independent of global render/theme state.
fn render_region_rows(state: &TestState, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            // Mirror production: the region gets exactly the height the layout
            // reserves for it (bounded), not the whole terminal.
            let reserved = crate::tui::ui::input_ui::live_tool_output_height(state);
            let area = ratatui::layout::Rect {
                x: frame.area().x,
                y: frame.area().y,
                width,
                height: reserved.min(height),
            };
            crate::tui::ui::input_ui::draw_live_tool_output(frame, state, area);
        })
        .expect("draw should not panic");
    let buffer = terminal.backend().buffer();
    let w = buffer.area.width;
    (0..buffer.area.height)
        .map(|y| {
            (0..w)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn live_output_reports_one_consistent_earlier_count() {
    // Both the app-level tail cap (truncated) and render-level fitting elide
    // lines. The region must show a single combined count, not two numbers.
    enable_bash_output();
    let lines: Vec<(&str, bool)> = (0..12)
        .map(|i| (Box::leak(format!("row-{i}").into_boxed_str()) as &str, false))
        .collect();
    let mut state = live_state("bash", &lines);
    state.live_tool_output.as_mut().unwrap().truncated = 5;

    let rows = render_region_rows(&state, 80, 24);
    let joined = rows.join("\n");
    assert!(
        joined.contains("earlier lines"),
        "elided lines should be summarized:\n{joined}"
    );
    // The old header suffix duplicated this information with a different count.
    assert!(
        !joined.contains("more)"),
        "header must not show a second, conflicting overflow count:\n{joined}"
    );
}

#[test]
fn live_region_layout_total_always_equals_drawn_rows() {
    // The height reservation and the draw path must agree exactly, or the
    // region leaves a blank row (reserved > drawn) or clips the newest line
    // (drawn > reserved). Exercise the shared layout directly across the
    // reachable state space, including the case that previously diverged:
    // truncated > 0 with fewer than the max retained lines.
    let max_lines = crate::tui::app::live_tool_output::LIVE_OUTPUT_MAX_LINES;
    for lines in 1..=max_lines {
        for truncated in 0..=5usize {
            if truncated > 0 && lines != max_lines {
                continue; // not reachable: the cap only elides at the cap
            }
            let layout = crate::tui::ui::input_ui::live_region_layout_for_tests(lines, truncated);
            let drawn = 1 + layout.indicator + layout.body;
            assert_eq!(
                layout.total, drawn,
                "reserved vs drawn mismatch at lines={lines} truncated={truncated}"
            );
            assert!(
                layout.indicator + layout.body <= layout.total,
                "indicator+body must fit the region at lines={lines} truncated={truncated}"
            );
        }
    }
    // The specific former divergence: 1 line, 5 elided -> 1 header + 1 body.
    let layout = crate::tui::ui::input_ui::live_region_layout_for_tests(1, 5);
    assert_eq!((layout.total, layout.indicator, layout.body), (2, 0, 1));
}

#[test]
fn live_region_layout_fits_a_squeezed_area() {
    // When the renderer hands the region fewer rows than it reserved, the draw
    // must fit within those rows and never overflow (which would clip the newest
    // line). Check across budgets that the drawn rows never exceed the area.
    for area in 0..=8usize {
        for lines in [1usize, 2, 7, 12] {
            let layout =
                crate::tui::ui::input_ui::live_region_layout_at_budget_for_tests(lines, 0, area);
            // Nothing renders without a budget, so nothing (not even a header)
            // is drawn in that case.
            let drawn = if layout.total == 0 {
                0
            } else {
                1 + layout.indicator + layout.body
            };
            assert_eq!(
                layout.total, drawn,
                "reserved != drawn at area={area} lines={lines}"
            );
            assert!(
                drawn <= area.max(1),
                "drawn {drawn} rows must not exceed area {area} (lines={lines})"
            );
        }
    }
}

#[test]
fn live_output_region_hidden_when_bash_output_disabled() {
    // `show_bash_output = false` is documented as "no bash output at all", so
    // the live region must not appear even while a bash call is running.
    crate::tui::ui::tools_ui::tests_show_bash_output_override::set(false);
    let state = TestState {
        live_tool_output: Some(crate::tui::LiveToolOutputView {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            lines: vec![crate::tui::LiveOutputLine {
                text: "visible only when enabled".to_string(),
                stderr: false,
                partial: false,
            }],
            truncated: 0,
        }),
        ..Default::default()
    };
    assert_eq!(
        crate::tui::ui::input_ui::live_tool_output_height(&state),
        0,
        "region must reserve no height when bash output is disabled"
    );
    let rows = render_region_rows(&state, 80, 24);
    assert!(
        !rows.join("\n").contains("bash output"),
        "region must not render when bash output is disabled"
    );
    enable_bash_output();
}
