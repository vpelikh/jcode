//! Session-log invariant registry (deepseek-harness takeaway #3 and #4).
//!
//! dsh ships a package-owned `ctx.invariants` registry of runtime assertions;
//! its headline invariant is "anything that reaches a model request must be
//! reconstructable from the session log." We take the same shape here: a small
//! set of *named* checks over the append-only event log. [`InvariantLog::enforce`]
//! is the seam that turns a violation into a hard `debug_assert!` panic in dev
//! and a structured log/metric in release. `enforce` is wired at a safe,
//! deliberately narrow call site in [`Session::compact_transcript_with_bracket`]:
//! after WE open and close a fresh bracket, an open/duplicated bracket there is
//! provably a bug. When the method merely COMPLETES a pre-existing orphaned
//! bracket (crash recovery), the strict enforce is skipped so a shallower,
//! still-open orphan (a real, pre-existing "incomplete compaction" signal) does
//! not hard-fail the recovery. Other callers adopt `enforce` explicitly; notably
//! it is NOT enforced on the plain load/resume path, because a crashed session
//! legitimately carries an open bracket / unanswered tool call there. The
//! built-in checks additionally run as a *diagnostic* pass on the load path
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
use jcode_message_types::{ContentBlock, Role};
use jcode_session_types::StoredMessage;
use std::any::Any;
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
    /// The fold result type. Must be `'static`, `Send`, and `Sync` so the
    /// [`ProjectionRegistry`] can erase it into a `Box<dyn Any + Send + Sync>`
    /// shared across builder threads.
    type State: Default + 'static + Send + Sync;

    /// Stable name for the projection, used to address its derived state in a
    /// [`ProjectionRegistry`] registry (safe for metrics tags and debugging).
    fn name() -> &'static str;

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

/// One registered projection unit inside a [`ProjectionRegistry`]: an erased
/// `dyn Any` running state together with the concrete apply/validate closures.
///
/// Erasing the state lets a registry hold *many* heterogeneous projections and
/// fold them all in a single pass, the core of dsh's "one fold, many readers"
/// (takeaway #4).
struct ProjectionUnit {
    name: &'static str,
    state: Box<dyn Any + Send + Sync>,
    fresh: fn() -> Box<dyn Any + Send + Sync>,
    apply: fn(&mut dyn Any, &SessionEvent),
    validate: fn(&dyn Any) -> Result<(), InvariantViolation>,
}

impl ProjectionUnit {
    fn new<P: LogProjection>() -> Self {
        fn fresh_impl<P: LogProjection>() -> Box<dyn Any + Send + Sync> {
            Box::new(P::State::default())
        }
        fn apply_impl<P: LogProjection>(state: &mut dyn Any, event: &SessionEvent) {
            // Invariant: the boxed state is always P::State because we only ever
            // insert it via `ProjectionRegistry::add::<P>()`.
            let state = state.downcast_mut::<P::State>().expect(
                "ProjectionRegistry internal state downcast failed: state type mismatch",
            );
            P::apply(state, event);
        }
        fn validate_impl<P: LogProjection>(state: &dyn Any) -> Result<(), InvariantViolation> {
            let state = state
                .downcast_ref::<P::State>()
                .expect("ProjectionRegistry validate downcast failed: state type mismatch");
            P::validate(state)
        }
        ProjectionUnit {
            name: P::name(),
            state: fresh_impl::<P>(),
            fresh: fresh_impl::<P>,
            apply: apply_impl::<P>,
            validate: validate_impl::<P>,
        }
    }

    fn reset_active(&mut self) {
        self.state = (self.fresh)();
    }
}

/// A registry of [`LogProjection`] units (takeaway #4) that folds *all* of them
/// in a single pass over the log, then exposes each typed derived state by name.
///
/// This is dsh's incremental-projection pattern: consumers subscribe to derived
/// state instead of re-scanning the raw event stream. Two usage shapes are
/// supported:
/// - **Bootstrap**: [`fold`](Self::fold) recomputes every state from a fresh
///   empty default, so it is idempotent — call it once at startup over the whole
///   log.
/// - **Incremental**: after the initial fold, [`apply`](Self::apply) keeps the
///   running states current by folding only newly-appended events, so the registry
///   never refolds the whole log per append. The current folded states are read
///   back via [`current`](Self::current).
#[derive(Default)]
pub struct ProjectionRegistry {
    units: Vec<ProjectionUnit>,
}

