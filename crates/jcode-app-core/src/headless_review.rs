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
//! The client review loop can then dispatch "run this lens headless" to the
//! server and poll the reviewer session for the verdict, exactly as it already
//! polls a spawned-window reviewer — but with no window and no terminal
//! dependence. This module is the foundation of that integration.
//!
//! The execution pattern mirrors
//! `ambient::runner::spawn_session_for_scheduled_item` (fork provider -> build
//! registry -> `Agent::new_with_session` -> `run_once_capture_with_display_role`),
//! which already runs headless agent turns in the server for scheduled ambient
//! sessions.

use crate::agent::Agent;
use crate::logging;
use crate::message::{ContentBlock, Role};
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
/// per-lens independence the proposal requires. `lens` is used for logging; the
/// actual review instructions live in `lens_prompt`, which the caller should
/// build with `build_lens_review_startup_message` (TUI) or its server
/// equivalent so the report contract (`VERDICT: CLEAN | FINDINGS`) is emitted.
///
/// Returns a `HeadlessReviewOutcome` — never a hard error for a missing or
/// malformed verdict: the caller's loop decides how to surface a non-verdict.
pub async fn run_review_lens_headless(
    provider: Arc<dyn Provider>,
    parent_session: &Session,
    lens_label: &str,
    lens_prompt: String,
    working_dir: Option<String>,
) -> HeadlessReviewOutcome {
    let child = match build_reviewer_session(parent_session, &lens_prompt, working_dir) {
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

/// Build a fresh reviewer session cloning the parent's context, with the lens
/// prompt injected as the one-shot first user turn.
fn build_reviewer_session(
    parent_session: &Session,
    lens_prompt: &str,
    working_dir: Option<String>,
) -> anyhow::Result<Session> {
    let mut child = Session::create(Some(parent_session.id.clone()), Some("revel".to_string()));
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
    // Inject the lens prompt as the reviewer's first user turn; the agent will
    // process it on its own (one-shot) rather than wait for a client submit.
    child.add_message_with_display_role(
        Role::User,
        vec![crate::message::ContentBlock::Text {
            text: lens_prompt.to_string(),
            cache_control: None,
        }],
        Some(crate::session::StoredDisplayRole::System),
    );
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
        // A stale CLEAN followed by a new FINDINGS must surface find more recent.
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
}
