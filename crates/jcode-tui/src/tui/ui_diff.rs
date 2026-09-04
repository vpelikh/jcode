use crate::{message::ToolCall, tui::ui::tools_ui};
use ratatui::prelude::*;

pub(super) fn diff_add_color() -> Color {
    Color::Rgb(100, 200, 100)
}

pub(super) fn diff_del_color() -> Color {
    Color::Rgb(200, 100, 100)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DiffLineKind {
    Add,
    Del,
}

#[derive(Clone, Debug)]
pub(super) struct ParsedDiffLine {
    pub kind: DiffLineKind,
    pub prefix: String,
    pub content: String,
    pub file_path: Option<String>,
}

pub(super) fn diff_change_counts(content: &str) -> (usize, usize) {
    let lines = filter_unnumbered_prose_lines(collect_diff_lines(content));
    let additions = lines
        .iter()
        .filter(|line| line.kind == DiffLineKind::Add)
        .count();
    let deletions = lines
        .iter()
        .filter(|line| line.kind == DiffLineKind::Del)
        .count();
    (additions, deletions)
}

pub(super) fn diff_change_counts_for_tool(tool: &ToolCall, content: &str) -> (usize, usize) {
    let (additions, deletions) = diff_change_counts(content);
    if additions > 0 || deletions > 0 {
        return (additions, deletions);
    }

    match tools_ui::canonical_tool_name(&tool.name) {
        "edit" => {
            diff_counts_from_input_pair(&tool.input, "old_string", "new_string").unwrap_or((0, 0))
        }
        "write" => {
            let content = tool
                .input
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            diff_counts_from_strings("", content)
        }
        "multiedit" => diff_counts_from_multiedit(&tool.input).unwrap_or((0, 0)),
        "patch" => diff_counts_from_unified_patch_input(&tool.input).unwrap_or((0, 0)),
        "apply_patch" => diff_counts_from_apply_patch_input(&tool.input).unwrap_or((0, 0)),
        _ => (additions, deletions),
    }
}

fn diff_counts_from_input_pair(
    input: &serde_json::Value,
    old_key: &str,
    new_key: &str,
) -> Option<(usize, usize)> {
    let old = input.get(old_key)?.as_str()?;
    let new = input.get(new_key)?.as_str()?;
    Some(diff_counts_from_strings(old, new))
}

fn diff_counts_from_multiedit(input: &serde_json::Value) -> Option<(usize, usize)> {
    let edits = input.get("edits")?.as_array()?;
    let mut additions = 0usize;
    let mut deletions = 0usize;

    for edit in edits {
        let old = edit
            .get("old_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let new = edit
            .get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if old.is_empty() && new.is_empty() {
            continue;
        }
        let (add, del) = diff_counts_from_strings(old, new);
        additions += add;
        deletions += del;
    }

    Some((additions, deletions))
}

fn diff_counts_from_unified_patch_input(input: &serde_json::Value) -> Option<(usize, usize)> {
    let patch_text = input.get("patch_text")?.as_str()?;
    let mut additions = 0usize;
    let mut deletions = 0usize;

    for line in patch_text.lines() {
        if line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("@@")
            || line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("\\ No newline")
        {
            continue;
        }
        if line.starts_with('+') {
            additions += 1;
        } else if line.starts_with('-') {
            deletions += 1;
        }
    }

    Some((additions, deletions))
}

fn diff_counts_from_apply_patch_input(input: &serde_json::Value) -> Option<(usize, usize)> {
    let patch_text = input.get("patch_text")?.as_str()?;
    let mut additions = 0usize;
    let mut deletions = 0usize;

    for line in patch_text.lines() {
        if line.starts_with("***") || line.starts_with("@@") {
            continue;
        }

        if line.starts_with('+') {
            additions += 1;
        } else if line.starts_with('-') {
            deletions += 1;
        }
    }

    Some((additions, deletions))
}

fn diff_counts_from_strings(old: &str, new: &str) -> (usize, usize) {
    use similar::ChangeTag;

    let diff = similar::TextDiff::from_lines(old, new);
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => additions += 1,
            ChangeTag::Delete => deletions += 1,
            ChangeTag::Equal => {}
        }
    }
    (additions, deletions)
}