impl ProjectionRegistry {
    /// The default projection set. Callers may add their own with [`add`](Self::add).
    pub fn builtin() -> Self {
        let mut r = Self::default();
        r.add::<MessageCountProjection>();
        r.add::<LiveTranscriptProjection>();
        r.add::<RoleCountsProjection>();
        r
    }

    /// Register an additional projection. Fold order is insert order.
    pub fn add<P: LogProjection>(&mut self) {
        // Re-checking avoids registering the same name twice, which would make
        // lookups ambiguous. Skips (with a warning) if a caller tries to
        // register a duplicate.
        if self.units.iter().any(|u| u.name == P::name()) {
            crate::logging::warn(&format!(
                "projection registry: skipping duplicate projection '{}'",
                P::name()
            ));
            return;
        }
        self.units.push(ProjectionUnit::new::<P>());
    }

    /// Number of registered projections.
    pub fn len(&self) -> usize {
        self.units.len()
    }

    /// True when no projections are registered.
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// Names of every registered projection, in registration order.
    pub fn names(&self) -> Vec<&'static str> {
        self.units.iter().map(|u| u.name).collect()
    }

    /// Fold the whole log through every registered projection, resetting each
    /// running state to its default first so the fold is idempotent (reusable at
    /// bootstrap). Validates every state afterwards. Postcondition: the current
    /// running states are fully folded — read them back via [`current`](Self::current).
    pub fn fold(&mut self, events: &[SessionEvent]) -> Result<(), InvariantViolation> {
        // Reset all states to their default so a re-fold means "replay from empty",
        // never "append to a stale prefix".
        for unit in &mut self.units {
            unit.reset_active();
        }
        for event in events {
            for unit in &mut self.units {
                (unit.apply)(&mut *unit.state, event);
            }
        }
        // Validate each state after the full fold.
        for unit in &self.units {
            (unit.validate)(&*unit.state)?;
        }
        Ok(())
    }

    /// Read the current running states as a [`ProjectionResults`] view that
    /// borrows from this registry.
    pub fn current(&self) -> ProjectionResults<'_> {
        ProjectionResults {
            entries: self
                .units
                .iter()
                .map(|u| ProjectionEntry {
                    name: u.name,
                    state: &*u.state,
                })
                .collect(),
        }
    }

    /// Incremental seam: apply a single new event to every registered projection
    /// so a running registry can stay current without refolding the log.
    /// Cost per event is projection-specific: O(1) for counters that track
    /// deltas, O(span) for projections that splice (e.g. the live transcript).
    /// Equivalent to [`fold`](Self::fold) on the prefix plus this event, without
    /// re-walking the earlier events.
    pub fn apply(&mut self, event: &SessionEvent) {
        for unit in &mut self.units {
            (unit.apply)(&mut *unit.state, event);
        }
    }

    /// Validate every registered projection against its current running state,
    /// collecting the first error for each.
    pub fn validate_all(&self) -> Vec<InvariantViolation> {
        self.units
            .iter()
            .filter_map(|u| (u.validate)(&*u.state).err())
            .collect()
    }
}

/// A borrowed, named, typed slice of derived state produced by a
/// [`ProjectionRegistry`]. Mirrors dsh's `stateOf(key)` accessor.
#[derive(Default)]
pub struct ProjectionResults<'a> {
    entries: Vec<ProjectionEntry<'a>>,
}

struct ProjectionEntry<'a> {
    name: &'static str,
    state: &'a dyn Any,
}

impl<'a> ProjectionResults<'a> {
    /// The typed state for projection `P`, if it was registered and folded.
    pub fn get<P: LogProjection>(&self) -> Option<&P::State> {
        self.entries
            .iter()
            .find(|e| e.name == P::name())
            .and_then(|e| e.state.downcast_ref::<P::State>())
    }

    /// Names of every projection present in these results.
    pub fn names(&self) -> Vec<&'static str> {
        self.entries.iter().map(|e| e.name).collect()
    }

    /// True when every registered projection is present.
    pub fn contains<P: LogProjection>(&self) -> bool {
        self.get::<P>().is_some()
    }
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
        let mut open: Vec<super::ToolCallId> = Vec::new();
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

