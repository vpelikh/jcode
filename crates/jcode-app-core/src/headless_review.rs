//! Headless in-process review: run a review-lens reviewer as a background
//! `Agent` turn on the server, with no terminal window.
//!
//! Today the TUI's auto review loop (`jcode-tui`) spawns a **headed client in a
//! new terminal window** per lens and polls that child session for a `VERDICT`.
//! That is environment-dependent and fragile (needs a terminal emulator, a
//! headed client that consumes the one-shot prompt handoff and auto-runs the
//! turn). The proposal (`docs/proposals/review-rounds.md`) locks auto-mode to
//! headless in-process execution with no windows.
//!
//! The TUI client is a thin client on an `InertRuntimeProvider` with an empty
//! tool `Registry`, so it **cannot** run an `Agent` in-process. The server is
//! where the real provider, tool registry, and `Agent` live. This module is the
//! server-side headless reviewer: it clones the parent session into a fresh
//! reviewer child, binds an `Agent` to it via `Agent::new_with_session`, runs
//! the lens prompt headlessly, closes the agent, and re-loads the session to
//! parse the machine-readable `VERDICT`.
//!
//! The client review loop dispatches "run this lens headless" to the server,
//! which runs the reviewer here and replies with a `ServerEvent::HeadlessReviewResult`
//! that the client applies to advance the loop — no window and no terminal
//! dependence, and no client-side session polling.
//!
//! The execution pattern mirrors
//! `ambient::runner::spawn_session_for_scheduled_item` (fork provider -> build
//! registry -> `Agent::new_with_session` -> `run_once_capture_with_display_role`),
//! which already runs headless agent turns in the server for scheduled ambient
//! sessions.

use crate::agent::Agent;
use crate::logging;
use crate::message::ContentBlock;
use crate::provider::Provider;
use crate::session::Session;
use crate::tool;
use std::sync::Arc;

use jcode_session_types::ReviewReport;

/// Outcome of a headless review-lens run.
#[derive(Debug, Clone)]
pub enum HeadlessReviewOutcome {
    /// The reviewer produced a parseable verdict (CLEAN or FINDINGS).
    Report(ReviewReport),
    /// The reviewer session could not be set up or the turn failed.
    Failed(String),
    /// The lens turn ran but produced no parseable `VERDICT`.
    NoVerdict(String),
}

/// Run a single review lens headlessly against the parent session's working
/// tree.
///
/// The reviewer is a clone of the parent session (context + working dir) plus
/// the lens prompt injected as its one-shot first user turn, preserving the
/// per-lens independence the proposal requires. `lens_label` is used for
/// logging; the actual review instructions live in `lens_prompt`, which the
/// caller builds (server-side or on the client) so the report contract
/// (`VERDICT: CLEAN | FINDINGS`) is emitted.
///
/// Returns a `HeadlessReviewOutcome` — never a hard error for a missing or
/// malformed verdict: the caller's loop decides how to surface a non-verdict.
pub async fn run_review_lens_headless(
    provider: Arc<dyn Provider>,
    parent_session: &Session,
    lens_label: &str,
    _lens_prompt: String,
    working_dir: Option<String>,
) -> HeadlessReviewOutcome {
    // The lens prompt is built server-side from the lens label (not trusted from
    // the client), so the report contract and read-only guardrails are always
    // applied consistently regardless of which client dispatched the review.
    let lens_prompt = build_lens_prompt(parent_session.id.as_str(), lens_label);

    let child = match build_reviewer_session(parent_session, working_dir) {
        Ok(c) => c,
        Err(e) => return HeadlessReviewOutcome::Failed(format!("set up reviewer session: {e}")),
    };
    let reviewer_id = child.id.clone();

    logging::info(&format!(
        "Headless review: starting '{}' lens on session {} (reviewer {})",
        lens_label,
        parent_session.id,
        reviewer_id
    ));

    let cycle_provider = provider.fork();
    let registry = tool::Registry::new(cycle_provider.clone()).await;
    let mut agent = Agent::new_with_session(cycle_provider, registry, child, None);

    let run = agent
        .run_once_capture_with_display_role(
            &lens_prompt,
            Some(crate::session::StoredDisplayRole::System),
        )
        .await;
    agent.mark_closed();

    let session = match Session::load(&reviewer_id) {
        Ok(s) => s,
        Err(e) => {
            logging::warn(&format!(
                "Headless review '{}': could not reload reviewer session {}: {}",
                lens_label,
                reviewer_id,
                e
            ));
            return match run {
                Ok(_) => HeadlessReviewOutcome::NoVerdict(
                    "reviewer session unloadable after run".to_string(),
                ),
                Err(e) => HeadlessReviewOutcome::Failed(format!("review turn failed: {e}")),
            };
        }
    };

    // The reviewer writes `VERDICT` as part of its messages; scan most-recent-
    // first for the first parseable verdict, so a trailing tool result does not
    // hide a verdict already written.
    if let Some(report) = parse_verdict_from_session(&session) {
        logging::info(&format!(
            "Headless review '{}': verdict parsed from session {}",
            lens_label,
            reviewer_id
        ));
        return HeadlessReviewOutcome::Report(report);
    }

    match run {
        Err(e) => HeadlessReviewOutcome::Failed(format!("review turn failed: {e}")),
        Ok(_) => HeadlessReviewOutcome::NoVerdict(
            "reviewer turn finished but no VERDICT was parsed".to_string(),
        ),
    }
}