pub(super) fn generate_diff_lines_from_tool_input(tool: &ToolCall) -> Vec<ParsedDiffLine> {
    match tools_ui::canonical_tool_name(&tool.name) {
        "edit" => {
            let old = tool
                .input
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = tool
                .input
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            generate_diff_lines_from_strings(old, new)
        }
        "multiedit" => {
            let Some(edits) = tool.input.get("edits").and_then(|v| v.as_array()) else {
                return Vec::new();
            };
            let mut all_lines = Vec::new();
            for edit in edits {
                let old = edit
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let new = edit
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                all_lines.extend(generate_diff_lines_from_strings(old, new));
            }
            all_lines
        }
        "write" => {
            let content = tool
                .input
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            generate_diff_lines_from_strings("", content)
        }
        "patch" => {
            let patch_text = tool
                .input
                .get("patch_text")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            collect_diff_lines(patch_text)
        }
        "apply_patch" => {
            let patch_text = tool
                .input
                .get("patch_text")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            collect_diff_lines(patch_text)
        }
        _ => Vec::new(),
    }
}

fn generate_diff_lines_from_strings(old: &str, new: &str) -> Vec<ParsedDiffLine> {
    use similar::ChangeTag;

    let diff = similar::TextDiff::from_lines(old, new);
    let mut lines = Vec::new();

    for change in diff.iter_all_changes() {
        let content = change.value().trim();
        if content.is_empty() {
            continue;
        }

        match change.tag() {
            ChangeTag::Delete => {
                lines.push(ParsedDiffLine {
                    kind: DiffLineKind::Del,
                    prefix: format!("{}- ", change.old_index().unwrap_or(0) + 1),
                    content: content.to_string(),
                    file_path: None,
                });
            }
            ChangeTag::Insert => {
                lines.push(ParsedDiffLine {
                    kind: DiffLineKind::Add,
                    prefix: format!("{}+ ", change.new_index().unwrap_or(0) + 1),
                    content: content.to_string(),
                    file_path: None,
                });
            }
            ChangeTag::Equal => {}
        }
    }

    lines
}

pub(super) fn collect_diff_lines(content: &str) -> Vec<ParsedDiffLine> {
    let mut file_path = None;
    let mut lines = Vec::new();

    for raw_line in content.lines() {
        if let Some(path) = diff_file_path(raw_line) {
            file_path = Some(path);
            continue;
        }
        if let Some(mut line) = parse_diff_line(raw_line) {
            line.file_path = file_path.clone();
            lines.push(line);
        }
    }

    lines
}

/// Whether a parsed diff line carries a numeric line number in its prefix.
///
/// Edit-family tools emit numbered prefixes like `42- ` / `42+ `. A line whose
/// prefix has no leading number came from the bare `- `/`+ ` glyph fallback in
/// [`parse_diff_line`], which also matches markdown bullets such as the ones a
/// config-edit notice appends ("- `key`: old -> new"). Those are prose, not
/// deletions, and must not be surfaced as unnumbered diff lines.
fn has_line_number(line: &ParsedDiffLine) -> bool {
    line.prefix.chars().any(|c| c.is_ascii_digit())
}

/// Whether a line is a config-notice markdown bullet, i.e. a bare `- ` glyph
/// followed by a backtick-led key (``- `key`: old -> new``). These can appear
/// even when the edit produced no numbered diff (e.g. a write to config.toml
/// with unchanged content), so they need removing on their own signature.
fn is_config_notice_bullet(line: &ParsedDiffLine) -> bool {
    !has_line_number(line)
        && line.kind == DiffLineKind::Del
        && line.content.trim_start().starts_with('`')
}

/// Drop unnumbered (prose) lines from a set of parsed diff lines.
///
/// In an edit-family tool's output the numbered lines are the real diff; any
/// unnumbered `- `/`+ ` line alongside them is a markdown bullet or notice
/// prose and would only blank out a line number in the rendered diff. Lines
/// that look like config-notice bullets are dropped on their own signature too,
/// so a write to config.toml that produced no numbered diff still excludes
/// them. All other unnumbered lines are kept when no numbered line is present,
/// so plain (legitimately unnumbered) diffs are unaffected.
pub(super) fn filter_unnumbered_prose_lines(lines: Vec<ParsedDiffLine>) -> Vec<ParsedDiffLine> {
    let has_numbered = lines.iter().any(has_line_number);
    lines.into_iter().filter(|l| {
        if is_config_notice_bullet(l) {
            return false;
        }
        !has_numbered || has_line_number(l)
    }).collect()
}

fn diff_file_path(raw_line: &str) -> Option<String> {
    let trimmed = raw_line.trim();
    if let Some(path) = trimmed
        .strip_prefix("*** Add File: ")
        .or_else(|| trimmed.strip_prefix("*** Update File: "))
        .or_else(|| trimmed.strip_prefix("*** Delete File: "))
    {
        return non_empty_diff_path(path);
    }

    if let Some(path) = trimmed.strip_prefix("+++ ") {
        return unified_diff_path(path);
    }
    if let Some(path) = trimmed.strip_prefix("--- ") {
        return unified_diff_path(path);
    }

    let status = trimmed
        .strip_prefix('✓')
        .or_else(|| trimmed.strip_prefix('✗'))?
        .trim_start();
    let (path, _) = status.split_once(": ")?;
    non_empty_diff_path(path)
}

