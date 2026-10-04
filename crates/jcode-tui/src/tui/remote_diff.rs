use serde_json::Value;
use similar::TextDiff;
use std::collections::HashMap;
use std::path::PathBuf;

use super::ui::tools_ui;

/// Tracks a pending file edit for diff generation.
pub(crate) struct PendingFileDiff {
    pub(crate) file_path: String,
    pub(crate) original_content: String,
}

#[derive(Default)]
pub(crate) struct RemoteDiffTracker {
    pub(crate) pending_diffs: HashMap<String, PendingFileDiff>,
    pub(crate) current_tool_id: Option<String>,
    pub(crate) current_tool_name: Option<String>,
    pub(crate) current_tool_input: String,
}

impl RemoteDiffTracker {
    pub(crate) fn handle_tool_start(&mut self, id: &str, name: &str) {
        self.current_tool_id = Some(id.to_string());
        self.current_tool_name = Some(name.to_string());
        self.current_tool_input.clear();
    }

    pub(crate) fn handle_tool_input(&mut self, delta: &str) {
        self.current_tool_input.push_str(delta);
    }

    pub(crate) fn current_tool_input_json(&self) -> Value {
        serde_json::from_str(&self.current_tool_input).unwrap_or(Value::Null)
    }

    pub(crate) fn handle_tool_exec(&mut self, id: &str, name: &str) {
        if show_diffs_enabled()
            && tools_ui::is_edit_tool_name(name)
            && let Ok(input) = serde_json::from_str::<Value>(&self.current_tool_input)
            && let Some(file_path) = edit_tool_file_path(name, &input)
        {
            let resolved = resolve_diff_path(&file_path);
            let original = std::fs::read_to_string(&resolved).unwrap_or_default();
            self.pending_diffs.insert(
                id.to_string(),
                PendingFileDiff {
                    file_path: resolved.to_string_lossy().to_string(),
                    original_content: original,
                },
            );
        }

        self.current_tool_id = None;
        self.current_tool_name = None;
        self.current_tool_input.clear();
    }

    /// Finalize a remote tool result into its display content.
    ///
    /// The returned string leads with a `[<tool>] ` label. That label is
    /// transport framing for the TUI's own row rendering, not part of the tool's
    /// real result, so every consumer that parses this content (body renderers,
    /// error summaries) must strip it first — see
    /// `ui_messages::strip_tool_result_transport_headers` and
    /// `jcode-tui-tool-display`'s `strip_leading_transport`.
    ///
    /// Do not strip the label here instead: the label is not the only transport
    /// decoration, and some parsers (`parse_bash_timing_duration`,
    /// `parse_bash_working_dir`) need the raw `[tool timing: ...]` header and
    /// trailing footers that a blanket normalization would also remove. Each
    /// consumer therefore strips exactly the decorations it owns.
    pub(crate) fn finish_tool(&mut self, id: &str, name: &str, output: &str) -> String {
        if let Some(pending) = self.pending_diffs.remove(id) {
            let new_content = std::fs::read_to_string(&pending.file_path).unwrap_or_default();
            let diff =
                generate_unified_diff(&pending.original_content, &new_content, &pending.file_path);
            if !diff.is_empty() {
                return format!("[{}] {}\n{}", name, pending.file_path, diff);
            }
        }

        format!("[{}] {}", name, output)
    }

    pub(crate) fn clear(&mut self) {
        self.pending_diffs.clear();
        self.current_tool_id = None;
        self.current_tool_name = None;
        self.current_tool_input.clear();
    }
}

/// Resolve the target file for an edit-family tool call.
///
/// `edit`/`write`/`multiedit` carry `file_path` directly. `patch` and
/// `apply_patch` carry only `patch_text`, so their primary path is extracted
/// instead of silently producing no diff.
fn edit_tool_file_path(name: &str, input: &Value) -> Option<String> {
    if let Some(path) = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .filter(|p| !p.trim().is_empty())
    {
        return Some(path.to_string());
    }

    let patch_text = input.get("patch_text").and_then(|v| v.as_str())?;
    match tools_ui::canonical_tool_name(name) {
        "apply_patch" => tools_ui::extract_apply_patch_primary_file(patch_text),
        "patch" => tools_ui::extract_unified_patch_primary_file(patch_text),
        _ => None,
    }
}

