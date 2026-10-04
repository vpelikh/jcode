use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Map provider-side tool names to internal display names.
///
/// This is the canonical resolution plus the one display-only alias the
/// registry does not model: `file_glob` (the registry's `resolve_tool_name`
/// folds `file_grep` into `agentgrep` but has no `file_glob` entry, while the
/// transcript shows a dedicated `glob` row). Delegating keeps the friendly
/// label in lockstep with the alias set used for classification.
pub fn resolve_display_tool_name(name: &str) -> &str {
    let bare = name.strip_prefix("functions.").unwrap_or(name);
    match bare {
        "file_glob" => "glob",
        other => canonical_tool_name(other),
    }
}

/// Resolve a provider-side tool name to its canonical internal name.
///
/// This delegates to [`jcode_tool_types::resolve_tool_name`], the single source
/// of truth shared with the execution registry, so provider aliases such as
/// `file_edit`/`edit_file`/`write_file` and the `functions.` namespace cannot
/// silently stop being recognized on the display side. A tool name that fails
/// to resolve here is treated as "not an edit tool", which drops the diff
/// gutter entirely, so the alias set must stay in lockstep with the registry.
///
/// Two deliberate TUI-only divergences are layered on top:
/// - `grep`/`file_grep` stay distinct from their `agentgrep` identity so the
///   grep-specific summary arm (which reads `pattern`) still applies.
/// - The PascalCase patch-family names `MultiEdit`/`Patch`/`ApplyPatch` are not
///   part of the registry alias set but are emitted by some providers.
pub fn canonical_tool_name(name: &str) -> &str {
    let bare = name.strip_prefix("functions.").unwrap_or(name);
    match bare {
        "MultiEdit" => "multiedit",
        "Patch" => "patch",
        "ApplyPatch" => "apply_patch",
        "grep" | "file_grep" | "Grep" => "grep",
        other => jcode_tool_types::resolve_tool_name(other),
    }
}

pub fn is_edit_tool_name(name: &str) -> bool {
    matches!(
        canonical_tool_name(name),
        "write" | "edit" | "multiedit" | "patch" | "apply_patch"
    )
}

fn parse_nonzero_exit_code_line(line: &str) -> bool {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("Exit code:") {
        return rest
            .trim()
            .parse::<i32>()
            .map(|code| code != 0)
            .unwrap_or(false);
    }
    if let Some(rest) = trimmed.strip_prefix("--- Command finished with exit code:") {
        return rest
            .trim()
            .trim_end_matches('-')
            .trim()
            .parse::<i32>()
            .map(|code| code != 0)
            .unwrap_or(false);
    }
    false
}

fn display_prefix_by_width(s: &str, max_width: usize) -> &str {
    if max_width == 0 {
        return "";
    }
    let mut used = 0usize;
    let mut end = 0usize;
    for (idx, ch) in s.char_indices() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > max_width {
            break;
        }
        used += cw;
        end = idx + ch.len_utf8();
    }
    &s[..end]
}

fn display_suffix_by_width(s: &str, max_width: usize) -> &str {
    if max_width == 0 {
        return "";
    }
    let mut used = 0usize;
    let mut start = s.len();
    for (idx, ch) in s.char_indices().rev() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > max_width {
            break;
        }
        used += cw;
        start = idx;
    }
    &s[start..]
}

pub fn truncate_middle_display(s: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let remaining = max_width.saturating_sub(1);
    let head = remaining / 2 + remaining % 2;
    let tail = remaining / 2;
    format!(
        "{}…{}",
        display_prefix_by_width(s, head),
        display_suffix_by_width(s, tail)
    )
}

fn normalize_backticked_identifier(text: &str) -> String {
    text.replace('`', "").trim().to_string()
}

/// Peel a single leading remote-client transport label (`[<tool>] `), returning
/// the remainder. A label is any non-empty, single-line bracket token
/// immediately followed by `] `; this intentionally mirrors the long-standing
/// normalization in failure detection, so a first line that happens to open
/// with a bracketed token is treated as a label too.
fn peel_leading_tool_label(content: &str) -> Option<&str> {
    let after_open = content.strip_prefix('[')?;
    let (label, rest) = after_open.split_once("] ")?;
    (!label.is_empty() && !label.contains(['\n', '\r'])).then_some(rest)
}

