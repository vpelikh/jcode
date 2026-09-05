use super::commands::{
    REVIEW_PREFERRED_MODEL, active_session_id, active_working_dir, todo_confidence_summary,
};
use super::review_loop;
use super::{App, DisplayMessage};
use crate::id;
use crate::message::{ContentBlock, Role, ToolCall};
use crate::session::{Session, StoredMessage};
use std::time::Instant;

/// A coarse signature of the working tree's tracked/untracked changes, used to
/// detect whether a review-loop fix turn actually touched files. Returns `None`
/// when the session is not in a git repo. We deliberately do not shell out to a
/// full diff — `status --porcelain` is enough to distinguish "files changed"
/// from "no change", which is all the stall cap needs.
fn working_tree_signature(cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort_unstable();
    let joined = lines.join("\n");
    if joined.trim().is_empty() {
        Some(String::new())
    } else {
        Some(joined)
    }
}

/// Extract the set of file paths referenced by a `git status --porcelain`
/// signature, used to record which files a review-loop fix turn actually
/// touched. Handles the common states (` M`, `A `, `??`, ` D`) and renames
/// (`R100 old -> new`).
fn changed_files_from_signature(sig: &str) -> Vec<String> {
    use std::collections::BTreeSet;
    let mut files = BTreeSet::new();
    for line in sig.lines() {
        let line = line.trim_start();
        if line.is_empty() {
            continue;
        }
        // Skip the 2-char status columns, then any rename/copy score digits
        // (the porcelain form is `R100 old -> new`), then the separating space.
        let bytes = line.as_bytes();
        let mut idx = 2.min(bytes.len());
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
            idx += 1;
        }
        let rest = &line[idx..];
        if rest.is_empty() {
            continue;
        }
        // Renames look like "old -> new"; keep the destination path.
        let path = match rest.split_once(" -> ") {
            Some((_, new)) => new.trim(),
            None => rest.trim(),
        };
        if !path.is_empty() {
            files.insert(path.to_string());
        }
    }
    files.into_iter().collect()
}

fn review_session_read_only_guardrails() -> &'static str {
    "Important constraints for this session:\n\
- This session is analysis-only. Do not do the work yourself.\n\
- Do not modify files or repo state. Do not call `edit`, `write`, `multiedit`, `patch`, `apply_patch`, or destructive `bash`/`git` commands.\n\
- Do not continue implementation, fix issues, or take follow-up actions yourself.\n\
- If additional work is needed, describe it in your DM to the parent session instead.\n\
\n"
}

fn judge_session_visible_context_notice() -> &'static str {
    "Important context for this judge session:\n\
- This session contains a user-visible mirror of the parent conversation, not the full original implementation context.\n\
- It includes the user's prompts, the assistant's visible replies, and shallow summaries of visible tool calls.\n\
- It intentionally omits deep tool-result details and hidden internal context beyond what the user could see.\n\
- Base your judgment on this mirror, then verify claims by inspecting repo state or tests directly when needed.\n\
\n"
}

fn is_judge_session_title(title: Option<&str>) -> bool {
    matches!(title, Some("judge" | "autojudge"))
}

fn is_analysis_feedback_session_title(title: Option<&str>) -> bool {
    matches!(title, Some("review" | "autoreview" | "judge" | "autojudge"))
}

fn resolve_feedback_target_session_id(session_id: &str) -> String {
    let mut current_id = session_id.to_string();

    for _ in 0..16 {
        let Ok(session) = Session::load(&current_id) else {
            break;
        };

        if !is_analysis_feedback_session_title(session.title.as_deref()) {
            return current_id;
        }

        let Some(parent_id) = session.parent_id.clone() else {
            return current_id;
        };

        if parent_id == current_id {
            return current_id;
        }

        current_id = parent_id;
    }

    current_id
}

pub(super) fn current_feedback_target_session_id(app: &App) -> String {
    resolve_feedback_target_session_id(&active_session_id(app))
}

fn judge_transcript_text_message(role: Role, text: String) -> StoredMessage {
    StoredMessage {
        id: id::new_id("message"),
        role,
        content: vec![ContentBlock::Text {
            text,
            cache_control: None,
        }],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    }
}

fn truncate_judge_visible_text(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let truncated: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", truncated.trim_end())
}

fn judge_visible_value_summary(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(v) => Some(v.to_string()),
        serde_json::Value::Number(v) => Some(v.to_string()),
        serde_json::Value::String(v) => Some(truncate_judge_visible_text(v, 120)),
        serde_json::Value::Array(values) => Some(format!(
            "{} item{}",
            values.len(),
            if values.len() == 1 { "" } else { "s" }
        )),
        serde_json::Value::Object(map) => Some(format!(
            "{} field{}",
            map.len(),
            if map.len() == 1 { "" } else { "s" }
        )),
    }
}