/// The estimated-derived-state invariant (takeaway #3 + #4): the incremental
/// `LiveTranscriptProjection` must agree byte-for-byte with the on-demand
/// `SessionEventMap::derive_messages()`. Both implement the same event fold,
/// so this invariant guards against the two implementations drifting apart.
///
/// This is the consumer that makes the event-sourced log worthwhile: a future
/// refactor that switches a hot path to read from the registry (instead of
/// re-scanning the stream with `derive_messages`) can run this on the load path
/// to prove the cheaper projection never diverges from the established source.
pub struct ProjectionMatchesDerived;

impl LogInvariant for ProjectionMatchesDerived {
    fn name(&self) -> &'static str {
        "session.projection_matches_derived"
    }

    fn check(&self, map: &SessionEventMap) -> Result<(), InvariantViolation> {
        let derived = map.derive_messages();
        let projected = project_map::<LiveTranscriptProjection>(map)?;
        // `StoredMessage` is deliberately not `PartialEq`; compare the canonical
        // serialized forms, which is what actually matters (identical transcripts).
        let a = serde_json::to_vec(&derived).map_err(|e| {
            InvariantViolation::new(
                "session.projection_matches_derived",
                format!("derived transcript failed to serialize: {e}"),
            )
        })?;
        let b = serde_json::to_vec(&projected).map_err(|e| {
            InvariantViolation::new(
                "session.projection_matches_derived",
                format!("projected transcript failed to serialize: {e}"),
            )
        })?;
        if a != b {
            return Err(InvariantViolation::new(
                "session.projection_matches_derived",
                format!(
                    "LiveTranscriptProjection ({len} msgs) diverged from derive_messages ({len2} msgs)",
                    len = projected.len(),
                    len2 = derived.len(),
                ),
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
        r.add(ProjectionMatchesDerived);
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

    fn name() -> &'static str {
        "session.message_count"
    }

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

/// The **live transcript**: the ordered `Vec<StoredMessage>` currently
/// projected from the log. This is the concrete realization of takeaway #4's
/// "derive the transcript from the log, don't re-read it" — the same operation
/// `SessionEventMap::derive_messages` performs on demand, kept incrementally
/// current by the registry instead of recomputed from scratch on every read.
///
/// The fold mirrors `derive_messages` splice semantics exactly (capped indices,
/// corruption-tolerant on reversed bounds) so the projection and the derived
/// method always agree.
pub struct LiveTranscriptProjection;

impl LogProjection for LiveTranscriptProjection {
    type State = Vec<StoredMessage>;

    fn name() -> &'static str {
        "session.live_transcript"
    }

    fn apply(state: &mut Self::State, event: &SessionEvent) {
        match &event.op {
            SessionEventOp::AppendMessage { message, .. } => {
                state.push(message.clone());
            }
            SessionEventOp::InsertMessage {
                index,
                message,
                ..
            } => {
                let index = (*index).min(state.len());
                state.insert(index, message.clone());
            }
            SessionEventOp::ReplaceMessages {
                start_index,
                end_index,
                messages,
                ..
            } => {
                let bounds = SpliceBounds::of(*start_index, *end_index, state.len());
                state.splice(bounds.start..bounds.end, messages.iter().cloned());
            }
            SessionEventOp::ClearAll => state.clear(),
            _ => {}
        }
    }
}

/// Canonical `ReplaceMessages` splice bounds, mirroring
/// `SessionEventMap::derive_messages` exactly (including its clamping order) so
/// every consumer of a splice agrees byte-for-byte. Each bound is clamped to the
/// live length first, then `end` is forced `>= start` so a *reversed* span
/// (`start_index > end_index`) degrades to `start == end` — a point-insertion,
/// never a crash.
struct SpliceBounds {
    start: usize,
    end: usize,
}

impl SpliceBounds {
    fn of(start_index: usize, end_index: usize, live_len: usize) -> Self {
        let start = start_index.min(live_len);
        let raw_end = end_index.min(live_len);
        Self {
            start,
            end: raw_end.max(start),
        }
    }
}

/// Per-role message counts derived from the live transcript.
///
/// This is takeaway #4's "one fold, many readers" demonstration: it reads a
/// *different* derived domain than [`MessageCountProjection`] (a total) and
/// [`LiveTranscriptProjection`] (the full transcript) while sharing the single
/// registry fold. A TUI or usage overlay can render a per-role breakdown
/// without scanning the raw stream or re-walking the transcript; it just looks
/// up this typed state by name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoleCounts {
    pub user: usize,
    pub assistant: usize,
}

impl RoleCounts {
    /// Total messages across all roles.
    pub fn total(&self) -> usize {
        self.user + self.assistant
    }
}

/// Projects [`RoleCounts`] from the log: user vs assistant message counts.
///
/// Its state keeps a compact `Vec<Role>` (not full messages) alongside running
/// counts, updated by *delta* per event — O(1) on append/insert, O(span) on a
/// replace — so it never re-walks the whole transcript per event and clones no
/// message payloads. Bounds mirror the shared [`SpliceBounds`] helper so it
/// agrees with [`LiveTranscriptProjection`]/`derive_messages` on every splice.
pub struct RoleCountsProjection;

/// Internal running state: a compact per-role list plus running totals.
#[derive(Debug, Clone, Default)]
pub struct RoleCountsState {
    /// Roles in live message order (a memory-light stand-in for the transcript).
    pub roles: Vec<Role>,
    /// Per-role counts over `roles`.
    pub counts: RoleCounts,
}

impl RoleCountsState {
    fn add(&mut self, role: &Role) {
        match role {
            Role::User => self.counts.user += 1,
            Role::Assistant => self.counts.assistant += 1,
        }
    }

    fn subtract(&mut self, role: &Role) {
        match role {
            Role::User => self.counts.user = self.counts.user.saturating_sub(1),
            Role::Assistant => self.counts.assistant = self.counts.assistant.saturating_sub(1),
        }
    }
}

impl LogProjection for RoleCountsProjection {
    type State = RoleCountsState;

    fn name() -> &'static str {
        "session.role_counts"
    }

    fn apply(state: &mut Self::State, event: &SessionEvent) {
        match &event.op {
            SessionEventOp::AppendMessage { message, .. } => {
                state.roles.push(message.role.clone());
                state.add(&message.role);
            }
            SessionEventOp::InsertMessage {
                index,
                message,
                ..
            } => {
                let index = (*index).min(state.roles.len());
                state.roles.insert(index, message.role.clone());
                state.add(&message.role);
            }
            SessionEventOp::ReplaceMessages {
                start_index,
                end_index,
                messages,
                ..
            } => {
                let bounds = SpliceBounds::of(*start_index, *end_index, state.roles.len());
                // Remove the roles in the replaced span, then add the incoming
                // replacement roles. Only the new ones are added — the surviving
                // tail (after `end`) was never subtracted, so counting it again
                // would double it. Clone the removed roles first to avoid
                // borrowing `state.roles` while mutating `state`.
                let removed: Vec<Role> = state.roles[bounds.start..bounds.end].to_vec();
                for role in &removed {
                    state.subtract(role);
                }
                state
                    .roles
                    .splice(bounds.start..bounds.end, messages.iter().map(|m| m.role.clone()));
                for m in messages {
                    state.add(&m.role);
                }
            }
            SessionEventOp::ClearAll => {
                state.roles.clear();
                state.counts = RoleCounts::default();
            }
            _ => {}
        }
    }

    fn validate(state: &Self::State) -> Result<(), InvariantViolation> {
        // Internal self-consistency: the running totals must match the folded
        // roles list. This catches a future edit that drifts one of `roles` or
        // `counts` (the two are maintained by hand in `apply`).
        let derived = state.roles.iter().fold(
            RoleCounts::default(),
            |mut c, role| {
                match role {
                    Role::User => c.user += 1,
                    Role::Assistant => c.assistant += 1,
                }
                c
            },
        );
        if derived != state.counts {
            return Err(InvariantViolation::new(
                "session.role_counts_consistent",
                format!(
                    "role-counts projection self-inconsistent: roles derive {user}/{assistant} \
                     but counts hold {cu}/{ca}",
                    user = derived.user,
                    assistant = derived.assistant,
                    cu = state.counts.user,
                    ca = state.counts.assistant,
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn text_msg_user(id: &str) -> StoredMessage {
        let mut m = text_msg(id);
        m.role = Role::User;
        m
    }

    fn text_msg_assistant(id: &str) -> StoredMessage {
        text_msg(id)
    }

    fn tool_use_msg(id: &str, tool_id: &str) -> StoredMessage {
        StoredMessage {
            id: id.to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: tool_id.to_string().into(),
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
                tool_use_id: tool_id.to_string().into(),
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
            event_id: id.to_string().into(),
            op: SessionEventOp::AppendMessage {
                message_id: message.id.clone().into(),
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
            event_id: "e2".to_string().into(),
            op: SessionEventOp::ClearAll,
            parent_id: Some("ghost".to_string().into()),
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
            event_id: "e4".to_string().into(),
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
            event_id: "e_replace".to_string().into(),
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
            event_id: "e_full".to_string().into(),
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
            event_id: "rev".to_string().into(),
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
            event_id: "oob".to_string().into(),
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
            event_id: "full".to_string().into(),
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
            event_id: "partial".to_string().into(),
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
            event_id: "clear_mid".to_string().into(),
            op: SessionEventOp::ClearAll,
            parent_id: None,
            version: 1,
        });
        // Post-clear append (rebuilds from empty).
        append(&mut map, "post0", text_msg("p0"));
        // Post-clear replace (start == end when empty must append, not no-op).
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "post_repl".to_string().into(),
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

    #[test]
    fn registry_folds_multiple_projections_in_one_pass() {
        let mut map = SessionEventMap::default();
        append(&mut map, "e1", text_msg("m1"));
        append(&mut map, "e2", text_msg("m2"));

        let mut reg = ProjectionRegistry::builtin();
        reg.fold(&map.events).expect("fold should succeed");
        let view = reg.current();

        // Both built-in projections are present, one fold, two readers.
        assert!(view.contains::<MessageCountProjection>());
        assert!(view.contains::<LiveTranscriptProjection>());
        assert_eq!(view.get::<MessageCountProjection>(), Some(&2));
        assert_eq!(
            view.get::<LiveTranscriptProjection>().map(Vec::len),
            Some(2)
        );
        // The live transcript matches the derived transcript exactly.
        let live = view
            .get::<LiveTranscriptProjection>()
            .expect("live transcript present");
        let derive = map.derive_messages();
        assert_eq!(
            live.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            derive.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            "live transcript must mirror derive_messages' ordering and content"
        );
    }

    #[test]
    fn registry_incremental_apply_matches_full_refold() {
        let mut map = SessionEventMap::default();
        append(&mut map, "e1", text_msg("m1"));

        // Bootstrap: fold the initial log.
        let mut reg = ProjectionRegistry::builtin();
        reg.fold(&map.events).expect("initial fold ok");
        let bootstrap_cur = reg.current();
        let bootstrap_ids = bootstrap_cur
            .get::<LiveTranscriptProjection>()
            .unwrap()
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(bootstrap_ids, vec!["m1".to_string()], "bootstrap folded one message");

        // Incremental: append new events and apply ONLY the delta.
        append(&mut map, "e2", text_msg("m2"));
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "e_repl".to_string().into(),
            op: SessionEventOp::ReplaceMessages {
                start_index: 0,
                end_index: usize::MAX,
                messages: vec![text_msg("mnew")],
            },
            parent_id: None,
            version: 1,
        });
        for event in &map.events[1..] {
            reg.apply(event);
        }

        // The incremental path must equal a full re-fold of the same log.
        let mut refold = ProjectionRegistry::builtin();
        refold.fold(&map.events).expect("refold ok");
        let inc_cur = reg.current();
        let refold_cur = refold.current();
        let inc_ids = inc_cur
            .get::<LiveTranscriptProjection>()
            .unwrap()
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>();
        let refold_ids = refold_cur
            .get::<LiveTranscriptProjection>()
            .unwrap()
            .iter()
            .map(|m| m.id.clone())
            .collect::<Vec<_>>();
        let inc_count = inc_cur.get::<MessageCountProjection>().copied();
        let refold_count = refold_cur.get::<MessageCountProjection>().copied();
        assert_eq!(
            inc_ids, refold_ids,
            "incremental apply diverged from a fresh full fold"
        );
        assert_eq!(inc_count, refold_count);
        // And it must match derive_messages.
        let derive_ids = map.derive_messages().into_iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(inc_ids, derive_ids, "live transcript must match derive_messages");
    }

    #[test]
    fn projection_matches_derived_across_malformed_sequences() {
        // Stress the differential invariant: for a corpus of logs that exercise
        // every splice/insert edge (full replace, clamped insert, reversed
        // bounds, clear+rebuild), the incremental projection and the derived
        // method must agree byte-for-byte. Guards against the two folds
        // drifting apart.
        let mut cases: Vec<Vec<SessionEvent>> = Vec::new();

        // 1. Plain appends.
        let mut m = SessionEventMap::default();
        append(&mut m, "e1", text_msg("a"));
        append(&mut m, "e2", text_msg("b"));
        cases.push(m.events.clone());

        // 2. Partial splice then clear.
        m.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "sp1".to_string().into(),
            op: SessionEventOp::ReplaceMessages {
                start_index: 0,
                end_index: 1,
                messages: vec![text_msg("x"), text_msg("y")],
            },
            parent_id: None,
            version: 1,
        });
        m.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "clr".to_string().into(),
            op: SessionEventOp::ClearAll,
            parent_id: None,
            version: 1,
        });
        append(&mut m, "post", text_msg("p"));
        cases.push(m.events.clone());

        // 3. Reversed replace bounds (corruption-tolerant): start > end.
        let mut m2 = SessionEventMap::default();
        append(&mut m2, "e1", text_msg("a"));
        append(&mut m2, "e2", text_msg("b"));
        m2.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "rev".to_string().into(),
            op: SessionEventOp::ReplaceMessages {
                start_index: 5,
                end_index: 1,
                messages: vec![text_msg("z")],
            },
            parent_id: None,
            version: 1,
        });
        cases.push(m2.events.clone());

        // 4. Replace beyond live length (append when empty).
        let mut m3 = SessionEventMap::default();
        m3.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "full".to_string().into(),
            op: SessionEventOp::ReplaceMessages {
                start_index: 0,
                end_index: usize::MAX,
                messages: vec![text_msg("only")],
            },
            parent_id: None,
            version: 1,
        });
        cases.push(m3.events.clone());

        // The invariant must hold on every case and every prefix of it.
        for (ci, events) in cases.iter().enumerate() {
            for cut in 0..=events.len() {
                let mut map = SessionEventMap::default();
                map.events = events[..cut].to_vec();
                let check = ProjectionMatchesDerived;
                assert!(
                    check.check(&map).is_ok(),
                    "projection/derived diverged on case {ci} prefix {cut}"
                );
            }
        }
    }

    #[test]
    fn one_fold_serves_transcript_and_role_domains() {
        // Takeaway #4's "one fold, many readers": a single fold over the log
        // must produce the full transcript AND a distinct per-role breakdown,
        // so a UI can render either without re-walking the raw stream.
        let mut map = SessionEventMap::default();
        append(&mut map, "e1", text_msg_user("m1"));
        append(&mut map, "e2", text_msg_assistant("m2"));
        append(&mut map, "e3", text_msg_user("m3"));

        let mut reg = ProjectionRegistry::builtin();
        reg.fold(&map.events).expect("fold ok");

        let cur = reg.current();
        // Transcript domain (full messages).
        let transcript = cur
            .get::<LiveTranscriptProjection>()
            .expect("transcript projection present");
        assert_eq!(transcript.len(), 3);

        // Distinct role domain, from the SAME single fold.
        let rc = cur
            .get::<RoleCountsProjection>()
            .expect("role-counts projection present");
        assert_eq!(rc.counts, RoleCounts { user: 2, assistant: 1 });
        assert_eq!(rc.counts.total(), 3);
        // The role-counts projection tracks the same number of messages as the
        // full transcript, with no full message payloads stored.
        assert_eq!(rc.roles.len(), transcript.len());
    }

    #[test]
    fn registry_introspection_api_is_consistent() {
        // Cover the registry/results introspection surface so it is not
        // dead-and-untested public API.
        let mut map = SessionEventMap::default();
        append(&mut map, "e1", text_msg_user("m1"));
        append(&mut map, "e2", text_msg_assistant("m2"));

        // Empty registry: len 0, is_empty, names empty, no projections.
        let empty = ProjectionRegistry::default();
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
        assert!(empty.names().is_empty());

        let mut reg = ProjectionRegistry::builtin();
        assert_eq!(reg.len(), 3);
        assert!(!reg.is_empty());
        let names = reg.names();
        assert!(names.contains(&"session.message_count"));
        assert!(names.contains(&"session.live_transcript"));
        assert!(names.contains(&"session.role_counts"));

        reg.fold(&map.events).expect("fold ok");
        let view = reg.current();
        // Results::names lists every folded projection.
        let result_names = view.names();
        assert_eq!(result_names.len(), 3);
        assert!(result_names.contains(&"session.live_transcript"));

        // validate_all is green for a well-formed sequence.
        assert!(reg.validate_all().is_empty());
    }

    #[test]
    fn role_counts_validate_catches_drifted_totals() {
        // The RoleCountsProjection.validate self-check must flag a state where
        // `counts` and `roles` have drifted (simulating a future edit bug).
        let mut map = SessionEventMap::default();
        append(&mut map, "e1", text_msg_user("m1"));
        append(&mut map, "e2", text_msg_assistant("m2"));

        let mut reg = ProjectionRegistry::builtin();
        reg.fold(&map.events).expect("fold ok");
        assert!(reg.validate_all().is_empty(), "well-formed fold is clean");

        // Build a synthetic drifted RoleCountsState and validate it directly.
        let mut bad = RoleCountsState::default();
        bad.roles.push(Role::User);
        bad.counts.user = 99; // drifted: roles has 1 user, counts says 99.
        let err = RoleCountsProjection::validate(&bad).expect_err("must flag drift");
        assert_eq!(err.invariant, "session.role_counts_consistent");
    }

    #[test]
    fn role_counts_track_splices() {
        // Role counts must stay correct across a replace (splice) that reshapes
        // the transcript, not just append-at-end.
        let mut map = SessionEventMap::default();
        append(&mut map, "e1", text_msg_user("m1"));
        append(&mut map, "e2", text_msg_assistant("m2"));
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "repl".to_string().into(),
            op: SessionEventOp::ReplaceMessages {
                start_index: 0,
                end_index: usize::MAX,
                messages: vec![text_msg_user("only-user")],
            },
            parent_id: None,
            version: 1,
        });
        let mut reg = ProjectionRegistry::builtin();
        reg.fold(&map.events).expect("fold ok");
        let cur = reg.current();
        let rc = cur.get::<RoleCountsProjection>().expect("present");
        assert_eq!(rc.counts, RoleCounts { user: 1, assistant: 0 });
        assert_eq!(rc.counts.total(), 1);
    }

    #[test]
    fn role_counts_track_partial_and_reversed_splices() {
        // Partial splice removing a mixed span must update counts via the delta
        // math, and a reversed-bounds span must not corrupt them.
        let mut map = SessionEventMap::default();
        append(&mut map, "e1", text_msg_user("m1"));
        append(&mut map, "e2", text_msg_assistant("m2"));
        append(&mut map, "e3", text_msg_user("m3"));
        append(&mut map, "e4", text_msg_assistant("m4"));
        // Partial: replace indices 1..=2 ([Asst, User]) with a single [User].
        // Live roles before: [User, Asst, User, Asst].
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "partial".to_string().into(),
            op: SessionEventOp::ReplaceMessages {
                start_index: 1,
                end_index: 3,
                messages: vec![text_msg_user("new")],
            },
            parent_id: None,
            version: 1,
        });
        // Assert against derive_messages, which stays the ground truth.
        let rc_before_partial = {
            let mut reg = ProjectionRegistry::builtin();
            reg.fold(&map.events).expect("fold ok");
            reg.current().get::<RoleCountsProjection>().expect("present").clone()
        };
        let derived = map.derive_messages();
        let (u, a) = derived
            .iter()
            .fold((0, 0), |(u, a), m| match m.role {
                Role::User => (u + 1, a),
                Role::Assistant => (u, a + 1),
            });
        assert_eq!(rc_before_partial.counts, RoleCounts { user: u, assistant: a });
        assert_eq!(rc_before_partial.counts.total(), derived.len());

        // Reversed-bounds replace: must be a no-op (point-insertion at end),
        // counts unchanged.
        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "reversed".to_string().into(),
            op: SessionEventOp::ReplaceMessages {
                start_index: 99,
                end_index: 1,
                messages: vec![text_msg_assistant("sneaky")],
            },
            parent_id: None,
            version: 1,
        });
        let mut reg = ProjectionRegistry::builtin();
        reg.fold(&map.events).expect("fold ok");
        let cur = reg.current();
        let rc = cur.get::<RoleCountsProjection>().expect("present");
        let derived = map.derive_messages();
        let (u, a) = derived
            .iter()
            .fold((0, 0), |(u, a), m| match m.role {
                Role::User => (u + 1, a),
                Role::Assistant => (u, a + 1),
            });
        assert_eq!(rc.counts, RoleCounts { user: u, assistant: a });
        assert_eq!(rc.counts.total(), derived.len());
    }
}
