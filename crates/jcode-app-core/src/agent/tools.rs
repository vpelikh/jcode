use crate::message::{ContentBlock, ToolCall};
use crate::terminal_println as println;
use crate::tool::ToolOutput;

pub(super) const MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY: usize = 512 * 1024;

pub(super) fn cap_tool_output_for_history(tool_name: &str, mut output: ToolOutput) -> ToolOutput {
    if output.output.chars().count() <= MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY {
        return output;
    }

    let original_chars = output.output.chars().count();
    let kept = crate::util::truncate_str(&output.output, MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY);
    output.output = format!(
        "{}\n\n[Tool output truncated by jcode: tool `{}` produced {} chars; kept first {} chars to protect the remote protocol, session history, and prompt cache. Redirect large logs to a file and read targeted sections.]",
        kept, tool_name, original_chars, MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY,
    );
    output
}

pub(super) fn cap_sdk_tool_content_for_history(tool_name: &str, content: String) -> String {
    if content.chars().count() <= MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY {
        return content;
    }
    let original_chars = content.chars().count();
    let kept = crate::util::truncate_str(&content, MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY);
    format!(
        "{}\n\n[Tool output truncated by jcode: tool `{}` produced {} chars; kept first {} chars to protect the remote protocol, session history, and prompt cache. Redirect large logs to a file and read targeted sections.]",
        kept, tool_name, original_chars, MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY,
    )
}

/// Build rendered side-pane images from a tool output's attached images.
///
/// This mirrors how `render_messages_and_images` derives images from persisted
/// session history (source = ToolResult), so live-streamed images match what a
/// later History reload would produce. `tool_name` and `tool_input` provide the
/// label fallback (e.g. the `read` tool's `file_path`); `tool_call_id` anchors
/// the image to its tool message in the transcript.
pub(super) fn tool_output_side_pane_images(
    tool_call_id: &str,
    tool_name: &str,
    tool_input: &serde_json::Value,
    output: &ToolOutput,
) -> Vec<jcode_session_types::RenderedImage> {
    if output.images.is_empty() {
        return Vec::new();
    }
    let fallback_label = tool_input
        .get("file_path")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    output
        .images
        .iter()
        .map(|img| jcode_session_types::RenderedImage {
            media_type: img.media_type.clone(),
            data: img.data.clone(),
            label: img
                .label
                .as_ref()
                .map(|label| label.trim().to_string())
                .filter(|label| !label.is_empty())
                .or_else(|| fallback_label.clone()),
            source: jcode_session_types::RenderedImageSource::ToolResult {
                tool_name: tool_name.to_string(),
            },
            anchor: Some(jcode_session_types::RenderedImageAnchor::ToolCall {
                id: tool_call_id.to_string(),
            }),
        })
        .collect()
}

pub(super) fn tool_output_to_content_blocks(
    tool_use_id: impl Into<crate::session::ToolCallId>,
    output: ToolOutput,
) -> Vec<ContentBlock> {
    let mut blocks = vec![ContentBlock::ToolResult {
        tool_use_id: tool_use_id.into(),
        content: output.output,
        is_error: None,
    }];
    for img in output.images {
        blocks.push(ContentBlock::Image {
            media_type: img.media_type,
            data: img.data,
        });
        if let Some(label) = img.label.filter(|label| !label.trim().is_empty()) {
            blocks.push(ContentBlock::Text {
                text: format!(
                    "[Attached image associated with the preceding tool result: {}]",
                    label
                ),
                cache_control: None,
            });
        }
    }
    blocks
}

pub(super) fn print_tool_summary(tool: &ToolCall) {
    if let Some(line) = tool_summary_line(tool) {
        println!("{}", line);
    }
}

/// Compute the single-line live summary for a tool call, or `None` when the
/// tool has nothing worth surfacing. Kept pure (returns a `String` rather than
/// printing directly) so the summaries are unit-testable without capturing
/// stdout.
fn tool_summary_line(tool: &ToolCall) -> Option<String> {
    match tool.name.as_str() {
        "bash" => tool
            .input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|cmd| {
                if cmd.len() > 60 {
                    format!("{}...", crate::util::truncate_str(cmd, 60))
                } else {
                    cmd.to_string()
                }
            })
            .map(|short| format!("$ {}", short)),
        "read" | "write" | "edit" => tool
            .input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|path| path.to_string()),
        "glob" | "grep" => tool
            .input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|pattern| format!("'{}'", pattern)),
        "compass_query" => tool
            .input
            .get("query")
            .and_then(|v| v.as_str())
            .and_then(agent_query_summary),
        "agentgrep" => {
            // `query` describes grep/find searches. Other modes (outline, trace,
            // smart) use different fields (`file`, `terms`), so fall back to the
            // resolved mode name rather than emitting an empty summary. This
            // mirrors the TUI agentgrep arm (`grep 'query'`, mode name otherwise)
            // and the `mode` default is "grep".
            tool.input
                .get("query")
                .and_then(|v| v.as_str())
                .and_then(agent_query_summary)
                .or_else(|| {
                    let mode = tool
                        .input
                        .get("mode")
                        .and_then(|v| v.as_str())
                        .unwrap_or("grep");
                    (!mode.is_empty()).then(|| mode.to_string())
                })
        }
        "ls" => Some(
            tool.input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".")
                .to_string(),
        ),
        _ => None,
    }
}