fn judge_visible_tool_summary(tool: &ToolCall) -> Option<String> {
    let obj = tool.input.as_object()?;
    let preferred_keys = [
        "file_path",
        "command",
        "pattern",
        "query",
        "url",
        "path",
        "subject",
        "channel",
        "action",
        "description",
        "task_id",
        "target_session",
        "to_session",
        "model",
        "reason",
    ];
    let mut parts = Vec::new();
    for key in preferred_keys {
        let Some(value) = obj.get(key) else {
            continue;
        };
        let Some(summary) = judge_visible_value_summary(value) else {
            continue;
        };
        if summary.is_empty() {
            continue;
        }
        parts.push(format!("{}={}", key, summary));
        if parts.len() >= 2 {
            break;
        }
    }

    if parts.is_empty() {
        if obj.contains_key("patch_text") {
            let lines = obj
                .get("patch_text")
                .and_then(|v| v.as_str())
                .map(|text| text.lines().count())
                .unwrap_or(0);
            return Some(format!("patch_text={} lines", lines));
        }
        if obj.contains_key("tool_calls") {
            let count = obj
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .map(|items| items.len())
                .unwrap_or(0);
            return Some(format!(
                "tool_calls={} item{}",
                count,
                if count == 1 { "" } else { "s" }
            ));
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

fn build_judge_visible_transcript_messages(parent_session: &Session) -> Vec<StoredMessage> {
    // A judge must never see the parent's private chain-of-thought. Clone the
    // parent and drop reasoning/thinking blocks so `render_messages` (which
    // otherwise re-renders reasoning under the Full display mode) has no
    // reasoning left to include, regardless of the current display setting.
    let mut visible = parent_session.clone();
    for message in &mut visible.messages {
        message.content.retain(|block| {
            !matches!(
                block,
                ContentBlock::Reasoning { .. }
                    | ContentBlock::ReasoningTrace { .. }
                    | ContentBlock::AnthropicThinking { .. }
                    | ContentBlock::OpenAIReasoning { .. }
            )
        });
    }

    let mut transcript = Vec::new();

    for rendered in crate::session::render_messages(&visible) {
        match rendered.role.as_str() {
            "user" => {
                if !rendered.content.trim().is_empty() {
                    transcript.push(judge_transcript_text_message(
                        Role::User,
                        rendered.content.trim().to_string(),
                    ));
                }
            }
            "assistant" => {
                let mut text = rendered.content.trim().to_string();
                if !rendered.tool_calls.is_empty() {
                    let visible_tools = rendered
                        .tool_calls
                        .iter()
                        .map(|name| format!("`{}`", name))
                        .collect::<Vec<_>>()
                        .join(", ");
                    if text.is_empty() {
                        text = format!(
                            "Visible tool call{}: {}",
                            if rendered.tool_calls.len() == 1 {
                                ""
                            } else {
                                "s"
                            },
                            visible_tools
                        );
                    } else {
                        text.push_str(&format!(
                            "\n\nVisible tool call{}: {}",
                            if rendered.tool_calls.len() == 1 {
                                ""
                            } else {
                                "s"
                            },
                            visible_tools
                        ));
                    }
                }
                if !text.trim().is_empty() {
                    transcript.push(judge_transcript_text_message(Role::Assistant, text));
                }
            }
            "tool" => {
                let text = if let Some(tool) = rendered.tool_data.as_ref() {
                    let status = if rendered.content.trim_start().starts_with("Error:")
                        || rendered.content.trim_start().starts_with("error:")
                        || rendered.content.trim_start().starts_with("Failed:")
                    {
                        "failed"
                    } else {
                        "completed"
                    };
                    let summary = judge_visible_tool_summary(tool)
                        .map(|summary| format!(" - {}", summary))
                        .unwrap_or_default();
                    format!(
                        "Visible tool call: `{}`{} ({}). Detailed tool output is intentionally omitted from this judge transcript.",
                        tool.name, summary, status
                    )
                } else {
                    "Visible tool call completed. Detailed tool output is intentionally omitted from this judge transcript.".to_string()
                };
                transcript.push(judge_transcript_text_message(Role::Assistant, text));
            }
            "system" => {}
            _ => {}
        }
    }

    transcript
}

fn apply_judge_visible_context_if_needed(session: &mut Session, title_override: Option<&str>) {
    let effective_title = title_override.or(session.title.as_deref());
    if !is_judge_session_title(effective_title) {
        return;
    }

    let Some(parent_session_id) = session.parent_id.clone() else {
        return;
    };
    let Ok(parent_session) = Session::load(&parent_session_id) else {
        return;
    };

    let transcript = build_judge_visible_transcript_messages(&parent_session);
    session.replace_messages(transcript);
    session.compaction = None;
    session.provider_session_id = None;
    // `replace_messages` emits a ReplaceMessages event but the compaction clear
    // above is a direct write; rebuild the event log so derive_compaction()
    // agrees with the cleared legacy state.
    session.rebuild_event_map();
}

/// Drop every side panel page belonging to the discarded session (#605).
///
/// The server only emits `SidePanelState` when a page is written, so nothing
/// else ever tells the client to drop the old session's pages. Shared by both
/// `/clear` implementations so they cannot drift apart again.
pub(crate) fn clear_side_panel_for_new_session(app: &mut App) {
    app.apply_side_panel_snapshot(crate::side_panel::SidePanelSnapshot::default());
    app.last_side_panel_focus_id = None;
    app.diff_pane_scroll = 0;
    app.diff_pane_scroll_x = 0;
}

pub(super) fn reset_current_session(app: &mut App) {
    app.session.mark_closed();
    let _ = app.session.save();
    app.clear_provider_messages();
    app.clear_display_messages();
    // A streaming mermaid preview (STREAMING_PREVIEW_DIAGRAM) belongs to the
    // transcript being discarded; clear it with the rest of the streaming
    // render state so it cannot outlive the reset (remote /clear at
    // remote/key_handling.rs does the same).
    app.clear_streaming_render_state();
    app.clear_live_usage_state();
    // The WHOLE transcript is discarded, so every entry in the process-global
    // ACTIVE_DIAGRAMS registry is now orphaned; drop them so the pinned pane
    // and the Margin info widget (which draws get_active_diagrams()[0])
    // cannot keep showing a diagram from the old transcript. Only
    // full-discard paths may do this: partial-retention paths (/rewind,
    // Ctrl+R recovery) deliberately keep the registry because body-cache
    // prefix reuse means retained messages do not re-render/re-register
    // (see the comments at the /rewind handlers in commands.rs).
    crate::tui::mermaid::clear_active_diagrams();
    app.swarm_plan_items.clear();
    app.swarm_plan_version = None;
    app.swarm_plan_swarm_id = None;
    app.queued_messages.clear();
    app.pasted_contents.clear();
    app.pending_images.clear();
    app.clear_inline_image_state();
    app.active_skill = None;
    app.improve_mode = None;
    let mut session = Session::create(None, None);
    session.mark_active();
    session.model = Some(app.provider.model());
    session.provider_key = crate::session::derive_session_provider_key(app.provider.name());
    session.autoreview_enabled = Some(app.autoreview_enabled);
    session.autojudge_enabled = Some(app.autojudge_enabled);
    session.ensure_initial_session_context_message();
    app.session = session;
    clear_side_panel_for_new_session(app);
    app.provider_session_id = None;
}

fn observe_status_message(app: &App) -> String {
    format!(
        "Observe mode: {}\n\nWhen enabled, the side panel shows a transient Observe page with only the latest useful tool call or tool result added to context. UI/bookkeeping tools like side_panel, goal, and todo reads/writes are skipped so the view stays readable. It is not persisted to disk.",
        if app.observe_mode_enabled() {
            "enabled"
        } else {
            "disabled"
        }
    )
}

pub(super) fn handle_observe_command(app: &mut App, trimmed: &str) -> bool {
    if !trimmed.starts_with("/observe") {
        return false;
    }

    let arg = trimmed.strip_prefix("/observe").unwrap_or_default().trim();
    match arg {
        "" => {
            let enabled = !app.observe_mode_enabled();
            app.set_observe_mode_enabled(enabled, true);
            if enabled {
                app.set_status_notice("Observe: ON");
                app.push_display_message(DisplayMessage::system(
                    "Observe mode enabled - the side panel now tracks the latest useful tool call/result added to context."
                        .to_string(),
                ));
            } else {
                app.set_status_notice("Observe: OFF");
                app.push_display_message(DisplayMessage::system(
                    "Observe mode disabled.".to_string(),
                ));
            }
        }
        "on" => {
            app.set_observe_mode_enabled(true, true);
            app.set_status_notice("Observe: ON");
            app.push_display_message(DisplayMessage::system(
                "Observe mode enabled - the side panel now tracks the latest useful tool call/result added to context."
                    .to_string(),
            ));
        }
        "off" => {
            app.set_observe_mode_enabled(false, false);
            app.set_status_notice("Observe: OFF");
            app.push_display_message(DisplayMessage::system("Observe mode disabled.".to_string()));
        }
        "status" => {
            app.push_display_message(DisplayMessage::system(observe_status_message(app)));
        }
        _ => {
            app.push_display_message(DisplayMessage::error(
                "Usage: /observe [on|off|status]".to_string(),
            ));
        }
    }

    true
}

fn current_autoreview_model_summary(app: &App) -> String {
    crate::config::config()
        .autoreview
        .model
        .clone()
        .or_else(|| app.session.model.clone())
        .unwrap_or_else(|| app.provider.model())
}

fn current_autoreview_model_override() -> Option<String> {
    crate::config::config().autoreview.model.clone()
}

fn current_autojudge_model_summary(app: &App) -> String {
    crate::config::config()
        .autojudge
        .model
        .clone()
        .or_else(|| app.session.model.clone())
        .unwrap_or_else(|| app.provider.model())
}

fn current_autojudge_model_override() -> Option<String> {
    crate::config::config().autojudge.model.clone()
}

pub(super) fn autoreview_status_message(app: &App) -> String {
    let default_enabled = crate::config::config().autoreview.enabled;
    let config_model = crate::config::config().autoreview.model.as_deref();
    let model_line = match config_model {
        Some(model) => format!("Reviewer model override: {}", model),
        None => format!(
            "Reviewer model: inherit current session ({})",
            current_autoreview_model_summary(app)
        ),
    };
    format!(
        "Autoreview: {} (config default: {})\n{}",
        if app.autoreview_enabled {
            "enabled"
        } else {
            "disabled"
        },
        if default_enabled {
            "enabled"
        } else {
            "disabled"
        },
        model_line,
    )
}

pub(super) fn autojudge_status_message(app: &App) -> String {
    let default_enabled = crate::config::config().autojudge.enabled;
    let config_model = crate::config::config().autojudge.model.as_deref();
    let model_line = match config_model {
        Some(model) => format!("Judge model override: {}", model),
        None => format!(
            "Judge model: inherit current session ({})",
            current_autojudge_model_summary(app)
        ),
    };
    format!(
        "Autojudge: {} (config default: {})\n{}",
        if app.autojudge_enabled {
            "enabled"
        } else {
            "disabled"
        },
        if default_enabled {
            "enabled"
        } else {
            "disabled"
        },
        model_line,
    )
}

pub(super) fn build_autoreview_startup_message(parent_session_id: &str) -> String {
    format!(
        "You are the automatic reviewer for parent session `{}`.\n\
Your job is to inspect the just-finished work and decide whether a review is needed.\n\
\n\
First read only the conversation history you actually need:\n\
1. Use `conversation_search` with `stats=true` to learn the history size.\n\
2. Read the most recent turns with `conversation_search turns` (start with roughly the last 6-12 turns, then widen only if needed).\n\
3. If requirements are unclear, use `conversation_search query` to find the latest relevant user request or acceptance criteria.\n\
\n\
{}\
Then determine whether review is needed. Review is needed if the recent work likely changed code, config, docs, tests, tooling behavior, or made technical claims worth validating. If the recent turn was purely conversational or administrative, no review is needed.\n\
\n\
If no review is needed:\n\
- Send exactly one DM to session `{}` using `communicate` with action `dm`.\n\
- Briefly explain why no review was needed.\n\
- Then stop.\n\
\n\
If review is needed:\n\
- Inspect the actual repo changes with targeted commands such as `git diff --stat`, `git diff --name-only`, and focused file reads.\n\
- Perform a concise code review. Look for correctness bugs, regressions, missing validation, missing tests, edge cases, unsafe behavior, or broken assumptions. Prefer concrete findings over style comments.\n\
- When finished, send exactly one DM to session `{}` summarizing:\n\
  - whether review was needed\n\
  - any findings with severity and file paths\n\
  - or `No issues found` if the work looks good\n\
- After sending the DM, stop.\n\
\n\
Do not ask the user anything unless absolutely necessary. Keep your own session concise.",
        parent_session_id,
        review_session_read_only_guardrails(),
        parent_session_id,
        parent_session_id
    )
}

pub(super) fn build_autojudge_startup_message(parent_session_id: &str) -> String {
    format!(
        "You are the automatic judge for parent session `{}`.\n\
Your job is to act like a strong completion manager/reviewer for the parent agent.\n\
Your purpose is not just to critique. Your purpose is to decide whether the parent agent should keep going, and if so, tell it exactly what to do next. Only tell it to stop when the user's best likely intent has been carried through thoughtfully and completely.\n\
\n\
First read only the conversation history you actually need:\n\
1. Use `conversation_search` with `stats=true` to learn the history size.\n\
2. Read the most recent turns with `conversation_search turns` (start with roughly the last 6-12 turns, then widen only if needed).\n\
3. If requirements are unclear, use `conversation_search query` to find the latest relevant user request, constraints, preferences, or acceptance criteria.\n\
\n\
{}{}\
Then determine whether a judgment pass is needed. It is needed if the recent work likely changed code, docs, tests, tooling behavior, repo state, or made claims about what was completed. If the recent turn was purely conversational or administrative, no judgment is needed.\n\
\n\
If no judgment is needed:\n\
- Send exactly one DM to session `{}` using `communicate` with action `dm`.\n\
- Start the DM with `STOP:` and briefly explain why no judgment was needed.\n\
- Then stop.\n\
\n\
If judgment is needed:\n\
- Inspect the actual repo changes with targeted commands such as `git diff --stat`, `git diff --name-only`, focused file reads, and relevant tests or validation commands when warranted.\n\
- Evaluate: intent alignment, completeness, initiative, approach quality, correctness, validation quality, and whether obvious next steps were missed.\n\
- Prefer concrete findings over vague commentary. Call out if the work stopped after one pass when more follow-through was clearly needed.\n\
- Be strict about incomplete execution. If the parent likely stopped too early, missed obvious follow-through, only implemented a narrow slice of the user's intent, skipped validation, or left a refactor/feature half-finished, you should tell it to continue.\n\
- Default to `CONTINUE:` unless you are genuinely convinced the work is complete, well-executed, and ready to stop.\n\
- When finished, send exactly one DM to session `{}` summarizing:\n\
  - Start with either `CONTINUE:` or `STOP:`\n\
  - `CONTINUE:` means the parent should immediately keep working. Include the concrete missing follow-through, better interpretation of user intent, and the next steps to execute now. Be specific and action-oriented.\n\
  - `STOP:` means the work is aligned, thoughtful, complete, and it is fine for the parent to stop. Briefly say why the completion bar is met.\n\
  - Mention file paths, validation gaps, correctness concerns, or missed next steps when relevant.\n\
- After sending the DM, stop.\n\
\n\
Do not ask the user anything unless absolutely necessary. Keep your own session concise. Address the DM to the parent agent, not to the user.",
        parent_session_id,
        judge_session_visible_context_notice(),
        review_session_read_only_guardrails(),
        parent_session_id,
        parent_session_id
    )
}

pub(super) fn build_review_startup_message(parent_session_id: &str) -> String {
    format!(
        "You are the one-shot reviewer for parent session `{}`.\n\
Your job is to inspect the recent work, determine whether a review is needed, and perform that review if needed.\n\
\n\
First read only the conversation history you actually need:\n\
1. Use `conversation_search` with `stats=true` to learn the history size.\n\
2. Read the most recent turns with `conversation_search turns` (start with roughly the last 6-12 turns, then widen only if needed).\n\
3. If requirements are unclear, use `conversation_search query` to find the latest relevant user request or acceptance criteria.\n\
\n\
{}\
Then determine whether review is needed. Review is needed if the recent work likely changed code, config, docs, tests, tooling behavior, or made technical claims worth validating. If the recent turn was purely conversational or administrative, no review is needed.\n\
\n\
If no review is needed:\n\
- Send exactly one DM to session `{}` using `communicate` with action `dm`.\n\
- Briefly explain why no review was needed.\n\
- Then stop.\n\
\n\
If review is needed:\n\
- Inspect the actual repo changes with targeted commands such as `git diff --stat`, `git diff --name-only`, and focused file reads.\n\
- Perform a concise code review. Look for correctness bugs, regressions, missing validation, missing tests, edge cases, unsafe behavior, or broken assumptions. Prefer concrete findings over style comments.\n\
- When finished, send exactly one DM to session `{}` summarizing:\n\
  - whether review was needed\n\
  - any findings with severity and file paths\n\
  - or `No issues found` if the work looks good\n\
- After sending the DM, stop.\n\
\n\
Do not ask the user anything unless absolutely necessary. Keep your own session concise.",
        parent_session_id,
        review_session_read_only_guardrails(),
        parent_session_id,
        parent_session_id
    )
}

pub(super) fn build_judge_startup_message(parent_session_id: &str) -> String {
    format!(
        "You are the one-shot judge for parent session `{}`.\n\
Your job is to inspect the recent work, determine whether a judgment pass is needed, and perform that judgment if needed.\n\
{}\
\n\
First read only the conversation history you actually need:\n\
1. Use `conversation_search` with `stats=true` to learn the history size.\n\
2. Read the most recent turns with `conversation_search turns` (start with roughly the last 6-12 turns, then widen only if needed).\n\
3. If requirements are unclear, use `conversation_search query` to find the latest relevant user request, constraints, preferences, or acceptance criteria.\n\
\n\
{}\
Then determine whether a judgment pass is needed. It is needed if the recent work likely changed code, docs, tests, tooling behavior, repo state, or made claims about what was completed. If the recent turn was purely conversational or administrative, no judgment is needed.\n\
\n\
If no judgment is needed:\n\
- Send exactly one DM to session `{}` using `communicate` with action `dm`.\n\
- Briefly explain why no judgment was needed.\n\
- Then stop.\n\
\n\
If judgment is needed:\n\
- Inspect the actual repo changes with targeted commands such as `git diff --stat`, `git diff --name-only`, focused file reads, and relevant tests or validation commands when warranted.\n\
- Evaluate: intent alignment, completeness, initiative, approach quality, correctness, validation quality, and whether obvious next steps were missed.\n\
- Prefer concrete findings over vague commentary. Call out if the work stopped after one pass when more follow-through was clearly needed.\n\
- When finished, send exactly one DM to session `{}` summarizing:\n\
  - whether judgment was needed\n\
  - whether the work looks complete and well-executed\n\
  - any findings with severity and file paths when relevant\n\
  - specific missing follow-through or better next steps if the execution was incomplete or low-agency\n\
  - or `Looks good` if the work is aligned, thoughtful, and complete\n\
- After sending the DM, stop.\n\
\n\
Do not ask the user anything unless absolutely necessary. Keep your own session concise.",
        parent_session_id,
        judge_session_visible_context_notice(),
        review_session_read_only_guardrails(),
        parent_session_id,
        parent_session_id
    )
}

pub(super) fn preferred_one_shot_review_override() -> Option<(String, String)> {
    let creds = crate::auth::codex::load_credentials().ok()?;
    let has_oauth = !creds.refresh_token.trim().is_empty() || creds.id_token.is_some();
    if has_oauth {
        Some((REVIEW_PREFERRED_MODEL.to_string(), "openai".to_string()))
    } else {
        None
    }
}

fn current_review_model_override() -> (Option<String>, Option<String>) {
    preferred_one_shot_review_override()
        .map(|(model, provider_key)| (Some(model), Some(provider_key)))
        .unwrap_or_else(|| (current_autoreview_model_override(), None))
}

fn current_judge_model_override() -> (Option<String>, Option<String>) {
    preferred_one_shot_review_override()
        .map(|(model, provider_key)| (Some(model), Some(provider_key)))
        .unwrap_or_else(|| (current_autojudge_model_override(), None))
}

fn clone_session_for_review(
    app: &App,
    session_title: &str,
    initial_model: String,
    provider_key_override: Option<String>,
) -> anyhow::Result<(String, String)> {
    let parent_session_id = current_feedback_target_session_id(app);
    let mut child = Session::create(Some(parent_session_id), Some(session_title.to_string()));
    child.replace_messages(app.session.messages.clone());
    child.compaction = app.session.compaction.clone();
    child.working_dir = app.session.working_dir.clone();
    child.model = Some(initial_model);
    child.provider_key = provider_key_override.or_else(|| app.session.provider_key.clone());
    child.subagent_model = app.session.subagent_model.clone();
    child.reasoning_effort = app.session.reasoning_effort.clone();
    child.autoreview_enabled = Some(false);
    child.autojudge_enabled = Some(false);
    child.status = crate::session::SessionStatus::Closed;
    child.rebuild_event_map();
    child.save()?;
    Ok((child.id.clone(), child.display_name().to_string()))
}

fn clone_session_for_prompt(app: &App) -> anyhow::Result<(String, String)> {
    let parent_session_id = active_session_id(app);
    let mut child = Session::create(Some(parent_session_id.clone()), None);
    child.replace_messages(app.session.messages.clone());
    child.compaction = app.session.compaction.clone();
    child.working_dir = app.session.working_dir.clone();
    child.model = app.session.model.clone();
    child.provider_key = app.session.provider_key.clone();
    child.subagent_model = app.session.subagent_model.clone();
    child.autoreview_enabled = app.session.autoreview_enabled;
    child.autojudge_enabled = app.session.autojudge_enabled;
    child.status = crate::session::SessionStatus::Closed;
    // The parent agent keeps ownership of any in-flight request; tell the
    // forked agent so it treats the next prompt as fresh work instead of
    // continuing (and duplicating) the parent's current turn.
    child.append_fork_notice(&parent_session_id, app.session.display_name());
    child.rebuild_event_map();
    child.save()?;
    Ok((child.id.clone(), child.display_name().to_string()))
}

pub(super) fn prepare_review_spawned_session(
    session_id: &str,
    startup_message: String,
    model_override: Option<String>,
    provider_key_override: Option<String>,
    title_override: Option<String>,
    parent_session_id_override: Option<String>,
) {
    if let Ok(mut session) = crate::session::Session::load(session_id) {
        session.autoreview_enabled = Some(false);
        session.autojudge_enabled = Some(false);
        if let Some(parent_session_id) = parent_session_id_override {
            session.parent_id = Some(parent_session_id);
        }
        if let Some(title) = title_override.clone() {
            session.title = Some(title);
        }
        if let Some(model) = model_override {
            session.model = Some(model);
        }
        if provider_key_override.is_some() {
            session.provider_key = provider_key_override;
        }
        apply_judge_visible_context_if_needed(&mut session, title_override.as_deref());
        let _ = session.save();
    }
    App::save_startup_message_for_session(session_id, startup_message);
}

pub(super) fn launch_prompt_in_new_session_local(
    app: &mut App,
    content: String,
    images: Vec<(String, String)>,
) -> anyhow::Result<bool> {
    launch_forked_session_local(app, Some((content, images)))
}

/// Fork (split) the current session into a new window. When `prompt` is
/// provided it is staged as the first submission of the forked session;
/// otherwise the fork opens idle with the cloned conversation.
pub(super) fn launch_forked_session_local(
    app: &mut App,
    prompt: Option<(String, Vec<(String, String)>)>,
) -> anyhow::Result<bool> {
    let (session_id, session_name) = clone_session_for_prompt(app)?;
    let has_prompt = prompt.is_some();
    if let Some((content, images)) = prompt {
        App::save_startup_submission_for_session(&session_id, content, images);
    }
    let exe = super::launch_client_executable();
    let cwd = active_working_dir(app)
        .filter(|path| path.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let socket = std::env::var("JCODE_SOCKET").ok();
    let opened = super::spawn_in_new_terminal(&exe, &session_id, &cwd, socket.as_deref())?;
    match (opened, has_prompt) {
        (true, true) => {
            app.push_display_message(DisplayMessage::system(format!(
                "↗ Next prompt launched in {}.",
                session_name
            )));
            app.set_status_notice("Prompt launched in new session");
        }
        (true, false) => {
            app.push_display_message(DisplayMessage::system(format!(
                "✂ Fork → {} (opened in new pane/window)",
                session_name
            )));
            app.set_status_notice(format!("Fork → {}", session_name));
        }
        (false, true) => {
            app.push_display_message(DisplayMessage::system(format!(
                "↗ New session {} created for the next prompt.\n\nNo terminal was opened automatically. Resume manually:\n\n  jcode --resume {}",
                session_name, session_id
            )));
            app.set_status_notice("Prompt session created");
        }
        (false, false) => {
            app.push_display_message(DisplayMessage::system(format!(
                "✂ Fork → {}\n\nNo terminal was opened automatically. Resume manually:\n\n  jcode --resume {}",
                session_name, session_id
            )));
            app.set_status_notice("Forked session created");
        }
    }
    Ok(opened)
}

fn launch_review_window_local(
    app: &mut App,
    session_title: &str,
    label: &str,
    startup_message: String,
    model_override: Option<String>,
    provider_key_override: Option<String>,
) -> anyhow::Result<bool> {
    let initial_model = model_override
        .clone()
        .unwrap_or_else(|| current_autoreview_model_summary(app));
    let (session_id, session_name) = clone_session_for_review(
        app,
        session_title,
        initial_model,
        provider_key_override.clone(),
    )?;
    prepare_review_spawned_session(
        &session_id,
        startup_message,
        model_override,
        provider_key_override,
        Some(session_title.to_string()),
        None,
    );
    let exe = super::launch_client_executable();
    let cwd = active_working_dir(app)
        .filter(|path| path.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let socket = std::env::var("JCODE_SOCKET").ok();
    let opened = super::spawn_in_new_terminal(&exe, &session_id, &cwd, socket.as_deref())?;
    if opened {
        app.push_display_message(DisplayMessage::system(format!(
            "🔍 {} launched in {}.",
            label, session_name
        )));
        app.set_status_notice(format!("{} launched", label));
    } else {
        app.push_display_message(DisplayMessage::system(format!(
            "🔍 {} session {} created.\n\nNo terminal was opened automatically. Resume manually:\n\n  jcode --resume {}",
            label, session_name, session_id
        )));
        app.set_status_notice(format!("{} session created", label));
    }
    Ok(opened)
}

fn launch_autoreview_window_local(app: &mut App) -> anyhow::Result<bool> {
    let parent_session_id = current_feedback_target_session_id(app);
    launch_review_window_local(
        app,
        "autoreview",
        "Autoreview",
        build_autoreview_startup_message(&parent_session_id),
        current_autoreview_model_override(),
        None,
    )
}

fn launch_review_once_local(app: &mut App) -> anyhow::Result<bool> {
    let (model_override, provider_key_override) = current_review_model_override();
    let parent_session_id = current_feedback_target_session_id(app);
    launch_review_window_local(
        app,
        "review",
        "Review",
        build_review_startup_message(&parent_session_id),
        model_override,
        provider_key_override,
    )
}

fn launch_autojudge_window_local(app: &mut App) -> anyhow::Result<bool> {
    let parent_session_id = current_feedback_target_session_id(app);
    launch_review_window_local(
        app,
        "autojudge",
        "Autojudge",
        build_autojudge_startup_message(&parent_session_id),
        current_autojudge_model_override(),
        None,
    )
}

fn launch_judge_once_local(app: &mut App) -> anyhow::Result<bool> {
    let (model_override, provider_key_override) = current_judge_model_override();
    let parent_session_id = current_feedback_target_session_id(app);
    launch_review_window_local(
        app,
        "judge",
        "Judge",
        build_judge_startup_message(&parent_session_id),
        model_override,
        provider_key_override,
    )
}

pub(super) fn queue_review_spawn_remote(
    app: &mut App,
    label: &str,
    parent_session_id: String,
    startup_message: String,
    model_override: Option<String>,
    provider_key_override: Option<String>,
) {
    app.pending_split_parent_session_id = Some(parent_session_id);
    app.pending_split_startup_message = Some(startup_message);
    app.pending_split_model_override = model_override;
    app.pending_split_provider_key_override = provider_key_override;
    app.pending_split_label = Some(label.to_string());
    app.pending_split_started_at = Some(Instant::now());
    app.pending_split_request = true;
    app.set_status_notice(format!("{} queued", label));
}

#[cfg(test)]
pub(super) fn queue_autojudge_remote(app: &mut App) {
    if !app.autojudge_enabled
        || app.pending_split_request
        || app.pending_split_startup_message.is_some()
    {
        return;
    }
    let parent_session_id = current_feedback_target_session_id(app);
    queue_review_spawn_remote(
        app,
        "Autojudge",
        parent_session_id.clone(),
        build_autojudge_startup_message(&parent_session_id),
        current_autojudge_model_override(),
        None,
    );
}

pub(super) fn maybe_trigger_autoreview_local(app: &mut App) {
    if !app.autoreview_enabled || app.is_remote || app.is_replay {
        return;
    }
    // Match maybe_enter_review_loop: never launch a reviewer window under the
    // unit-test harness (would open a live review child/terminal, making
    // todo-completion tests non-deterministic).
    if app.runtime_mode == super::AppRuntimeMode::TestHarness {
        return;
    }
    // When loop_mode is enabled, the review loop replaces the one-shot
    // autoreview entirely. Suppress the one-shot to avoid double review.
    if crate::config::config().autoreview.loop_mode {
        return;
    }
    if let Err(error) = launch_autoreview_window_local(app) {
        app.push_display_message(DisplayMessage::error(format!(
            "Failed to launch autoreview: {}",
            error
        )));
        app.set_status_notice("Autoreview launch failed");
    }
}

pub(super) fn maybe_trigger_autojudge_local(app: &mut App) {
    if !app.autojudge_enabled || app.is_remote || app.is_replay {
        return;
    }
    if let Err(error) = launch_autojudge_window_local(app) {
        app.push_display_message(DisplayMessage::error(format!(
            "Failed to launch autojudge: {}",
            error
        )));
        app.set_status_notice("Autojudge launch failed");
    }
}

pub(super) fn handle_review_command_local(app: &mut App, trimmed: &str) -> bool {
    if !trimmed.starts_with("/review") {
        return false;
    }

    let rest = trimmed.strip_prefix("/review").unwrap_or_default().trim();

    if rest.is_empty() {
        if let Err(error) = launch_review_once_local(app) {
            app.push_display_message(DisplayMessage::error(format!(
                "Failed to launch review: {}",
                error
            )));
            app.set_status_notice("Review launch failed");
        }
        return true;
    }

    app.push_display_message(DisplayMessage::error("Usage: /review".to_string()));
    true
}

pub(super) fn handle_autoreview_command_local(app: &mut App, trimmed: &str) -> bool {
    if !trimmed.starts_with("/autoreview") {
        return false;
    }

    let rest = trimmed
        .strip_prefix("/autoreview")
        .unwrap_or_default()
        .trim();

    if rest.is_empty() || matches!(rest, "status" | "show") {
        app.push_display_message(DisplayMessage::system(autoreview_status_message(app)));
        return true;
    }

    match rest {
        "on" => {
            app.set_autoreview_feature_enabled(true);
            let _ = app.session.save();
            app.push_display_message(DisplayMessage::system(
                "Autoreview enabled for this session.".to_string(),
            ));
            app.set_status_notice("Autoreview: ON");
            true
        }
        "off" => {
            app.set_autoreview_feature_enabled(false);
            let _ = app.session.save();
            app.push_display_message(DisplayMessage::system(
                "Autoreview disabled for this session.".to_string(),
            ));
            app.set_status_notice("Autoreview: OFF");
            true
        }
        "now" => {
            if let Err(error) = launch_autoreview_window_local(app) {
                app.push_display_message(DisplayMessage::error(format!(
                    "Failed to launch autoreview: {}",
                    error
                )));
                app.set_status_notice("Autoreview launch failed");
            }
            true
        }
        _ => {
            app.push_display_message(DisplayMessage::error(
                "Usage: /autoreview [on|off|status|now]".to_string(),
            ));
            true
        }
    }
}

pub(super) fn handle_judge_command_local(app: &mut App, trimmed: &str) -> bool {
    if !trimmed.starts_with("/judge") {
        return false;
    }

    let rest = trimmed.strip_prefix("/judge").unwrap_or_default().trim();

    if rest.is_empty() {
        if let Err(error) = launch_judge_once_local(app) {
            app.push_display_message(DisplayMessage::error(format!(
                "Failed to launch judge: {}",
                error
            )));
            app.set_status_notice("Judge launch failed");
        }
        return true;
    }

    app.push_display_message(DisplayMessage::error("Usage: /judge".to_string()));
    true
}

pub(super) fn handle_autojudge_command_local(app: &mut App, trimmed: &str) -> bool {
    if !trimmed.starts_with("/autojudge") {
        return false;
    }

    let rest = trimmed
        .strip_prefix("/autojudge")
        .unwrap_or_default()
        .trim();

    if rest.is_empty() || matches!(rest, "status" | "show") {
        app.push_display_message(DisplayMessage::system(autojudge_status_message(app)));
        return true;
    }

    match rest {
        "on" => {
            app.set_autojudge_feature_enabled(true);
            let _ = app.session.save();
            app.push_display_message(DisplayMessage::system(
                "Autojudge enabled for this session.".to_string(),
            ));
            app.set_status_notice("Autojudge: ON");
            true
        }
        "off" => {
            app.set_autojudge_feature_enabled(false);
            let _ = app.session.save();
            app.push_display_message(DisplayMessage::system(
                "Autojudge disabled for this session.".to_string(),
            ));
            app.set_status_notice("Autojudge: OFF");
            true
        }
        "now" => {
            if let Err(error) = launch_autojudge_window_local(app) {
                app.push_display_message(DisplayMessage::error(format!(
                    "Failed to launch autojudge: {}",
                    error
                )));
                app.set_status_notice("Autojudge launch failed");
            }
            true
        }
        _ => {
            app.push_display_message(DisplayMessage::error(
                "Usage: /autojudge [on|off|status|now]".to_string(),
            ));
            true
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ManualSubagentSpec {
    pub(super) subagent_type: String,
    pub(super) model: Option<String>,
    pub(super) session_id: Option<String>,
    pub(super) prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ImproveCommand {
    Run {
        plan_only: bool,
        focus: Option<String>,
    },
    Resume,
    Status,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RefactorCommand {
    Run {
        plan_only: bool,
        focus: Option<String>,
    },
    Resume,
    Status,
    Stop,
}

// ============================================================================
// Review-loop harness glue (docs/proposals/review-rounds.md).
//
// The actual state machine lives in `review_loop.rs` (pure, unit-tested). This
// section only translates between that state machine and the App/TUI: spawning
// the per-lens reviewer child sessions, polling them for a `VERDICT`, and
// feeding the result back. The auto loop runs for the normal TUI (including
// remote server-clients, matching the manual `/review-loop`); replay sessions
// are excluded.
// ============================================================================

/// Per-lens reviewer startup message. Independent per-lens reviewers each get a
/// clean prompt focused on a single lens, with the report contract so the
/// harness can parse the verdict deterministically.
fn build_lens_review_startup_message(parent_session_id: &str, lens_name: &str, lens_label: &str, lens_focus: &str) -> String {
    format!(
        "You are the `{lens_name}` reviewer for parent session `{parent_session_id}`.\n\
You are one of several independent reviewers. Your job is ONLY to inspect the recent work through the `{lens_label}` lens.\n\
\n\
First read only the conversation history you actually need:\n\
1. Use `conversation_search` with `stats=true` to learn the history size.\n\
2. Read the most recent turns with `conversation_search turns` (start with roughly the last 6-12 turns, then widen only if needed).\n\
3. If requirements are unclear, use `conversation_search query` to find the latest relevant user request or acceptance criteria.\n\
\n\
{guard}\
Inspect the actual repo changes with targeted commands such as `git diff --stat`, `git diff --name-only`, and focused file reads.\n\
\n\
LENS FOCUS — only flag issues in this area:\n{lens_focus}\n\
\n\
Only flag issues in the changed code (the recent batch). Prefer concrete findings over style comments.\n\
When done, respond with the machine-readable report contract and nothing else:\n\
\n\
VERDICT: CLEAN\n\
  (if nothing in your lens scope is wrong)\n\
or\n\
VERDICT: FINDINGS\n\
FINDING: <severity>|<file>|<issue text>\n\
FINDING: <severity>|<file>|<issue text>\n\
  (one FINDING line per issue; severity is HIGH/MEDIUM/LOW/INFO)\n\
\n\
Then stop. Do not ask the user anything. Keep your session concise.",
        lens_name = lens_name,
        lens_label = lens_label,
        lens_focus = lens_focus,
        guard = review_session_read_only_guardrails(),
    )
}

/// True when an auto review loop is active on this session (unfinished).
pub(super) fn is_review_loop_active(app: &App) -> bool {
    app.session
        .review_loop
        .as_ref()
        .map(|s| !s.finished)
        .unwrap_or(false)
}

/// True when the review loop has a fix turn queued-but-not-yet-dispatched.
///
/// A review fix turn on the remote client is sent through `queued_messages`
/// (there is no `pending_turn` handler remotely). `pending_queued_dispatch` is
/// the first signal one is queued, but after a failed send the message is
/// restored to `queued_messages` without re-arming the flag. While the fix is
/// unborn, `active_reviewer_id` is None and `awaiting_postfix_recheck` is true,
/// so polling the loop would spawn the post-fix re-check reviewer against the
/// PRE-fix tree. This is the narrow guard the idle self-drive uses: it blocks
/// only on the review's own unborn fix — not on an unrelated `interleave_message`
/// or system reminder, which should not stall lens progress.
pub(super) fn review_fix_pending(app: &App) -> bool {
    app.session
        .review_loop
        .as_ref()
        .is_some_and(|s| {
            s.awaiting_postfix_recheck && (app.pending_queued_dispatch || !app.queued_messages.is_empty())
        })
}

/// Minimum interval between idle-self-drive polls of an in-flight reviewer.
///
/// `step_review_loop` calls `Session::load` (which replays the session journal)
/// on every idle tick when a reviewer is pending. The idle tick fires many
/// times per second, so that would put continuous disk reads behind what is
/// often a long-running reviewer. We poll aggressively on real turn-end events
/// (not throttled), but the *idle* self-drive — whose only job is to notice
/// that an async reviewer eventually finished — is debounced to this interval.
/// A reviewer taking longer than the interval is polled at most once per
/// interval instead of on every tick, which cuts the load rate by ~an order of
/// magnitude while keeping latency to noticed-verdict bounded by the interval.
const REVIEW_LOOP_IDLE_POLL_DEBOUNCE: std::time::Duration =
    std::time::Duration::from_millis(250);

/// How many times a lens's reviewer may be respawned after being lost before
/// the loop hard-finalizes with `reviewer_unavailable`. A single transient loss
/// (terminal killed, OOM'd, window closed) is retried so one bad reviewer does
/// not silently abort the rest of the 6-lens loop, but an unbounded retry could
/// loop forever on a persistently-broken environment, so the budget is capped.
const REVIEW_LOOP_MAX_REVIEWER_RESPAWNS: u32 = 2;

/// Poll the review loop from an idle tick if the debounce window has elapsed.
///
/// Returns `true` only when a poll actually ran. Callers do not fold this into
/// `needs_redraw`; loop progress pushes its own display/status updates.
pub(super) fn maybe_poll_review_loop_from_idle(app: &mut App) -> bool {
    // While a queued follow-up is about to be dispatched (pending_queued_dispatch),
    // do not also step the review loop: the run loop will dispatch that message
    // (setting is_processing) and the loop would otherwise double-schedule a
    // reviewer against it. This covers both a review fix (via review_fix_pending)
    // and a poke/gate continuation queued in a loop gap (which #2's interleave
    // can now produce while awaiting_postfix_recheck is false).
    if app.pending_queued_dispatch {
        return false;
    }
    // Round-E narrow guard: never self-drive while the review's own fix turn is
    // queued-but-undispatched (would spawn the re-check reviewer against the
    // pre-fix tree). This deliberately does NOT use `has_queued_followups()`:
    // an unrelated interleave message or hidden reminder must not stall lens
    // progress.
    if review_fix_pending(app) {
        return false;
    }
    let now = Instant::now();
    let due = match app.last_review_loop_idle_poll {
        Some(prev) if now.duration_since(prev) < REVIEW_LOOP_IDLE_POLL_DEBOUNCE => false,
        _ => {
            app.last_review_loop_idle_poll = Some(now);
            true
        }
    };
    if !due {
        return false;
    }
    step_review_loop(app)
}

/// Enter the review loop after the completion gates pass. Seeded on the session
/// so it survives reloads; the actual reviewing is driven by turn-end followups.
pub(super) fn maybe_enter_review_loop(app: &mut App) {
    // The review loop auto-seeds for the normal TUI (server-client with
    // `is_remote`), matching the manual `/review-loop` command, which already
    // works there using the same local clone/spawn mechanism. Replay sessions
    // are excluded (deterministic playback, never reviews live work).
    if app.is_replay || !app.autoreview_enabled {
        return;
    }
    // Skip auto-seeding under the unit-test harness: the loop drives
    // independent reviewer child sessions, which would add non-deterministic
    // state to tests that complete todos. The loop still runs in the real
    // product (normal TUI / remote client). Review-loop logic itself is still
    // unit-tested directly against the engine and command surface.
    if app.runtime_mode == super::AppRuntimeMode::TestHarness {
        return;
    }
    if !crate::config::config().autoreview.loop_mode {
        return;
    }
    // The completion gates (ownership / confidence) may still be running a
    // follow-up continuation this turn. Per the proposal the loop enters only
    // once the gates have passed; don't seed it on the same turn the gate is
    // still nudging the model for more work.
    if app.pending_queued_dispatch {
        return;
    }
    // Mutual exclusion: do not auto-enter a review loop while an improve/refactor
    // loop is active. (Going the other way, starting improve clears the review
    // loop via clear_review_loop_on_improve().)
    if app.improve_mode.is_some() {
        return;
    }
    // Auto-entry seeds the loop only once per session: only when no review-loop
    // state exists yet. A finished loop must NOT be re-seeded here (that would
    // restart the whole 6-lens loop after every completed turn). Restart of a
    // finished loop is a deliberate, manual action via `/review-loop start`,
    // which calls enter_review_loop() directly and resets the finished flag.
    if app.session.review_loop.is_some() {
        return;
    }
    let state = app
        .session
        .review_loop
        .get_or_insert_with(crate::session::ReviewLoopState::new);
    review_loop::enter_review_loop(state);
    state.active_reviewer_id = None;
    // A fresh loop must not inherit the idle-poll debounce clock from a previous
    // (just-finished) loop; otherwise the first tick would treat it as a recent
    // poll and delay the first reviewer spawn by up to the debounce interval.
    app.last_review_loop_idle_poll = None;
    let _ = app.session.save();
    app.push_display_message(DisplayMessage::system(
        "🔁 Review loop started: reviewing the finished work across 6 lenses.".to_string(),
    ));
    app.set_status_notice("Review loop: started");
}

/// Spawn the independent per-lens reviewer for the loop's current lens.
fn spawn_loop_reviewer(app: &mut App, lens: jcode_session_types::ReviewLens) -> anyhow::Result<String> {
    let parent_session_id = current_feedback_target_session_id(app);
    let lens_prompt = build_lens_review_startup_message(
        &parent_session_id,
        lens.name(),
        lens.label(),
        lens.focus(),
    );
    let model_override = current_autoreview_model_override();
    let initial_model = model_override
        .clone()
        .unwrap_or_else(|| current_autoreview_model_summary(app));

    // Each lens gets its own fresh reviewer session + client process. This is
    // the per-lens independence the proposal requires (see
    // `docs/proposals/review-rounds.md`): a lens review runs on a clean slate,
    // untainted by an earlier lens's prompt or verdict.
    //
    // Reusing a single `reviewer_session_id` across lenses does NOT work: the
    // startup prompt is delivered via the one-shot `client-input-<id>` handoff
    // file, which a headed client process consumes only once at launch
    // (`--fresh-spawn --resume <id>`, see `tui_lifecycle_runtime.rs`). On the
    // second lens we do not launch a new process, so the already-running
    // reviewer never receives the new lens prompt and `poll_loop_reviewer`
    // would wait forever (or mis-read a stale verdict still in the reused
    // session's history). We deliberately spawn fresh per lens.
    let (session_id, _name) =
        clone_session_for_review(app, "review-loop", initial_model, None)?;

    prepare_review_spawned_session(
        &session_id,
        lens_prompt,
        model_override,
        None,
        Some("review-loop".to_string()),
        None,
    );

    let exe = super::launch_client_executable();
    let cwd = active_working_dir(app)
        .filter(|path| path.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let socket = std::env::var("JCODE_SOCKET").ok();
    super::spawn_in_new_terminal(&exe, &session_id, &cwd, socket.as_deref())?;
    Ok(session_id)
}

/// Poll the in-flight reviewer child session for a `VERDICT`.
enum PollResult {
    /// The reviewer session is gone (deleted/unloadable); the loop cannot make
    /// progress and must finalize rather than poll forever.
    Gone,
    /// The reviewer is still running and has not emitted a verdict yet.
    Pending,
    /// A verdict was parsed.
    Report(jcode_session_types::ReviewReport),
}

fn poll_loop_reviewer(reviewer_id: &str) -> PollResult {
    let session = match crate::session::Session::load(reviewer_id) {
        Ok(s) => s,
        // The reviewer session vanished (deleted or unloadable). Treat as a
        // terminal condition: do NOT keep polling, or the loop spins forever
        // waiting on a child that no longer exists.
        Err(_) => return PollResult::Gone,
    };
    // Scan messages most-recent-first for the first parseable verdict. The
    // reviewer may emit a trailing message (e.g. a tool result) after its
    // VERDICT text, so only checking `messages.last()` would miss it and poll
    // forever. Stop at the first block that parses so a single verdict is not
    // double-counted.
    for message in session.messages.iter().rev() {
        let mut text = String::new();
        for block in &message.content {
            if let crate::message::ContentBlock::Text { text: t, .. } = block {
                text.push_str(t);
            }
        }
        if let Ok(report) = jcode_session_types::ReviewReport::parse(&text) {
            return PollResult::Report(report);
        }
    }
    PollResult::Pending
}

/// Step the review loop from the turn-end followups hook. Returns true when a
/// follow-up was scheduled (so the caller can consider the turn extended).
pub(super) fn step_review_loop(app: &mut App) -> bool {
    // Take the state out so we can mutate `app` freely while driving the loop.
    let mut state = match app.session.review_loop.take() {
        Some(s) if !s.finished => s,
        _ => return false,
    };

    let max_stalled = crate::config::config().autoreview.max_stalled_turns;

    // The in-flight reviewer id is persisted on the state (not just in-memory)
    // so a reloaded session resumes polling the same child session instead of
    // spawning a duplicate.
    let result = if state.active_reviewer_id.is_some() {
        let reviewer_id = state.active_reviewer_id.clone().unwrap();
        match poll_loop_reviewer(&reviewer_id) {
            PollResult::Gone => {
                // The reviewer child session disappeared (deleted/unloadable).
                // Retry a bounded number of times before giving up: a single
                // transient loss (terminal killed, OOM, window closed) should
                // not silently abort the other 5 lenses. Clear the stale id and
                // respawn the same lens; once the budget is exhausted, finalize
                // the loop with a terminal reason so it cannot keep spinning.
                state.active_reviewer_id = None;
                if state.reviewer_respawn_count < REVIEW_LOOP_MAX_REVIEWER_RESPAWNS {
                    state.reviewer_respawn_count += 1;
                    let lens = state.current_lens.unwrap_or(jcode_session_types::ReviewLens::Correctness);
                    app.push_display_message(DisplayMessage::system(format!(
                        "↻ Review loop: the '{}' reviewer session was lost; respawning (attempt {} of {}).",
                        lens.label(),
                        state.reviewer_respawn_count,
                        REVIEW_LOOP_MAX_REVIEWER_RESPAWNS,
                    )));
                    let respawned = spawn_review_loop_reviewer(app, &mut state, lens);
                    // Only claim the lost reviewer was respawned if the spawn
                    // actually succeeded. On failure `spawn_review_loop_reviewer`
                    // sets its own "spawn failed" status + finalized the loop;
                    // overwriting it here would misreport a failed respawn as
                    // an in-progress one.
                    if respawned {
                        app.set_status_notice("Review loop: respawning lost reviewer");
                    }
                    respawned
                } else {
                    state.finished = true;
                    state.finish_reason = Some("reviewer_unavailable".to_string());
                    let digest = review_loop::build_and_store_digest(&mut state);
                    app.push_display_message(DisplayMessage::system(format!(
                        "{digest}\n\n(Review loop stopped: the reviewer session kept being lost after {} respawns.)",
                        REVIEW_LOOP_MAX_REVIEWER_RESPAWNS,
                    )));
                    app.session.review_loop = Some(state);
                    let _ = app.session.save();
                    app.set_status_notice("Review loop: reviewer gone");
                    false
                }
            }
            PollResult::Pending => {
                // Reviewer still working: wait, don't stall.
                app.session.review_loop = Some(state);
                true
            }
            PollResult::Report(report) => {
                state.active_reviewer_id = None;
                // A verdict was consumed: the loss-budget for this lens's
                // reviewer is spent, so the next reviewer (a later lens, or the
                // re-check after a fix) starts with a full respawn budget.
                state.reviewer_respawn_count = 0;
                // Determine whether the fix turn actually changed files: compare
                // the baseline captured at fix-queue time against the current
                // working tree. A file-touching fix is productive repair work and
                // must not count toward the stall cap.
                if let Some(cwd) = active_working_dir(app) {
                    // Capture the current signature once and reuse it for both
                    // the touched-flag comparison and the file list, avoiding a
                    // second `git status` subprocess.
                    let now_sig = working_tree_signature(&cwd);
                    let touched = match (&state.fix_baseline_tree, &now_sig) {
                        (Some(baseline), Some(now)) => now != baseline,
                        _ => false,
                    };
                    state.last_fix_touched_files = touched;
                    // Record which files the fix turn actually touched so the
                    // digest can report real "Files touched" counts. This was
                    // previously dead code (record_fix_files was never called).
                    if touched {
                        if let Some(now_sig) = now_sig {
                            let files = changed_files_from_signature(&now_sig);
                            review_loop::record_fix_files(&mut state, files);
                        }
                    }
                }
                let action = review_loop::apply_verdict(&mut state, &report, max_stalled);
                match action {
                    review_loop::ReviewLoopAction::QueueFixTurn(findings) => {
                        app.session.review_loop = Some(state);
                        let summary = findings
                            .iter()
                            .map(|f| format!("[{}] {}: {}", f.severity, f.path, f.text))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let prompt = format!(
                            "The reviewer found the following issues. Fix them:\n\n{summary}"
                        );
                        // Capture the working-tree signature so the next re-check
                        // can tell whether the fix actually changed files (a
                        // productive, file-touching fix must not count toward the
                        // stall cap even if the open set did not shrink).
                        if let Some(cwd) = active_working_dir(app) {
                            if let Some(sig) = working_tree_signature(&cwd) {
                                app.session.review_loop.as_mut().unwrap().fix_baseline_tree =
                                    Some(sig);
                            }
                        }
                        if app.is_remote {
                            // R-G1: the remote client has NO `pending_turn`
                            // handler -- `start_synthetic_user_turn` sets
                            // `pending_turn`, which only the local `run()`
                            // loop consumes. On the product TUI (remote
                            // server-client) the synthetic fix turn would never
                            // be sent, so the loop would stall after the first
                            // findings. Enqueue the fix prompt instead:
                            // `process_remote_followups` drains `queued_messages`
                            // on the remote run loop and dispatches it via
                            // `begin_remote_send` (which sets is_processing,
                            // streams, emits Done -> the loop re-polls the
                            // re-check reviewer). The server owns the transcript
                            // on the remote path (echo), so we do NOT add the
                            // message locally here -- that would double-record it.
                            app.queued_messages.push(prompt);
                            // Round-E guard: until the fix turn is actually
                            // dispatched (begin_remote_send sets is_processing),
                            // `is_processing` is still false and `active_reviewer_id`
                            // is None, so an idle tick would otherwise spawn the
                            // post-fix re-check reviewer prematurely (reviewing the
                            // pre-fix tree). Marking pending_queued_dispatch both
                            // guards the tick-poll off and forces the remote run
                            // loop to clear-and-dispatch the queued fix on its next
                            // iteration.
                            app.pending_queued_dispatch = true;
                        } else {
                            super::commands_improve::start_synthetic_user_turn(app, prompt);
                        }
                        true
                    }
                    review_loop::ReviewLoopAction::Converged => {
                        let recheck = review_touched_files(&state);
                        finish_review_loop(app, &mut state, recheck);
                        false
                    }
                    review_loop::ReviewLoopAction::Stalled => {
                        finish_review_loop(app, &mut state, false);
                        false
                    }
                    review_loop::ReviewLoopAction::SpawnReviewer(lens) => {
                        spawn_review_loop_reviewer(app, &mut state, lens)
                    }
                    review_loop::ReviewLoopAction::None => {
                        app.session.review_loop = Some(state);
                        false
                    }
                }
            }
        }
    } else {
        let action = review_loop::next_action(&mut state);
        match action {
            review_loop::ReviewLoopAction::SpawnReviewer(lens) => {
                spawn_review_loop_reviewer(app, &mut state, lens)
            }
            review_loop::ReviewLoopAction::Converged => {
                let recheck = review_touched_files(&state);
                finish_review_loop(app, &mut state, recheck);
                false
            }
            review_loop::ReviewLoopAction::Stalled => {
                finish_review_loop(app, &mut state, false);
                false
            }
            _ => {
                app.session.review_loop = Some(state);
                false
            }
        }
    };
    result
}

/// Whether the review loop changed files during any fix round. Only a
/// file-touching convergence re-runs the completion gates; a clean/no-op review
/// leaves the original gate pass valid. (Stall is handled separately: a stalled
/// loop has not converged and never re-runs the gates.)
fn review_touched_files(state: &jcode_session_types::ReviewLoopState) -> bool {
    state
        .record
        .as_ref()
        .is_some_and(|r| !r.files_touched.is_empty())
}

/// Emit the end-of-loop digest and, when the review fixed files (so the work
/// changed after the completion gates first passed), re-run the completion
/// gates once against the post-fix state (N2). A failing gate in that one
/// re-run surfaces and stops; it never re-enters the review loop, so there is
/// no gates↔review ping-pong. The loop is left finished either way.
///
/// `recheck_gates` is true only on a *file-touching convergence* (`Converged`
/// with `review_touched_files`); a stall never re-runs the gates.
pub(super) fn finish_review_loop(
    app: &mut App,
    state: &mut jcode_session_types::ReviewLoopState,
    recheck_gates: bool,
) {
    // The N2 gate re-check: if the review touched files, evaluate the completion
    // gates once against the post-fix state and fold the result into the loop's
    // finish reason *before* the digest is built, so the digest reports what
    // actually happened. It runs exactly once here; the loop is finished
    // afterward, so it can never re-trigger.
    if recheck_gates {
        let session_id = active_session_id(app);
        let todos = crate::todo::load_todos(&session_id).unwrap_or_default();
        let goals = crate::todo::load_goals(&session_id).unwrap_or_default();
        let ownership_ok = crate::todo::completed_groups_have_sufficient_delivery(&todos, &goals);
        let confidence = todo_confidence_summary(&todos);
        // Match the completion-gate signal the auto-poke path uses. `needs_more_work`
        // is the consolidated completion-confidence + spike check; a confidence spike
        // also demands one re-validation and must not silently pass.
        let confidence_ok = !confidence.needs_more_work;

        if !(ownership_ok && confidence_ok) {
            // The completion assessment now disagrees with the reviewed + fixed
            // result. Surface it and stop; do not run another review round (no
            // gates↔review ping-pong). Record the specific gate that failed so
            // it shows up in the same telemetry the primary auto-poke gate uses.
            let kind = if !ownership_ok {
                crate::telemetry::TodoGateKind::Ownership
            } else if confidence.confidence_spike_detected {
                crate::telemetry::TodoGateKind::ConfidenceSpike
            } else {
                crate::telemetry::TodoGateKind::Completion
            };
            let reason = if !ownership_ok {
                "end-to-end delivery assessment no longer holds"
            } else {
                "completion confidence needs re-validation"
            };
            crate::telemetry::record_todo_gate(kind);
            state.finish_reason = Some(format!("converged_gate_recheck_failed: {reason}"));
            app.push_display_message(DisplayMessage::system(format!(
                "⚠️ Review fixed files, but the completion assessment now disagrees ({reason}). Please review the result yourself."
            )));
            app.set_status_notice("Review loop: done (gate re-check failed)");
        } else {
            crate::logging::info(&format!(
                "REVIEW_LOOP_GATE_RECHECK action=passed files_touched={}",
                state
                    .record
                    .as_ref()
                    .map(|r| r.files_touched.len())
                    .unwrap_or(0)
            ));
            state.finish_reason = Some("converged".to_string());
            app.set_status_notice("Review loop: done");
        }
    } else {
        // No re-check needed (converged without touching files, or stalled): the
        // review made no post-gate change, so the original gate pass still holds.
        app.set_status_notice("Review loop: done");
    }

    let digest = review_loop::build_and_store_digest(state);
    app.push_display_message(DisplayMessage::system(digest));
    app.session.review_loop = Some(state.clone());
    let _ = app.session.save();
}

/// Spawn the per-lens reviewer for the current lens and persist the resulting
/// loop state. Returns `false` and ends the loop cleanly when spawn fails, so
/// a transient spawn failure does not re-trigger an infinite spawn-and-poll
/// cycle every turn-end.
fn spawn_review_loop_reviewer(
    app: &mut App,
    state: &mut jcode_session_types::ReviewLoopState,
    lens: jcode_session_types::ReviewLens,
) -> bool {
    match spawn_loop_reviewer(app, lens) {
        Ok(id) => {
            state.active_reviewer_id = Some(id);
            app.session.review_loop = Some(state.clone());
            let _ = app.session.save();
            // Surface the lens actually being reviewed so the parent status bar
            // tracks loop progress. The spawned reviewer runs in its own window;
            // this is the parent-side signal that the loop advanced (and it makes
            // the next idle redraw show the current lens rather than a stale one).
            app.set_status_notice(format!("Review loop: reviewing {}", lens.label()));
            true
        }
        Err(error) => {
            state.finished = true;
            state.finish_reason = Some("spawn_failed".to_string());
            let record = state.record.get_or_insert_with(jcode_session_types::ReviewRecord::default);
            record.digest = Some(
                format!(
                    "## Review stopped\n\nCould not spawn the reviewer for the '{}' lens: {}",
                    lens.label(),
                    error
                ),
            );
            app.session.review_loop = Some(state.clone());
            let _ = app.session.save();
            app.push_display_message(DisplayMessage::error(format!(
                "Review loop stopped: failed to spawn reviewer for '{}': {}",
                lens.label(),
                error
            )));
            app.set_status_notice("Review loop: spawn failed");
            false
        }
    }
}

/// Manual `/review-loop` command (mirrors `/improve`): start a full per-lens
/// loop for the current session. Enforces mutual exclusion with improve/refactor.
pub(super) fn handle_review_loop_command_local(app: &mut App, trimmed: &str) -> bool {
    if !trimmed.starts_with("/review-loop") {
        return false;
    }
    let rest = trimmed.strip_prefix("/review-loop").unwrap_or_default().trim();

    match rest {
        "" | "start" | "run" => {
            // Mutual exclusion: starting a review loop clears improve/refactor.
            app.improve_mode = None;
            app.session.improve_mode = None;
            // Also cancel any improve/refactor continuation that was queued
            // (e.g. interrupt_and_queue_synthetic_message during a busy state):
            // the review loop now owns the turn, and a leftover improve "fix
            // this" prompt must not be dispatched mid-review. Mirror the
            // clear_review_loop_on_improve / /review-loop stop semantics.
            app.queued_messages.clear();
            app.hidden_queued_system_messages.clear();
            app.pending_queued_dispatch = false;
            let state = app
                .session
                .review_loop
                .get_or_insert_with(crate::session::ReviewLoopState::new);
            review_loop::enter_review_loop(state);
            // Match the auto-entry path (maybe_enter_review_loop): a manual
            // start must not keep polling a stale in-flight reviewer from a
            // previous run/lens, nor inherit the prior loop's idle-poll debounce.
            state.active_reviewer_id = None;
            app.last_review_loop_idle_poll = None;
            let _ = app.session.save();
            app.push_display_message(DisplayMessage::system(
                "🔁 Review loop started (manual). Reviewing across 6 lenses.".to_string(),
            ));
            app.set_status_notice("Review loop: started");
            true
        }
        "stop" => {
            if let Some(state) = app.session.review_loop.as_mut() {
                state.finish_with("user_stopped");
                // Cancel any review fix turn that is queued-but-not-yet-dispatched
                // (remote path stages the fix into queued_messages). After stop
                // the loop is finished, but the queued "fix them" prompt would
                // still be dispatched by the run loop as if it were a user
                // message, which is exactly what "stop the review" should prevent.
                app.queued_messages.clear();
                app.hidden_queued_system_messages.clear();
                app.pending_queued_dispatch = false;
                let _ = app.session.save();
                app.push_display_message(DisplayMessage::system(
                    "Review loop stopped.".to_string(),
                ));
                app.set_status_notice("Review loop: stopped");
            } else {
                app.push_display_message(DisplayMessage::system(
                    "No active review loop to stop.".to_string(),
                ));
            }
            true
        }
        "status" => {
            let status = match &app.session.review_loop {
                None => "No review loop for this session.".to_string(),
                Some(state) if state.finished => {
                    // The digest was persisted when the loop finished, so a
                    // reloaded session can still show the outcome. Fall back to
                    // a one-line summary if it is somehow absent.
                    match state.record.as_ref().and_then(|r| r.digest.as_deref()) {
                        Some(digest) => digest.to_string(),
                        None => format!(
                            "Review loop finished ({}).",
                            state.finish_reason.as_deref().unwrap_or("unknown")
                        ),
                    }
                }
                Some(state) => {
                    let lens = state
                        .current_lens
                        .map(|l| l.label().to_string())
                        .unwrap_or_else(|| "unset".to_string());
                    format!(
                        "Review loop active at lens: {lens} (phase: {:?}, stall turns: {}).",
                        state.phase, state.stall_turns
                    )
                }
            };
            app.push_display_message(DisplayMessage::system(status));
            true
        }
        _ => {
            app.push_display_message(DisplayMessage::error(
                "Usage: /review-loop [start|stop|status]".to_string(),
            ));
            true
        }
    }
}

/// When improve/refactor starts, clear any active review loop (mutual
/// exclusion: only one loop-mode per session at a time).
pub(super) fn clear_review_loop_on_improve(app: &mut App) {
    if app.session.review_loop.as_ref().map(|s| !s.finished).unwrap_or(false) {
        app.session.review_loop = None;
        // Cancel any review fix turn that is queued-but-undispatched, mirroring
        // `/review-loop stop`: the loop is being replaced by improve/refactor,
        // so a stranded "fix them" prompt must not be dispatched later.
        app.queued_messages.clear();
        app.hidden_queued_system_messages.clear();
        app.pending_queued_dispatch = false;
        let _ = app.session.save();
    }
}

#[cfg(test)]
#[path = "tests/issue_605_clear_side_panel.rs"]
mod issue_605_clear_side_panel_tests;