/// Build the server-side lens review prompt from the lens label. The report
/// contract and read-only guardrails are always applied here so the reviewer
/// emits a parseable `VERDICT` and stays analysis-only, independent of which
/// client dispatched the review.
fn build_lens_prompt(parent_session_id: &str, lens_label: &str) -> String {
    let (lens_name, focus) = match jcode_session_types::ReviewLens::from_name(lens_label) {
        Some(lens) => (lens.name(), lens.focus()),
        None => (lens_label, ""),
    };
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
LENS FOCUS — only flag issues in this area:\n{focus}\n\
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
        parent_session_id = parent_session_id,
        lens_name = lens_name,
        lens_label = lens_label,
        focus = focus,
        guard = READ_ONLY_GUARDRAILS,
    )
}

/// Read-only guardrails applied to every headless reviewer prompt so a review
/// lens never modifies files or continues implementation.
const READ_ONLY_GUARDRAILS: &str = "Important constraints for this session:\n\
- This session is analysis-only. Do not do the work yourself.\n\
- Do not modify files or repo state. Do not call `edit`, `write`, `multiedit`, `patch`, `apply_patch`, or destructive `bash`/`git` commands.\n\
- Do not continue implementation, fix issues, or take follow-up actions yourself.\n\
- If additional work is needed, describe it in your DM to the parent session instead.\n\
\n";

/// Scan a session's messages (most-recent-first) for the first parseable
/// review verdict. Returns `None` when no message contains a `VERDICT` line.
fn parse_verdict_from_session(session: &Session) -> Option<ReviewReport> {
    for message in session.messages.iter().rev() {
        let text = stored_message_text(message);
        if let Ok(report) = ReviewReport::parse(&text) {
            return Some(report);
        }
    }
    None
}

/// Concatenate a stored message's text content blocks into one string.
fn stored_message_text(message: &crate::session::StoredMessage) -> String {
    let mut text = String::new();
    for block in &message.content {
        if let ContentBlock::Text { text: t, .. } = block {
            text.push_str(t);
        }
    }
    text
}