/// Peel a leading harness timestamp header: either `[tool timing: ...]` or a
/// plain `[<rfc3339>]` tag (both are prepended to a tool result by
/// `jcode_message_types::Message::with_timestamps`).
fn peel_leading_tool_timestamp_header(content: &str) -> Option<&str> {
    let after_open = content.strip_prefix('[')?;
    let header_end = after_open.find(']')?;
    let header = &after_open[..header_end];
    (header.starts_with("tool timing:") || is_rfc3339_timestamp_tag(header))
        .then(|| after_open[header_end + 1..].trim_start())
}

/// Whether `tag` is the output of `Message::format_timestamp`:
/// `YYYY-MM-DDTHH:MM:SS.mmmZ` (chrono `to_rfc3339_opts(Millis, true)`).
/// Mirrors `jcode_message_types::is_rfc3339_timestamp_tag` so this crate need
/// not depend on chrono.
fn is_rfc3339_timestamp_tag(tag: &str) -> bool {
    const SHAPE: &[u8] = b"0000-00-00T00:00:00.000Z";
    let bytes = tag.as_bytes();
    if bytes.len() != SHAPE.len() {
        return false;
    }
    SHAPE.iter().enumerate().all(|(i, &expected)| {
        if expected == b'0' {
            bytes[i].is_ascii_digit()
        } else {
            bytes[i] == expected
        }
    })
}

/// Strip the leading transport decorations a tool result may carry before its
/// real first line: the remote client's `[<tool>] ` label and the harness's
/// `[tool timing: ...]` / `[<rfc3339>]` timestamp header. They may appear in
/// either order, so at most one of each is peeled (in whichever order they
/// lead). Each is removed once, so content that genuinely repeats a bracket
/// (e.g. a command echoing `[bash] `) is not eaten past the transport decoration.
fn strip_leading_transport(content: &str) -> &str {
    let mut content = content.trim_start();
    let mut stripped_label = false;
    let mut stripped_timestamp = false;
    loop {
        // Check the timestamp header first: it may contain an interior space
        // (`[tool timing: ...]`), which the looser label peel would otherwise
        // swallow as a label.
        if !stripped_timestamp && let Some(rest) = peel_leading_tool_timestamp_header(content) {
            content = rest;
            stripped_timestamp = true;
            continue;
        }
        if !stripped_label && let Some(rest) = peel_leading_tool_label(content) {
            content = rest;
            stripped_label = true;
            continue;
        }
        break;
    }
    content
}

/// Strip a transport label that sits *after* an error marker, e.g. the `[bash]`
/// in `Error: [bash] <msg>`.
///
/// Unlike [`strip_leading_transport`] this only peels a *single-token* label
/// (no interior whitespace). A remote label is always one token
/// (`[bash]`, `[compass_query]`, `[mcp__server__tool]`), whereas a genuine
/// bracket that opens an error message is usually multi-word
/// (`Error: [Errno 2] No such file or directory`). Requiring one token lets the
/// real detail survive untouched.
fn strip_error_marker_tool_label(content: &str) -> &str {
    let trimmed = content.trim();
    trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("] "))
        .filter(|(label, _)| {
            !label.is_empty()
                && !label.contains(['\n', '\r'])
                && !label.contains(char::is_whitespace)
        })
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed)
}

pub fn concise_tool_error_summary(content: &str) -> Option<String> {
    for raw_line in strip_leading_transport(content).lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        // Peel an error marker, then a transport label that may sit *after* it,
        // then another marker (the remote client wraps the already-labeled output
        // in `Error: `, so the real shape is `Error: [<tool>] <msg>` and a
        // tool-level `Error:` yields `Error: [<tool>] Error: <msg>`). Loop so any
        // wrapping order resolves to the innermost detail.
        let mut rest = line;
        let mut marked = false;
        loop {
            let detail = rest
                .strip_prefix("Error:")
                .or_else(|| rest.strip_prefix("error:"))
                .or_else(|| rest.strip_prefix("Failed:"))
                .map(str::trim);
            let Some(detail) = detail else {
                break;
            };
            marked = true;
            let after_label = strip_error_marker_tool_label(detail);
            if after_label.len() < detail.len() {
                rest = after_label;
                continue;
            }
            rest = detail;
            break;
        }
        if marked {
            let detail = rest;
            if let Some(field) = detail.strip_prefix("missing field ") {
                return Some(format!(
                    "invalid input: missing {}",
                    normalize_backticked_identifier(field)
                ));
            }
            if detail.starts_with("invalid type") || detail.starts_with("unknown variant") {
                return Some(format!("invalid input: {}", detail));
            }
            if detail.contains("source metadata") && detail.contains("was for") {
                return Some("build source changed before reload".to_string());
            }
            if detail.starts_with("Refusing to publish") {
                return Some("reload refused: rebuild against current source".to_string());
            }
            return Some(format!("error: {}", truncate_middle_display(detail, 80)));
        }

        if line.contains("Compile terminated by signal") {
            return Some(line.to_string());
        }
        if let Some(rest) = line.strip_prefix("Exit code:")
            && let Ok(code) = rest.trim().parse::<i32>()
            && code != 0
        {
            return Some(format!("exit {}", code));
        }
        if let Some(rest) = line.strip_prefix("--- Command finished with exit code:") {
            let code = rest.trim().trim_end_matches('-').trim();
            if code != "0" && !code.is_empty() {
                return Some(format!("exit {}", code));
            }
        }
    }

    None
}