fn unified_diff_path(raw_path: &str) -> Option<String> {
    let path = raw_path
        .split('\t')
        .next()
        .unwrap_or(raw_path)
        .split_whitespace()
        .next()
        .unwrap_or("");
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    non_empty_diff_path(path)
}

fn non_empty_diff_path(path: &str) -> Option<String> {
    let path = path.trim();
    (!path.is_empty() && path != "/dev/null").then(|| path.to_string())
}

fn parse_diff_line(raw_line: &str) -> Option<ParsedDiffLine> {
    let trimmed = raw_line.trim();
    if trimmed.is_empty() || trimmed == "..." {
        return None;
    }
    if trimmed.starts_with("diff --git ")
        || trimmed.starts_with("index ")
        || trimmed.starts_with("--- ")
        || trimmed.starts_with("+++ ")
        || trimmed.starts_with("@@ ")
        || trimmed.starts_with("\\ No newline")
    {
        return None;
    }

    if let Some(pos) = trimmed.find("- ") {
        let (prefix, content) = trimmed.split_at(pos + 2);
        if !prefix.is_empty() && prefix[..pos].chars().all(|c| c.is_ascii_digit()) {
            return Some(ParsedDiffLine {
                kind: DiffLineKind::Del,
                prefix: prefix.to_string(),
                content: trim_diff_content(content),
                file_path: None,
            });
        }
    }
    if let Some(pos) = trimmed.find("+ ") {
        let (prefix, content) = trimmed.split_at(pos + 2);
        if !prefix.is_empty() && prefix[..pos].chars().all(|c| c.is_ascii_digit()) {
            return Some(ParsedDiffLine {
                kind: DiffLineKind::Add,
                prefix: prefix.to_string(),
                content: trim_diff_content(content),
                file_path: None,
            });
        }
    }

    if let Some(rest) = raw_line.strip_prefix('+') {
        return Some(ParsedDiffLine {
            kind: DiffLineKind::Add,
            prefix: "+".to_string(),
            content: trim_diff_content(rest),
            file_path: None,
        });
    }
    if let Some(rest) = raw_line.strip_prefix('-') {
        return Some(ParsedDiffLine {
            kind: DiffLineKind::Del,
            prefix: "-".to_string(),
            content: trim_diff_content(rest),
            file_path: None,
        });
    }

    None
}

fn trim_diff_content(content: &str) -> String {
    content.trim_start_matches([' ', '\t']).to_string()
}

pub(super) fn tint_span_with_diff_color(span: Span<'static>, diff_color: Color) -> Span<'static> {
    let (dr, dg, db) = match diff_color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(n) => super::color_support::indexed_to_rgb(n),
        _ => return span,
    };

    let fg = span.style.fg.unwrap_or(Color::White);
    let (sr, sg, sb) = match fg {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(n) => super::color_support::indexed_to_rgb(n),
        Color::White => (255, 255, 255),
        Color::Black => (0, 0, 0),
        _ => return span,
    };

    let blend = |s: u8, d: u8| -> u8 { ((s as u16 * 70 + d as u16 * 30) / 100) as u8 };

    let tinted = Color::Rgb(blend(sr, dr), blend(sg, dg), blend(sb, db));
    Span::styled(span.content, span.style.fg(tinted))
}

#[cfg(test)]
mod tests {
    use super::{
        DiffLineKind, collect_diff_lines, diff_change_counts, diff_change_counts_for_tool,
        diff_counts_from_apply_patch_input, filter_unnumbered_prose_lines,
        generate_diff_lines_from_strings,
    };
    use crate::message::ToolCall;
    use serde_json::json;

    #[test]
    fn apply_patch_counts_ignore_context_lines_with_plus_or_minus_prefixes() {
        let input = json!({
            "patch_text": "*** Begin Patch\n*** Update File: demo.txt\n@@\n  +context line\n  -context line\n+added line\n-deleted line\n*** End Patch\n"
        });

        assert_eq!(diff_counts_from_apply_patch_input(&input), Some((1, 1)));
    }