/// Build a fresh reviewer session cloning the parent's context, WITHOUT
/// injecting the lens prompt. The prompt is delivered by `run_once_capture_with_display_role`
/// (as `Agent::new_with_session` + `run_once_capture` do for ambient scheduled
/// sessions), which appends it as the reviewer's one-shot user turn. Injecting
/// it here as well would produce two back-to-back identical user turns, so the
/// prompt is intentionally NOT added here.
fn build_reviewer_session(
    parent_session: &Session,
    working_dir: Option<String>,
) -> anyhow::Result<Session> {
    let mut child = Session::create(Some(parent_session.id.clone()), Some("review".to_string()));
    child.replace_messages(parent_session.messages.clone());
    child.compaction = parent_session.compaction.clone();
    child.provider_key = parent_session.provider_key.clone();
    child.route_api_method = parent_session.route_api_method.clone();
    child.model = parent_session.model.clone();
    child.subagent_model = parent_session.subagent_model.clone();
    child.reasoning_effort = parent_session.reasoning_effort.clone();
    child.is_canary = parent_session.is_canary;
    child.testing_build = parent_session.testing_build.clone();
    child.is_debug = parent_session.is_debug;
    child.memory_injections = parent_session.memory_injections.clone();
    child.replay_events = parent_session.replay_events.clone();
    child.working_dir = working_dir.or(parent_session.working_dir.clone());
    child.autoreview_enabled = Some(false);
    child.autojudge_enabled = Some(false);
    child.status = crate::session::SessionStatus::Closed;
    child.rebuild_event_map();
    child.save()?;
    Ok(child)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, Role};
    use crate::session::Session;

    /// Build a saved session with the given text content as a user message.
    fn session_with_verdict(text: &str) -> Session {
        let mut session = Session::create(None, None);
        session.add_message_with_display_role(
            Role::User,
            vec![ContentBlock::Text {
                text: text.to_string(),
                cache_control: None,
            }],
            Some(crate::session::StoredDisplayRole::System),
        );
        session.rebuild_event_map();
        session.save().expect("save session");
        session
    }

    #[test]
    fn parse_verdict_from_session_sees_clean() {
        let session = session_with_verdict("No issues found.\nVERDICT: CLEAN");
        let report = parse_verdict_from_session(&session).expect("parses a verdict");
        assert!(matches!(report, ReviewReport::Clean));
    }

    #[test]
    fn parse_verdict_from_session_sees_findings() {
        let session = session_with_verdict(
            "VERDICT: FINDINGS\nFINDING: HIGH|a.rs|off by one",
        );
        let report = parse_verdict_from_session(&session).expect("parses a verdict");
        match report {
            ReviewReport::Clean => panic!("expected findings"),
            ReviewReport::Findings(fs) => assert_eq!(fs.len(), 1),
        }
    }

    #[test]
    fn parse_verdict_returns_none_when_no_verdict() {
        let session = session_with_verdict("just a chat message, no VERDICT token");
        assert!(parse_verdict_from_session(&session).is_none());
    }

    #[test]
    fn verdict_scan_prefers_most_recent_message() {
        // A stale CLEAN followed by a newer FINDINGS must surface the newer
        // verdict (the scan goes most-recent-first).
        let mut session = session_with_verdict("VERDICT: CLEAN");
        session.add_message_with_display_role(
            Role::User,
            vec![ContentBlock::Text {
                text: "VERDICT: FINDINGS\nFINDING: MEDIUM|b.rs|leak".to_string(),
                cache_control: None,
            }],
            None,
        );
        session.save().expect("save");
        let report = parse_verdict_from_session(&session).expect("parses");
        match report {
            ReviewReport::Findings(fs) => assert_eq!(fs[0].severity, "MEDIUM"),
            ReviewReport::Clean => panic!("expected the newer FINDINGS verdict"),
        }
    }

    #[test]
    fn build_reviewer_session_does_not_inject_lens_prompt() {
        // Regression: the reviewer session must NOT pre-inject the lens prompt.
        // The prompt is delivered exactly once by `run_once_capture_with_display_role`,
        // so pre-injecting here would produce a duplicated user turn.
        let parent = Session::create(None, None);
        let child =
            build_reviewer_session(&parent, None).expect("build reviewer session clones parent");
        // The only message(s) in the reviewer should be a clone of the parent's
        // (which is empty here) — no injected user turn carrying "You are the".
        let texts: Vec<String> = child
            .messages
            .iter()
            .map(|m| {
                let mut t = String::new();
                for block in &m.content {
                    if let ContentBlock::Text { text, .. } = block {
                        t.push_str(text);
                    }
                }
                t
            })
            .collect();
        assert!(
            texts.iter().all(|t| !t.contains("You are the")),
            "reviewer session must not embed the lens prompt, got {texts:?}"
        );
    }
}
