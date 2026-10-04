//! Live output capture for a still-running tool call.
//!
//! While a foreground tool (notably `bash`) streams stdout/stderr, the server
//! publishes throttled `ToolOutputChunk` bus events. This module keeps a small,
//! bounded rolling tail for the currently-visible session so the TUI can render
//! what the command is doing, beneath the running-tool status line, instead of
//! only a one-line "running tool" status.
//!
//! The tail is deliberately capped: a chatty command must never grow this
//! unboundedly, and only the last few lines matter for a live view.

use super::App;
use crate::tui::{LiveOutputLine, LiveToolOutputView};

/// Maximum number of output lines retained for the live view.
pub(super) const LIVE_OUTPUT_MAX_LINES: usize = 12;

impl App {
    /// Append a live output chunk produced by a running tool call.
    ///
    /// `text` may contain multiple newline-separated lines. When `done` is set
    /// the live view for this call is cleared (the command is finishing and its
    /// full output belongs to the committed transcript, not the live region).
    ///
    /// Returns true when the live view changed and the frame needs a redraw.
    pub(super) fn apply_tool_output_chunk(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        text: &str,
        stderr: bool,
        done: bool,
    ) -> bool {
        if done {
            // Only clear if we are showing this exact call, so a late sentinel
            // from a finished call cannot wipe a newer call's live view.
            let matches = self
                .live_tool_output
                .as_ref()
                .is_some_and(|view| view.tool_call_id == tool_call_id);
            if matches {
                self.live_tool_output = None;
                return true;
            }
            return false;
        }

        if text.is_empty() {
            return false;
        }

        // A chunk for a different call replaces the current tail: only one tool
        // call streams live at a time in the common case.
        let stale = self
            .live_tool_output
            .as_ref()
            .is_some_and(|view| view.tool_call_id != tool_call_id);
        if stale {
            self.live_tool_output = None;
        }

        let view = self
            .live_tool_output
            .get_or_insert_with(|| LiveToolOutputView {
                tool_call_id: tool_call_id.to_string(),
                tool_name: tool_name.to_string(),
                lines: Vec::new(),
                truncated: 0,
            });

        for line in text.split('\n') {
            view.lines.push(LiveOutputLine {
                text: line.to_string(),
                stderr,
            });
        }
        let overflow = view.lines.len().saturating_sub(LIVE_OUTPUT_MAX_LINES);
        if overflow > 0 {
            view.lines.drain(..overflow);
            view.truncated = view.truncated.saturating_add(overflow);
        }
        true
    }

    /// Clear the live view if it belongs to `tool_call_id`.
    ///
    /// The explicit done sentinel normally handles this, but the bus is a
    /// broadcast channel: under a lag burst a client can miss the sentinel and
    /// be left showing a stale region. Tool completion is a reliable backstop,
    /// so every completion path calls this keyed by the finished call.
    pub(super) fn clear_live_tool_output_for(&mut self, tool_call_id: &str) {
        if self
            .live_tool_output
            .as_ref()
            .is_some_and(|view| view.tool_call_id == tool_call_id)
        {
            self.live_tool_output = None;
        }
    }
}