/// Quoted, truncated form of a free-form search `query` (used by compass_query
/// and agentgrep). Returns `None` for a blank query so callers can fall back to
/// a mode name. The 60-byte truncation keeps a single stdout line bounded and
/// mirrors the byte cap used by the adjacent `bash` arm.
fn agent_query_summary(query: &str) -> Option<String> {
    if query.trim().is_empty() {
        return None;
    }
    let label = if query.len() > 60 {
        format!("{}...", crate::util::truncate_str(query, 60))
    } else {
        query.to_string()
    };
    Some(format!("'{}'", label))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call(name: &str, query: &str) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            input: serde_json::json!({ "query": query }),
            ..Default::default()
        }
    }

    fn agentgrep_call(mode: &str, query: Option<&str>) -> ToolCall {
        let mut input = serde_json::Map::new();
        input.insert("mode".to_string(), serde_json::json!(mode));
        if let Some(query) = query {
            input.insert("query".to_string(), serde_json::json!(query));
        }
        ToolCall {
            name: "agentgrep".to_string(),
            input: serde_json::Value::Object(input),
            ..Default::default()
        }
    }

    #[test]
    fn tool_summary_shows_compass_query() {
        let line = tool_summary_line(&tool_call("compass_query", "search for the config"));
        assert_eq!(line.as_deref(), Some("'search for the config'"));
    }

    #[test]
    fn tool_summary_shows_agentgrep_query() {
        let line = tool_summary_line(&tool_call("agentgrep", "fn config"));
        assert_eq!(line.as_deref(), Some("'fn config'"));
    }

    #[test]
    fn agent_query_summary_blank_short_and_long() {
        // Blank -> None (caller decides a fallback).
        assert_eq!(agent_query_summary("   "), None);
        // Short (incl. multibyte) under the 60-byte cap -> quoted unchanged.
        assert_eq!(
            agent_query_summary("fn config 配置"),
            Some("'fn config 配置'".to_string())
        );
        // Over the 60-byte cap -> quoted, truncated, ellipsis-suffixed.
        let long = "界".repeat(100);
        let s = agent_query_summary(&long).unwrap();
        assert!(s.starts_with('\''));
        assert!(s.ends_with("...'"));
        assert!(long.starts_with(s.trim_matches('\'').trim_end_matches("...")));
    }

    #[test]
    fn tool_summary_agentgrep_grep_with_query_shows_query() {
        let line = tool_summary_line(&agentgrep_call("grep", Some("fn config")));
        assert_eq!(line.as_deref(), Some("'fn config'"));
    }

    /// agentgrep modes that don't use `query` (outline, trace, smart) fall back
    /// to the resolved mode name instead of an empty summary, mirroring the TUI.
    #[test]
    fn tool_summary_agentgrep_non_grep_mode_without_query_shows_mode() {
        assert_eq!(
            tool_summary_line(&agentgrep_call("outline", None)),
            Some("outline".to_string())
        );
        assert_eq!(
            tool_summary_line(&agentgrep_call("trace", None)),
            Some("trace".to_string())
        );
        assert_eq!(
            tool_summary_line(&agentgrep_call("smart", None)),
            Some("smart".to_string())
        );
    }

    /// A blank query in grep mode still falls back to the mode name (never an
    /// empty summary with no info at all).
    #[test]
    fn tool_summary_agentgrep_blank_query_falls_back_to_mode() {
        let line = tool_summary_line(&agentgrep_call("grep", Some("   ")));
        assert_eq!(line, Some("grep".to_string()));
    }

    #[test]
    fn tool_summary_blank_query_is_empty() {
        let line = tool_summary_line(&tool_call("compass_query", "   "));
        assert_eq!(line, None);
    }

    #[test]
    fn tool_summary_truncates_long_query() {
        let long = "x".repeat(100);
        let line = tool_summary_line(&tool_call("compass_query", &long)).unwrap();
        assert!(line.starts_with("'xxxx"));
        assert!(line.ends_with("...'"));
        // 2 quotes + up to 60 bytes + "..."
        assert!(line.chars().count() <= 2 + 60 + 3);
    }

    /// Byte-based truncation against a multi-byte query must not panic and
    /// must end at a valid UTF-8 char boundary (truncate_str guarantees this).
    #[test]
    fn tool_summary_truncates_multibyte_query_without_panicking() {
        // 100 three-byte CJK chars (300 bytes), exceeding the 60-byte cap.
        let long = "界".repeat(100);
        let line = tool_summary_line(&tool_call("compass_query", &long)).expect("non-blank query");
        assert!(line.starts_with('\''));
        assert!(line.ends_with("...'"));
        // Rebuild the inner label (no leading/trailing quotes + ellipsis) and
        // assert it is a valid slice of the original input, proving no chars
        // were split mid-boundary. Inner = line without outer quotes.
        let inner = line.trim_matches('\'').trim_end_matches("...");
        assert!(long.starts_with(inner), "inner must be a prefix: {inner:?}");
    }

    #[test]
    fn cap_tool_output_leaves_small_output_unchanged() {
        let output = ToolOutput::new("short output");
        let capped = cap_tool_output_for_history("bash", output.clone());
        assert_eq!(capped.output, output.output);
    }

    #[test]
    fn cap_tool_output_adds_visible_truncation_notice() {
        let output = ToolOutput::new("x".repeat(MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY + 10));
        let capped = cap_tool_output_for_history("bash", output);
        assert!(capped.output.len() < MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY + 1_000);
        assert!(capped.output.contains("Tool output truncated by jcode"));
        assert!(capped.output.contains("tool `bash` produced"));
        assert!(capped.output.contains("Redirect large logs to a file"));
    }

    #[test]
    fn cap_sdk_tool_content_adds_same_notice() {
        let capped = cap_sdk_tool_content_for_history(
            "custom",
            "y".repeat(MAX_TOOL_OUTPUT_CHARS_FOR_HISTORY + 10),
        );
        assert!(capped.contains("Tool output truncated by jcode"));
        assert!(capped.contains("tool `custom` produced"));
    }
}