/// Check if client-side diff generation is enabled.
pub(crate) fn show_diffs_enabled() -> bool {
    std::env::var("JCODE_SHOW_DIFFS")
        .map(|v| v != "0" && v.to_lowercase() != "false")
        .unwrap_or(true)
}

/// Resolve a file path for client-side diff generation.
/// Expands `~` to home directory and resolves relative paths against cwd.
pub(crate) fn resolve_diff_path(raw: &str) -> PathBuf {
    let expanded = if let Some(stripped) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(stripped)
        } else {
            PathBuf::from(raw)
        }
    } else {
        PathBuf::from(raw)
    };

    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().unwrap_or_default().join(expanded)
    }
}

/// Generate a unified diff between two strings.
pub(crate) fn generate_unified_diff(old: &str, new: &str, file_path: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut output = String::new();

    output.push_str(&format!("--- a/{}\n", file_path));
    output.push_str(&format!("+++ b/{}\n", file_path));

    for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
        output.push_str(&format!("{}", hunk));
    }

    output
}

#[cfg(test)]
mod tests {
    use super::{RemoteDiffTracker, edit_tool_file_path};
    use serde_json::json;

    #[test]
    fn remote_tracker_captures_diff_for_aliased_edit_tool() {
        // Drive the real public interface: start -> input -> exec -> finish.
        // An aliased edit tool must still produce a unified diff for the file,
        // exercising is_edit_tool_name and the path resolver end to end.
        let dir = std::env::temp_dir().join(format!("jcode-rdiff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("demo.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let mut tracker = RemoteDiffTracker::default();
        tracker.handle_tool_start("t1", "file_edit");
        tracker.handle_tool_input(&json!({ "file_path": path_str }).to_string());
        tracker.handle_tool_exec("t1", "file_edit");

        // The tool would rewrite the file; emulate the post-edit content.
        std::fs::write(&path, "one\nTWO\n").unwrap();
        let rendered = tracker.finish_tool("t1", "file_edit", "done");

        assert!(
            rendered.contains("-two") && rendered.contains("+TWO"),
            "aliased remote edit did not produce a diff: {rendered:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolves_file_path_for_direct_edit_tools() {
        let input = json!({ "file_path": "src/lib.rs" });
        assert_eq!(
            edit_tool_file_path("edit", &input).as_deref(),
            Some("src/lib.rs")
        );
        // Aliases resolve to the same direct-path behavior.
        assert_eq!(
            edit_tool_file_path("file_edit", &input).as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            edit_tool_file_path("write_file", &input).as_deref(),
            Some("src/lib.rs")
        );
    }

    #[test]
    fn resolves_patch_text_primary_file_for_patch_tools() {
        let apply = json!({
            "patch_text": "*** Begin Patch\n*** Update File: crates/a/src/lib.rs\n@@\n-old\n+new\n*** End Patch\n"
        });
        assert_eq!(
            edit_tool_file_path("apply_patch", &apply).as_deref(),
            Some("crates/a/src/lib.rs")
        );

        let unified = json!({ "patch_text": "--- a/src/f.rs\n+++ b/src/f.rs\n@@\n-old\n+new\n" });
        assert_eq!(
            edit_tool_file_path("patch", &unified).as_deref(),
            Some("src/f.rs")
        );
    }

    #[test]
    fn returns_none_without_a_resolvable_path() {
        assert_eq!(edit_tool_file_path("edit", &json!({})), None);
        assert_eq!(edit_tool_file_path("apply_patch", &json!({})), None);
        assert_eq!(
            edit_tool_file_path("patch", &json!({ "patch_text": "no headers here" })),
            None
        );
        // A blank file_path must not shadow the patch_text fallback.
        assert_eq!(
            edit_tool_file_path("edit", &json!({ "file_path": "  " })),
            None
        );
    }
}