/// Parse the numeric exit code embedded in a bash tool result, if present.
///
/// The bash tool appends `Exit code: N` (foreground/reload runs) or
/// `--- Command finished with exit code: N ---` (detached/background runs) as a
/// trailing footer. A successful run with no output surfaces the explicit
/// sentinel `Command completed successfully (no output)`, which is treated as
/// exit code 0. `None` is returned only when the outcome is genuinely unknown.
pub fn parse_bash_exit_code(content: &str) -> Option<i32> {
    let trimmed = content.trim();
    if trimmed == "Command completed successfully (no output)" {
        return Some(0);
    }
    for raw_line in content.lines().rev() {
        let line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("Exit code:") {
            return rest.trim().parse::<i32>().ok().filter(|_| !line.is_empty());
        }
        if let Some(rest) = line.strip_prefix("--- Command finished with exit code:") {
            let code = rest.trim().trim_end_matches('-').trim();
            if !code.is_empty() {
                return code.parse::<i32>().ok();
            }
        }
    }
    None
}

/// Parse the working directory recorded in a bash tool result's trailing
/// `Working directory: <path>` footer, if present. The bash tool appends this
/// footer (mirroring the `Exit code:` footer) so display surfaces can show
/// where the command actually ran without relying on the tool arguments.
pub fn parse_bash_working_dir(content: &str) -> Option<String> {
    for raw_line in content.lines().rev() {
        let line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("Working directory:") {
            let dir = rest.trim();
            if !dir.is_empty() {
                return Some(dir.to_string());
            }
        }
    }
    None
}

