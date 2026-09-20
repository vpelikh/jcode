//! Session-log invariant registry (deepseek-harness takeaway #3 and #4).
//!
//! dsh ships a package-owned `ctx.invariants` registry of runtime assertions;
//! its headline invariant is "anything that reaches a model request must be
//! reconstructable from the session log." We take the same shape here: a small
//! set of *named* checks over the append-only event log. [`InvariantLog::enforce`]
//! is the seam that turns a violation into a hard `debug_assert!` panic in dev
//! and a structured log/metric in release. `enforce` is wired at the safe,
//! deliberately narrow call site in
//! `Session::compact_transcript_with_bracket` (after the bracket closes, where an
//! open/duplicated bracket is provably a bug). Other callers adopt it explicitly;
//! notably it is NOT enforced on the plain load/resume path, because a crashed
//! session legitimately carries an open bracket / unanswered tool call there.
//! The built-in checks additionally run as a *diagnostic* pass on the load path
//! (reporting violations to stderr in debug builds without aborting load) and
//! in tests.
//!
//! The registry also provides a minimal **projection seam** (takeaway #4): rather
//! than re-scanning the raw event stream ad hoc, consumers fold committed
//! events through [`LogProjection`] implementations, and the built-in checks
//! verify the derived state machines (messages, compaction, tool-pairing) hold
//! when events are folded in order — so replay determinism is checked, not
//! assumed.

use crate::session::event_types::{SessionEvent, SessionEventMap, SessionEventOp};
use jcode_message_types::ContentBlock;
use std::collections::HashSet;

/// A named invariant check over the whole event log.
///
/// Each check returns `Ok(())` when the invariant holds, or
/// [`InvariantViolation`] describing exactly which boundary broke. Checks are
/// cheap and pure; they intentionally touch only the log so an invariant can be
/// run on any session without mutating it.
pub trait LogInvariant {
    /// Stable name used in metrics/logs to identify the check.
    fn name(&self) -> &'static str;

    /// Run the check over the log's derived state.
    fn check(&self, map: &SessionEventMap) -> Result<(), InvariantViolation>;
}

/// A single, location-accurate report of a broken invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantViolation {
    /// The invariant that failed (stable name, safe for metrics tags).
    pub invariant: &'static str,
    /// Human-readable description of the failure and where in the log it is.
    pub message: String,
    /// The index of the offending event, when applicable.
    pub event_index: Option<usize>,
}

impl InvariantViolation {
    fn new(invariant: &'static str, message: impl Into<String>) -> Self {
        Self {
            invariant,
            message: message.into(),
            event_index: None,
        }
    }

    fn at(invariant: &'static str, event_index: usize, message: impl Into<String>) -> Self {
        Self {
            invariant,
            message: message.into(),
            event_index: Some(event_index),
        }
    }
}

/// A projection unit folds committed events incrementally into typed derived
/// state (takeaway #4). This is the seam the event-sourced log should feed:
/// many readers subscribe to derived state instead of scanning the raw stream.
pub trait LogProjection {
    /// The fold result type.
    type State: Default;

    /// Apply one event to the running state.
    fn apply(state: &mut Self::State, event: &SessionEvent);

    /// A cheap self-consistency check over the folded state, if any.
    fn validate(_state: &Self::State) -> Result<(), InvariantViolation> {
        Ok(())
    }
}

/// Fold the log through a [`LogProjection`] and validate the folded state. This
/// is the projection-seam entry point: one pass over the log, many derived
/// states.
pub fn fold_projection<P: LogProjection>(
    events: &[SessionEvent],
) -> Result<P::State, InvariantViolation> {
    let mut state = P::State::default();
    for event in events {
        P::apply(&mut state, event);
    }
    P::validate(&state)?;
    Ok(state)
}

/// Convenience: fold a projection over a full [`SessionEventMap`].
pub fn project_map<P: LogProjection>(
    map: &SessionEventMap,
) -> Result<P::State, InvariantViolation> {
    fold_projection::<P>(&map.events)
}

/// A carried trace of [`InvariantViolation`]s produced by a check run.
#[derive(Debug, Default, Clone)]
pub struct InvariantLog {
    /// Violations detected on the last check run (empty = all green).
    pub violations: Vec<InvariantViolation>,
}