    #[test]
    fn collected_apply_patch_lines_retain_file_boundaries() {
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-old a\n+new a\n*** Update File: b.txt\n@@\n-old b\n+new b\n*** End Patch\n";

        let lines = collect_diff_lines(patch);

        assert_eq!(lines.len(), 4);
        assert!(
            lines[..2]
                .iter()
                .all(|line| line.file_path.as_deref() == Some("a.txt"))
        );
        assert!(
            lines[2..]
                .iter()
                .all(|line| line.file_path.as_deref() == Some("b.txt"))
        );
    }

    #[test]
    fn write_tool_falls_back_to_content_diff_counts() {
        let tool = ToolCall {
            id: "tool_1".to_string(),
            name: "write".to_string(),
            input: json!({
                "file_path": "demo.txt",
                "content": "first line\nsecond line\n"
            }),
            intent: None,
            thought_signature: None,
        };

        assert_eq!(diff_change_counts_for_tool(&tool, ""), (2, 0));
    }

    #[test]
    fn multiedit_pascal_case_falls_back_to_input_diff_counts() {
        let tool = ToolCall {
            id: "tool_2".to_string(),
            name: "MultiEdit".to_string(),
            input: json!({
                "file_path": "demo.txt",
                "edits": [
                    {"old_string": "two\n", "new_string": "TWO\n"},
                    {"old_string": "three\n", "new_string": "THREE\n"}
                ]
            }),
            intent: None,
            thought_signature: None,
        };

        assert_eq!(diff_change_counts_for_tool(&tool, ""), (2, 2));
    }

    #[test]
    fn generated_diff_lines_use_old_and_new_line_numbers() {
        let lines =
            generate_diff_lines_from_strings("one\ntwo\nthree\n", "one\nthree\nfour\nfive\n");

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].kind, DiffLineKind::Del);
        assert_eq!(lines[0].prefix, "2- ");
        assert_eq!(lines[1].kind, DiffLineKind::Add);
        assert_eq!(lines[1].prefix, "3+ ");
        assert_eq!(lines[2].kind, DiffLineKind::Add);
        assert_eq!(lines[2].prefix, "4+ ");
    }

    #[test]
    fn config_notice_markdown_bullets_are_stripped_from_edit_diff() {
        // The config-edit notice appends markdown bullets like
        // "- `x`: a -> b (live now)" to the edit tool body. They parse as
        // unnumbered diff lines and would blank out their line numbers in the
        // rendered diff. When a numbered diff is present, the consumer filter
        // must drop these unnumbered prose lines.
        let lines = collect_diff_lines(&format!(
            "2- two\n2+ TWO\n\nConfig changes:\n- `key`: old -> new (live now)\n"
        ));
        // Raw parse keeps the numbered diff plus the unnumbered bullet.
        assert_eq!(lines.len(), 3, "unexpected parse: {:?}", lines);

        let filtered = filter_unnumbered_prose_lines(lines);
        assert_eq!(filtered.len(), 2, "markdown bullet leaked into diff: {:?}", filtered);
        assert!(filtered.iter().all(|l| l.prefix.chars().any(|c| c.is_ascii_digit())));
        assert!(!filtered.iter().any(|l| l.content.contains("Config changes")));
    }

    #[test]
    fn filter_keeps_plain_unnumbered_diff_when_no_numbered_lines() {
        // A genuinely unnumbered diff (no numbered lines at all) keeps all its
        // deletions, so the filter never discards real edits.
        let lines = collect_diff_lines("- only\n+ pair");
        let filtered = filter_unnumbered_prose_lines(lines);
        assert_eq!(filtered.len(), 2, "plain diff was wrongly pruned: {:?}", filtered);
    }

    #[test]
    fn diff_change_counts_exclude_config_notice_bullets() {
        // The (+N -M) badge must not count a config-notice markdown bullet as a
        // deletion, which would overstate the diff (here 1 add / 1 del, not 1/2).
        let content = "Edited config.toml\n2- old\n2+ new\n\nConfig changes:\n- `key`: old -> new (live now)\n";
        assert_eq!(diff_change_counts(content), (1, 1), "badge counts leaked bullet");
    }

    #[test]
    fn config_notice_bullets_stripped_even_without_numbered_diff() {
        // A write to config.toml that changed no content emits only the config
        // notice (no numbered diff). Its bullet must still not surface as a
        // deletion row.
        let content =
            "Updated config.toml (12 lines)\n\nConfig changes:\n- `key`: old -> new (live now)\n";
        let filtered = filter_unnumbered_prose_lines(collect_diff_lines(content));
        assert_eq!(filtered.len(), 0, "config-only body leaked a diff row: {:?}", filtered);
        assert_eq!(diff_change_counts(content), (0, 0), "badge counted config bullet");
    }
}