/// Parse the execution time from the bash tool's `Execution time: <dur>`
/// footer (e.g. `Execution time: 120ms`, `1.5s`, `2m`), if present. This footer
/// is part of the stored transcript content, so the duration shows reliably on
/// resume/replay (unlike the feature-gated `[tool timing]` header).
pub fn parse_bash_execution_time(content: &str) -> Option<String> {
    for raw_line in content.lines().rev() {
        let line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("Execution time:") {
            let value = rest.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Parse the execution time from the `[tool timing: ...]` header prepended to a
/// tool result's text content (see `jcode_message_types::Message::with_timestamps`).
/// Returns the humanized duration (e.g. `120ms`, `1.5s`, `2m`) when available.
pub fn parse_bash_timing_duration(content: &str) -> Option<String> {
    let trimmed = content.trim_start();
    // The timing header is normally the first token, but a remote client may
    // prepend its own `[<tool>] ` label first (`[bash] [tool timing: ...]`), and
    // either decoration may be trimmed off a mirrored transcript. The header is
    // always single-line, so search only the first line: this tolerates a
    // leading label without matching a `[tool timing:` that appears later inside
    // the command's own output.
    let first_line_end = trimmed.find('\n').unwrap_or(trimmed.len());
    let searchable = &trimmed[..first_line_end.min(200)];
    let header_start = searchable.find("[tool timing:")?;
    let after_open = &searchable[header_start + 1..];
    let header_end = after_open.find(']')?;
    let header = &after_open[..header_end];
    let header = header.strip_prefix("tool timing:")?;
    let duration = header
        .split_whitespace()
        .find(|part| part.starts_with("duration="))?;
    let value = duration.strip_prefix("duration=")?;
    parse_duration_value(value)
}

fn parse_duration_value(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches(']').trim();
    let lowercase = value.to_ascii_lowercase();
    if let Some(ms) = lowercase.strip_suffix("ms") {
        let ms: u64 = ms.parse().ok()?;
        return Some(humanize_duration_ms(ms));
    }
    if let Some(secs) = lowercase.strip_suffix('s') {
        let secs: f64 = secs.parse().ok()?;
        return Some(humanize_duration_ms((secs * 1000.0).round() as u64));
    }
    if let Ok(ms) = value.parse::<u64>() {
        return Some(humanize_duration_ms(ms));
    }
    None
}

fn humanize_duration_ms(duration_ms: u64) -> String {
    match duration_ms {
        0..=999 => format!("{}ms", duration_ms),
        1_000..=9_999 => format!("{:.1}s", duration_ms as f64 / 1000.0),
        10_000..=59_999 => format!("{}s", duration_ms / 1000),
        _ => {
            let total_seconds = duration_ms / 1000;
            let minutes = total_seconds / 60;
            let seconds = total_seconds % 60;
            if seconds == 0 {
                format!("{}m", minutes)
            } else {
                format!("{}m {}s", minutes, seconds)
            }
        }
    }
}

pub fn tool_output_looks_failed(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return false;
    }
    let normalized = strip_leading_transport(content);
    let lower = normalized.to_ascii_lowercase();
    if concise_tool_error_summary(normalized).is_some()
        || lower.starts_with("error:")
        || lower.starts_with("failed:")
        || normalized.starts_with('✗')
    {
        return true;
    }

    normalized.lines().any(|line| {
        let line = line.trim();
        parse_nonzero_exit_code_line(line)
            || line.eq_ignore_ascii_case("Status: failed")
            || line.eq_ignore_ascii_case("failed to start")
            || line.eq_ignore_ascii_case("terminated")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_edit_tool_names() {
        assert_eq!(canonical_tool_name("ApplyPatch"), "apply_patch");
        assert!(is_edit_tool_name("MultiEdit"));
        assert!(!is_edit_tool_name("read"));
    }

    #[test]
    fn display_name_covers_provider_aliases() {
        // The row label must show the friendly name for every alias the
        // canonical resolver recognizes, or aliased calls render as raw
        // provider names (`edit_file`, `write_file`, `functions.read`).
        assert_eq!(resolve_display_tool_name("edit_file"), "edit");
        assert_eq!(resolve_display_tool_name("file_edit"), "edit");
        assert_eq!(resolve_display_tool_name("write_file"), "write");
        assert_eq!(resolve_display_tool_name("file_write"), "write");
        assert_eq!(resolve_display_tool_name("read_file"), "read");
        assert_eq!(resolve_display_tool_name("file_read"), "read");
        assert_eq!(resolve_display_tool_name("shell"), "bash");
        assert_eq!(resolve_display_tool_name("functions.Edit"), "edit");
    }

    #[test]
    fn canonicalizes_provider_aliases_for_edit_tools() {
        // Provider aliases must resolve to the edit family so the diff gutter
        // renders even when the model emits `file_edit`/`edit_file`/`write_file`.
        assert_eq!(canonical_tool_name("file_edit"), "edit");
        assert_eq!(canonical_tool_name("edit_file"), "edit");
        assert_eq!(canonical_tool_name("file_write"), "write");
        assert_eq!(canonical_tool_name("write_file"), "write");
        assert!(is_edit_tool_name("file_edit"));
        assert!(is_edit_tool_name("edit_file"));
        assert!(is_edit_tool_name("write_file"));
        // The `functions.` transport namespace is stripped first.
        assert_eq!(canonical_tool_name("functions.Edit"), "edit");
        assert_eq!(canonical_tool_name("functions.file_write"), "write");
        // PascalCase OAuth names, including the patch family, resolve.
        assert_eq!(canonical_tool_name("Write"), "write");
        assert_eq!(canonical_tool_name("Edit"), "edit");
        assert!(is_edit_tool_name("Patch"));
        assert!(is_edit_tool_name("ApplyPatch"));
    }

    #[test]
    fn grep_aliases_keep_their_grep_identity() {
        // `grep`/`file_grep` must not collapse to `agentgrep` on the display
        // side, because the summary arm keys off the `grep` identity (and reads
        // `pattern`), while `agentgrep` reads `query`.
        assert_eq!(canonical_tool_name("grep"), "grep");
        assert_eq!(canonical_tool_name("file_grep"), "grep");
        assert_eq!(canonical_tool_name("Grep"), "grep");
        assert_eq!(canonical_tool_name("functions.file_grep"), "grep");
        assert_eq!(canonical_tool_name("agentgrep"), "agentgrep");
    }

    #[test]
    fn summarizes_tool_errors() {
        assert_eq!(
            concise_tool_error_summary("Error: missing field `command`").as_deref(),
            Some("invalid input: missing command")
        );
        assert_eq!(
            concise_tool_error_summary("--- Command finished with exit code: 2 ---").as_deref(),
            Some("exit 2")
        );
        // A remote `[<tool>] ` prefix must not hide the error summary.
        assert_eq!(
            concise_tool_error_summary("[bash] Error: missing field `command`").as_deref(),
            Some("invalid input: missing command")
        );
        // The real remote shape wraps the labeled output in `Error: ` (the
        // label sits *after* the marker): `Error: [<tool>] <msg>`.
        assert_eq!(
            concise_tool_error_summary("Error: [bash] missing field `command`").as_deref(),
            Some("invalid input: missing command")
        );
        // A doubly-wrapped error (tool already prefixed `Error:`, remote added
        // the label and another `Error:`) must still classify.
        assert_eq!(
            concise_tool_error_summary("Error: [bash] Error: missing field `command`").as_deref(),
            Some("invalid input: missing command")
        );
        // A genuine bracketed payload must not be mistaken for a transport label.
        assert_eq!(concise_tool_error_summary("[1, 2] ok"), None);
        // A single-token bracket after the marker is a transport label and is
        // peeled, even for a non-builtin tool name.
        assert_eq!(
            concise_tool_error_summary("Error: [mcp__server__tool] boom").as_deref(),
            Some("error: boom")
        );
        // A multi-word bracket after the marker is real error detail (e.g.
        // Python's `[Errno 2]`), so it must be preserved.
        assert_eq!(
            concise_tool_error_summary("Error: [Errno 2] No such file or directory").as_deref(),
            Some("error: [Errno 2] No such file or directory")
        );
        // The label and the harness `[tool timing: ...]` header may appear in
        // either order (the body renderers strip both); the error summary must
        // see through both orders too.
        assert_eq!(
            concise_tool_error_summary(
                "[bash] [tool timing: start=2026-01-01T00:00:00.000Z finish=2026-01-01T00:00:03.000Z duration=3s] Error: missing field `command`"
            )
            .as_deref(),
            Some("invalid input: missing command")
        );
        assert_eq!(
            concise_tool_error_summary(
                "[tool timing: start=2026-01-01T00:00:00.000Z finish=2026-01-01T00:00:03.000Z duration=3s] [bash] Error: missing field `command`"
            )
            .as_deref(),
            Some("invalid input: missing command")
        );
        // A plain `[<rfc3339>]` header (no duration) is also transport, alone or
        // with a label in either order.
        assert_eq!(
            concise_tool_error_summary("[2026-01-01T00:00:00.000Z] Error: missing field `command`")
                .as_deref(),
            Some("invalid input: missing command")
        );
        assert_eq!(
            concise_tool_error_summary(
                "[2026-01-01T00:00:00.000Z] [bash] Error: missing field `command`"
            )
            .as_deref(),
            Some("invalid input: missing command")
        );
        assert_eq!(
            concise_tool_error_summary(
                "[bash] [2026-01-01T00:00:00.000Z] Error: missing field `command`"
            )
            .as_deref(),
            Some("invalid input: missing command")
        );
        // Each decoration is peeled at most once, so a genuinely repeated label is
        // left as content and no longer opens with a marker (nothing to classify).
        assert_eq!(
            concise_tool_error_summary("[bash] [bash] Error: missing field `command`"),
            None
        );
    }

    #[test]
    fn strips_leading_transport_in_either_order() {
        assert_eq!(strip_leading_transport("[bash] Error: x"), "Error: x");
        assert_eq!(
            strip_leading_transport("[tool timing: duration=3s] Error: x"),
            "Error: x"
        );
        assert_eq!(
            strip_leading_transport("[bash] [tool timing: duration=3s] Error: x"),
            "Error: x"
        );
        assert_eq!(
            strip_leading_transport("[tool timing: duration=3s] [bash] Error: x"),
            "Error: x"
        );
        // A plain `[<rfc3339>]` timestamp header (the form `with_timestamps`
        // emits when there is no duration) is also transport, alone or with a
        // label in either order.
        assert_eq!(
            strip_leading_transport("[2026-01-01T00:00:00.000Z] Error: x"),
            "Error: x"
        );
        assert_eq!(
            strip_leading_transport("[2026-01-01T00:00:00.000Z] [bash] Error: x"),
            "Error: x"
        );
        assert_eq!(
            strip_leading_transport("[bash] [2026-01-01T00:00:00.000Z] Error: x"),
            "Error: x"
        );
        // The rfc3339 shape must match exactly: a non-timestamp bracket is not
        // treated as a timestamp header.
        assert!(!is_rfc3339_timestamp_tag("2026-01-01"));
        assert!(!is_rfc3339_timestamp_tag("tool timing: duration=3s"));
        assert!(is_rfc3339_timestamp_tag("2026-01-01T00:00:00.000Z"));
        // The leading peel is intentionally loose (any bracket token counts as a
        // label, matching long-standing failure detection); the post-marker peel
        // is the strict one that preserves `[Errno 2]`.
        assert_eq!(strip_leading_transport("[Errno 2] boom"), "boom");
        assert_eq!(strip_leading_transport("plain output"), "plain output");
    }

    #[test]
    fn detects_failed_tool_output() {
        assert!(tool_output_looks_failed("Status: failed"));
        assert!(tool_output_looks_failed("Exit code: 1"));
        assert!(tool_output_looks_failed(
            "✗ demo.txt: failed to find expected lines"
        ));
        assert!(tool_output_looks_failed(
            "[apply_patch] ✗ demo.txt: failed to find expected lines"
        ));
        assert!(!tool_output_looks_failed("Exit code: 0"));
        assert!(!tool_output_looks_failed("completed successfully"));
    }

    #[test]
    fn parses_bash_exit_code_from_foreground_and_detached_markers() {
        assert_eq!(parse_bash_exit_code("out\n\nExit code: 0"), Some(0));
        assert_eq!(parse_bash_exit_code("boom\n\nExit code: 2"), Some(2));
        assert_eq!(
            parse_bash_exit_code("done\n\n--- Command finished with exit code: 3 ---"),
            Some(3)
        );
        assert_eq!(
            parse_bash_exit_code("Command completed successfully (no output)"),
            Some(0),
            "the success sentinel must be treated as exit code 0 so the badge shows"
        );
        assert_eq!(parse_bash_exit_code("no marker here"), None);
    }

    #[test]
    fn parses_bash_timing_duration() {
        assert_eq!(
            parse_bash_timing_duration(
                "[tool timing: start=2026-01-01T00:00:00.000Z finish=2026-01-01T00:00:00.120Z duration=120ms] git status"
            )
            .as_deref(),
            Some("120ms")
        );
        assert_eq!(
            parse_bash_timing_duration("[tool timing: duration=1500ms] echo").as_deref(),
            Some("1.5s")
        );
        assert_eq!(
            parse_bash_timing_duration("[tool timing: duration=90s] echo").as_deref(),
            Some("1m 30s")
        );
        // A remote `[<tool>] ` label may precede the timing header
        // (`[bash] [tool timing: ...]`); the header must still be found.
        assert_eq!(
            parse_bash_timing_duration(
                "[bash] [tool timing: start=2026-01-01T00:00:00.000Z finish=2026-01-01T00:00:03.000Z duration=3s] git status"
            )
            .as_deref(),
            Some("3.0s")
        );
        assert_eq!(parse_bash_timing_duration("no timing header"), None);
        // A `[tool timing:` that appears on a later output line (not as the
        // prepended single-line header) must not be mistaken for the header.
        assert_eq!(
            parse_bash_timing_duration("some output\nthen [tool timing: duration=9s] later"),
            None
        );
    }

    #[test]
    fn parses_bash_execution_time_footer() {
        assert_eq!(
            parse_bash_execution_time("out\n\nExecution time: 120ms").as_deref(),
            Some("120ms")
        );
        assert_eq!(
            parse_bash_execution_time("out\n\nExecution time: 1.5s").as_deref(),
            Some("1.5s")
        );
        assert_eq!(parse_bash_execution_time("plain output"), None);
    }

    #[test]
    fn parses_bash_working_directory_footer() {
        assert_eq!(
            parse_bash_working_dir("On branch main\n\nWorking directory: /home/user/project")
                .as_deref(),
            Some("/home/user/project")
        );
        assert_eq!(
            parse_bash_working_dir("clean\n\nExit code: 0").as_deref(),
            None
        );
        assert_eq!(parse_bash_working_dir("plain output"), None);
    }

    #[test]
    fn humanizes_duration_units() {
        assert_eq!(humanize_duration_ms(500), "500ms");
        assert_eq!(humanize_duration_ms(2_000), "2.0s");
        assert_eq!(humanize_duration_ms(90_000), "1m 30s");
        assert_eq!(humanize_duration_ms(120_000), "2m");
    }
}