impl InvariantLog {
    /// True when no invariant was violated.
    pub fn is_green(&self) -> bool {
        self.violations.is_empty()
    }

    /// Enforce the invariant registry as a hard assertion in dev builds and a
    /// logged signal in release. Mirrors dsh's "hard debug_assert in dev,
    /// log+metric in release" contract.
    ///
    /// In debug builds a violation panics immediately. In release it is recorded
    /// to stderr so callers can decide whether to surface a metric — replay and
    /// telemetry paths should treat a violation as a real signal, not ignore it.
    pub fn enforce(&self, context: &str) {
        if self.is_green() {
            return;
        }
        #[cfg(debug_assertions)]
        {
            panic!(
                "session invariant violated ({context}): {:#?}",
                self.violations
            );
        }
        #[cfg(not(debug_assertions))]
        {
            eprintln!(
                "[session-invariant] {context} violated {} invariant(s):",
                self.violations.len()
            );
            for v in &self.violations {
                eprintln!("  - {}: {}", v.invariant, v.message);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in invariant checks
// ---------------------------------------------------------------------------

/// Event ids must be non-empty within an appended log.
pub struct NonEmptyEventIds;

impl LogInvariant for NonEmptyEventIds {
    fn name(&self) -> &'static str {
        "session.event_ids_non_empty"
    }

    fn check(&self, map: &SessionEventMap) -> Result<(), InvariantViolation> {
        for (i, event) in map.events.iter().enumerate() {
            if event.event_id.is_empty() {
                return Err(InvariantViolation::at(
                    "session.event_ids_non_empty",
                    i,
                    "event carries an empty event_id",
                ));
            }
        }
        Ok(())
    }
}

/// `parent_id` references, when present, must point at an earlier event in the
/// same log (a "merge-extensibility" edge should never dangle forward).
pub struct ParentEdgesResolve;

impl LogInvariant for ParentEdgesResolve {
    fn name(&self) -> &'static str {
        "session.parent_edges_resolve"
    }

    fn check(&self, map: &SessionEventMap) -> Result<(), InvariantViolation> {
        let ids: HashSet<&str> = map.events.iter().map(|e| e.event_id.as_str()).collect();
        for (i, event) in map.events.iter().enumerate() {
            if let Some(parent) = &event.parent_id
                && parent != &event.event_id
                && !ids.contains(parent.as_str())
            {
                return Err(InvariantViolation::at(
                    "session.parent_edges_resolve",
                    i,
                    format!("event references unknown parent id '{parent}'"),
                ));
            }
        }
        Ok(())
    }
}

/// Tool pairing (shared with takeaway #5's edge integrity): as messages are
/// derived, an open `tool_call` that is never answered by a matching
/// `tool_result` breaks the derived surface — the model would see a tool call as
/// if it were *after* its result. This check walks the derived messages in
/// order and flags an unbalanced open tool call.
pub struct ToolPairingBalanced;

impl LogInvariant for ToolPairingBalanced {
    fn name(&self) -> &'static str {
        "session.tool_pairing_balanced"
    }

    fn check(&self, map: &SessionEventMap) -> Result<(), InvariantViolation> {
        let messages = map.derive_messages();
        // Stack of open tool_call ids, in the order the assistant emitted them.
        let mut open: Vec<String> = Vec::new();
        for (i, m) in messages.iter().enumerate() {
            for block in &m.content {
                match block {
                    ContentBlock::ToolUse { id, .. } => {
                        if !open.iter().any(|o| o == id) {
                            open.push(id.clone());
                        }
                    }
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        if let Some(pos) = open.iter().position(|o| o == tool_use_id) {
                            open.remove(pos);
                        } else {
                            return Err(InvariantViolation::at(
                                "session.tool_pairing_balanced",
                                i,
                                format!("tool_result for id '{tool_use_id}' has no matching open tool_call"),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(id) = open.first() {
            return Err(InvariantViolation::new(
                "session.tool_pairing_balanced",
                format!("unanswered tool_call remains open for id '{id}'"),
            ));
        }
        Ok(())
    }
}

/// Replay determinism: deriving messages from the log twice yields an identical
/// transcript. Catches non-pure projection logic (e.g. reading wall-clock or a
/// global cache) that would make replay non-deterministic.
pub struct ReplayDeterminism;

impl LogInvariant for ReplayDeterminism {
    fn name(&self) -> &'static str {
        "session.replay_determinism"
    }

    fn check(&self, map: &SessionEventMap) -> Result<(), InvariantViolation> {
        let first = map.derive_messages();
        let second = map.derive_messages();
        // `StoredMessage` is deliberately not `PartialEq` (heavy fields); compare
        // the canonical serialized form instead, which is exactly the property we
        // care about: two folds must produce byte-identical transcripts.
        let a = serde_json::to_vec(&first).map_err(|e| {
            InvariantViolation::new("session.replay_determinism", format!("first fold failed to serialize: {e}"))
        })?;
        let b = serde_json::to_vec(&second).map_err(|e| {
            InvariantViolation::new("session.replay_determinism", format!("second fold failed to serialize: {e}"))
        })?;
        if a != b {
            return Err(InvariantViolation::new(
                "session.replay_determinism",
                "derive_messages() returned different transcripts on consecutive runs",
            ));
        }
        Ok(())
    }
}

/// Compaction brackets must be well-formed (takeaway #5's orphan-detection
/// consumer of takeaway #3). Every `CompactionStart` must be closed by a later
/// `CompactionEnd`, and a `CompactionEnd` must never appear without a preceding
/// open start. An orphaned bracket is the replay-visible signal of a compaction
/// that crashed mid-summarize.
pub struct CompactionBracket;

impl LogInvariant for CompactionBracket {
    fn name(&self) -> &'static str {
        "session.compaction_bracket_balanced"
    }

    fn check(&self, map: &SessionEventMap) -> Result<(), InvariantViolation> {
        let mut depth = 0usize;
        for (i, event) in map.events.iter().enumerate() {
            match &event.op {
                SessionEventOp::CompactionStart { .. } => depth += 1,
                SessionEventOp::CompactionEnd { .. } => {
                    if depth == 0 {
                        return Err(InvariantViolation::at(
                            "session.compaction_bracket_balanced",
                            i,
                            "CompactionEnd appears without a matching open CompactionStart",
                        ));
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
        if depth != 0 {
            return Err(InvariantViolation::new(
                "session.compaction_bracket_balanced",
                format!("orphaned CompactionStart bracket remains open (depth {depth}); compaction likely crashed mid-summarize"),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// A registry of log invariants that can be run together over a session log.
#[derive(Default)]
pub struct InvariantRegistry {
    checks: Vec<Box<dyn LogInvariant + Send + Sync>>,
}

impl InvariantRegistry {
    /// The default built-in checks. Callers may add their own with [`add`].
    ///
    /// [`add`]: Self::add
    pub fn builtin() -> Self {
        let mut r = Self::default();
        r.add(NonEmptyEventIds);
        r.add(ParentEdgesResolve);
        r.add(ToolPairingBalanced);
        r.add(ReplayDeterminism);
        r.add(CompactionBracket);
        r
    }

    /// Register an additional check.
    pub fn add<I>(&mut self, check: I)
    where
        I: LogInvariant + Send + Sync + 'static,
    {
        self.checks.push(Box::new(check));
    }

    /// Run every registered check over the log, folding results into an
    /// [`InvariantLog`]. Runs all checks (not short-circuiting) so a single pass
    /// reports *all* broken boundaries, matching dsh's "findable" goal.
    pub fn check(&self, map: &SessionEventMap) -> InvariantLog {
        let mut violations = Vec::new();
        for check in &self.checks {
            if let Err(v) = check.check(map) {
                violations.push(v);
            }
        }
        InvariantLog { violations }
    }
}

/// A sample projection: the number of live transcript messages derived from the
/// log. Demonstrates the projection seam consuming append-only events without
/// re-scanning for other domains.
pub struct MessageCountProjection;

impl LogProjection for MessageCountProjection {
    type State = usize;

    fn apply(state: &mut Self::State, event: &SessionEvent) {
        match &event.op {
            SessionEventOp::AppendMessage { .. } => *state += 1,
            SessionEventOp::InsertMessage { .. } => *state += 1,
            SessionEventOp::ReplaceMessages {
                start_index,
                end_index,
                messages,
                ..
            } => {
                // A `ReplaceMessages` is a *splice*: it replaces the span
                // `start_index..end_index` (capped at the live length, mirroring
                // `derive_messages`) with `messages`. For a full replacement
                // (`start=0`, `end=usize::MAX`) the result is simply
                // `messages.len()`; for a partial splice the result is
                // `state - (end - start) + messages.len()`. Using
                // `messages.len()` alone would under-count partial replacements.
                let end = (*end_index).min(*state);
                let start = (*start_index).min(end);
                *state = state.saturating_sub(end - start).saturating_add(messages.len());
            }
            SessionEventOp::ClearAll => *state = 0,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_message_types::Role;
    use jcode_session_types::StoredMessage;

    fn text_msg(id: &str) -> StoredMessage {
        StoredMessage {
            id: id.to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "hello".into(),
                cache_control: None,
            }],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        }
    }

    fn tool_use_msg(id: &str, tool_id: &str) -> StoredMessage {
        StoredMessage {
            id: id.to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: tool_id.to_string(),
                name: "bash".into(),
                input: serde_json::json!({}),
                thought_signature: None,
            }],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        }
    }

    fn tool_result_msg(id: &str, tool_id: &str) -> StoredMessage {
        StoredMessage {
            id: id.to_string(),
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_id.to_string(),
                content: "ok".into(),
                is_error: None,
            }],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        }
    }

    fn append(map: &mut SessionEventMap, id: &str, message: StoredMessage) {
        map.append_event(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: id.to_string(),
            op: SessionEventOp::AppendMessage {
                message_id: message.id.clone(),
                message,
            },
            parent_id: None,
            version: 1,
        });
    }

    #[test]
    fn green_log_has_no_violations() {
        let mut map = SessionEventMap::default();
        append(&mut map, "e1", text_msg("m1"));
        append(&mut map, "e2", tool_use_msg("m2", "tool_1"));
        append(&mut map, "e3", tool_result_msg("m3", "tool_1"));
        let reg = InvariantRegistry::builtin();
        let log = reg.check(&map);
        assert!(
            log.is_green(),
            "expected green log, got {:#?}",
            log.violations
        );
    }

    #[test]
    fn unbalanced_tool_pairing_is_detected() {
        let mut map = SessionEventMap::default();
        append(&mut map, "e1", tool_use_msg("m1", "tool_1"));
        // No ToolResult -> open tool call must be flagged.
        let reg = InvariantRegistry::builtin();
        let log = reg.check(&map);
        assert!(!log.is_green(), "expected an open tool_call violation");
        assert!(
            log.violations
                .iter()
                .any(|v| v.invariant == "session.tool_pairing_balanced"),
            "violation list: {:#?}",
            log.violations
        );
    }

    #[test]
    fn dangling_parent_edge_is_detected() {
        let mut map = SessionEventMap::default();
        append(&mut map, "e1", text_msg("m1"));
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "e2".to_string(),
            op: SessionEventOp::ClearAll,
            parent_id: Some("ghost".to_string()),
            version: 1,
        });
        let reg = InvariantRegistry::builtin();
        let log = reg.check(&map);
        assert!(
            log.violations
                .iter()
                .any(|v| v.invariant == "session.parent_edges_resolve"),
            "violation list: {:#?}",
            log.violations
        );
    }

    #[test]
    fn projection_seam_folds_counts() {
        let mut map = SessionEventMap::default();
        append(&mut map, "e1", text_msg("m1"));
        append(&mut map, "e2", text_msg("m2"));
        append(&mut map, "e3", text_msg("m3"));
        let count = project_map::<MessageCountProjection>(&map).expect("valid projection");
        assert_eq!(count, 3);

        // ClearAll resets the projection to zero.
        map.append_event(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "e4".to_string(),
            op: SessionEventOp::ClearAll,
            parent_id: None,
            version: 1,
        });
        let count = project_map::<MessageCountProjection>(&map).expect("valid projection");
        assert_eq!(count, 0);
    }

    #[test]
    fn projection_folds_partial_splice_like_derive_messages() {
        let mut map = SessionEventMap::default();
        append(&mut map, "e1", text_msg("m1"));
        append(&mut map, "e2", text_msg("m2"));
        append(&mut map, "e3", text_msg("m3"));
        append(&mut map, "e4", text_msg("m4"));
        assert_eq!(map.derive_messages().len(), 4);

        // Partial replacement: replace m1..m3 (indices 1..=3) with a single
        // message. Real derived transcript is 4 + 1 - (3-1) = 3.
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "e_replace".to_string(),
            op: SessionEventOp::ReplaceMessages {
                start_index: 1,
                end_index: 3,
                messages: vec![text_msg("mnew")],
            },
            parent_id: None,
            version: 1,
        });
        assert_eq!(map.derive_messages().len(), 3, "sanity: derive_messages");
        assert_eq!(
            project_map::<MessageCountProjection>(&map).expect("valid projection"),
            3,
            "projection must match derive_messages for a partial splice"
        );

        // Full replacement (start=0, end=usize::MAX) collapses to the replacement size.
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "e_full".to_string(),
            op: SessionEventOp::ReplaceMessages {
                start_index: 0,
                end_index: usize::MAX,
                messages: vec![text_msg("only"), text_msg("two")],
            },
            parent_id: None,
            version: 1,
        });
        assert_eq!(map.derive_messages().len(), 2, "sanity: full replace");
        assert_eq!(
            project_map::<MessageCountProjection>(&map).expect("valid projection"),
            2,
            "projection must match derive_messages for a full replacement"
        );
    }

    /// Property-style consistency: the `MessageCountProjection` fold must agree
    /// with `derive_messages().len()` on every prefix of an arbitrary sequence of
    /// message ops — including malformed ones (out-of-range inserts, reversed
    /// `ReplaceMessages` spans, clamped indices). Since chat transcripts are
    /// corruption-tolerant by design, the projection used for derived-state
    /// monitoring must never diverge from the real fold even on a torn log.
    #[test]
    fn projection_agrees_with_derive_on_malformed_sequence() {
        let mut map = SessionEventMap::default();
        // Growing phase.
        for i in 0..5 {
            append(&mut map, &format!("a{i}"), text_msg(&format!("m{i}")));
        }
        // A replace with a reversed span (start > end) must not crash or diverge.
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "rev".to_string(),
            op: SessionEventOp::ReplaceMessages {
                start_index: 3,
                end_index: 1,
                messages: vec![text_msg("zz")],
            },
            parent_id: None,
            version: 1,
        });
        // An out-of-range insert (index beyond the live length) must clamp.
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "oob".to_string(),
            op: SessionEventOp::InsertMessage {
                index: 99,
                message: text_msg("oob"),
            },
            parent_id: None,
            version: 1,
        });
        // A full replacement collapse.
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "full".to_string(),
            op: SessionEventOp::ReplaceMessages {
                start_index: 0,
                end_index: usize::MAX,
                messages: vec![text_msg("x"), text_msg("y")],
            },
            parent_id: None,
            version: 1,
        });
        // A partial splice near the live length boundary.
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "partial".to_string(),
            op: SessionEventOp::ReplaceMessages {
                start_index: 0,
                end_index: 1,
                messages: vec![text_msg("only")],
            },
            parent_id: None,
            version: 1,
        });
        // A ClearAll MID-SEQUENCE (not just as a final op) must reset the
        // projection to zero, after which subsequent appends/inserts/replaces
        // rebuild from an empty base. This is the reset-then-append edge that a
        // per-prefix consistency check must not diverge on.
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "clear_mid".to_string(),
            op: SessionEventOp::ClearAll,
            parent_id: None,
            version: 1,
        });
        // Post-clear append (rebuilds from empty).
        append(&mut map, "post0", text_msg("p0"));
        // Post-clear replace (start == end when empty must append, not no-op).
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "post_repl".to_string(),
            op: SessionEventOp::ReplaceMessages {
                start_index: 0,
                end_index: usize::MAX,
                messages: vec![text_msg("x"), text_msg("y")],
            },
            parent_id: None,
            version: 1,
        });

        // Verify on the FULL set and on every prefix, the projection never panics
        // and always matches the real derived length.
        for cut in 0..=map.events.len() {
            let prefix: Vec<SessionEvent> = map.events[..cut].to_vec();
            let projected = fold_projection::<MessageCountProjection>(&prefix).expect("no panic");
            // Derive from a map holding exactly the same prefix. `events` is the
            // public storage; `derive_messages` ignores the private cache.
            let mut m = SessionEventMap::default();
            m.events = prefix;
            assert_eq!(
                projected,
                m.derive_messages().len(),
                "projection diverged from derive_messages at prefix {cut}"
            );
        }
    }
}
