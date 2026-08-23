use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Map provider-side tool names to internal display names.
/// Mirrors Registry::resolve_tool_name so TUI surfaces show friendly names.
pub fn resolve_display_tool_name(name: &str) -> &str {
    match name {
        "communicate" => "swarm",
        "discover_tools" => "integration_tools",
        "task" | "task_runner" => "subagent",
        "shell_exec" => "bash",
        "file_read" => "read",
        "file_write" => "write",
        "file_edit" => "edit",
        "file_glob" => "glob",
        "file_grep" => "grep",
        "todo_read" | "todo_write" | "todoread" | "todowrite" => "todo",
        other => other,
    }
}

pub fn canonical_tool_name(name: &str) -> &str {
    match name {
        "communicate" => "swarm",
        "discover_tools" => "integration_tools",
        "Write" => "write",
        "Edit" => "edit",
        "MultiEdit" => "multiedit",
        "Patch" => "patch",
        "ApplyPatch" => "apply_patch",
        other => other,
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

pub fn concise_tool_error_summary(content: &str) -> Option<String> {
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let detail = line
            .strip_prefix("Error:")
            .or_else(|| line.strip_prefix("error:"))
            .or_else(|| line.strip_prefix("Failed:"))
            .map(str::trim);
        if let Some(detail) = detail {
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
/// The bash tool appends one of two trailing lines when the command finishes:
/// `Exit code: N` (foreground runs) or `--- Command finished with exit code: N ---`
/// (detached/background runs). A successful run with no output surfaces the
/// placeholder `Command completed successfully (no output)` and carries no exit
/// marker, so callers should treat `None` as "exit unknown / no output" and
/// `Some(0)` as a confirmed success.
pub fn parse_bash_exit_code(content: &str) -> Option<i32> {
    for raw_line in content.lines().rev() {
        let line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("Exit code:") {
            return rest
                .trim()
                .parse::<i32>()
                .ok()
                .filter(|_| !line.is_empty());
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

/// Parse the execution time from the `[tool timing: ...]` header prepended to a
/// tool result's text content (see `jcode_message_types::Message::with_timestamps`).
/// Returns the humanized duration (e.g. `120ms`, `1.5s`, `2m`) when available.
pub fn parse_bash_timing_duration(content: &str) -> Option<String> {
    let trimmed = content.trim_start();
    // Search anywhere in the leading header so a trimmed/mirrored transcript
    // still finds it even if the header is not the very first token.
    let searchable = &trimmed[..trimmed.len().min(160)];
    let after_open = searchable.strip_prefix('[')?;
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
    let normalized = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("] "))
        .filter(|(label, _)| !label.is_empty() && !label.contains(['\n', '\r']))
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
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
    fn summarizes_tool_errors() {
        assert_eq!(
            concise_tool_error_summary("Error: missing field `command`").as_deref(),
            Some("invalid input: missing command")
        );
        assert_eq!(
            concise_tool_error_summary("--- Command finished with exit code: 2 ---").as_deref(),
            Some("exit 2")
        );
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
            None
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
        assert_eq!(parse_bash_timing_duration("no timing header"), None);
    }

    #[test]
    fn parses_bash_working_directory_footer() {
        assert_eq!(
            parse_bash_working_dir(
                "On branch main\n\nWorking directory: /home/user/project"
            )
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
