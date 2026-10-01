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
///   back via [`current`](Self::current) or the allocation-free
///   [`get`](Self::get).
#[derive(Default)]
pub struct ProjectionRegistry {
    units: Vec<ProjectionUnit>,
}

impl std::fmt::Debug for ProjectionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectionRegistry")
            .field("names", &self.names())
            .finish()
    }
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

    /// Allocation-free typed access to a single projection's running state,
    /// bypassing the [`ProjectionResults`] entry vector that
    /// [`current`](Self::current) builds. This is the hot-path accessor for
    /// consumers that read one derived state per call (e.g.
    /// [`Session::projected_messages`](crate::session::Session::projected_messages)
    /// reading `LiveTranscriptProjection`). Returns `None` if `P` was not
    /// registered, so consumers that *require* the projection should
    /// expect/unwrap (mirroring dsh's "no silent default" contract).
    pub fn get<P: LogProjection>(&self) -> Option<&P::State> {
        self.units
            .iter()
            .find(|u| u.name == P::name())
            .and_then(|u| u.state.downcast_ref::<P::State>())
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

/// The append-seam **incremental projection cache** (follow-up f1).
///
/// dsh keeps projection units incrementally current at the log append seam so a
/// reader no longer refolds the whole log per read. The folding cost moves to the
/// append seam (one event per plain append; a splice folds its span), and a read
/// against a current cache folds nothing — it still pays whatever its own
/// accessor costs, e.g. cloning the transcript. jcode's [`ProjectionRegistry`] is
/// that fold; this wrapper is the *bookkeeping* that makes it an append-seam
/// cache rather than a one-shot fold:
///
/// - `registry`: the running [`ProjectionRegistry`] holding every projection's
///   derived state, kept current by [`apply_event`](Self::apply_event).
/// - `folded_len`: how many of the owning log's *leading* events have already
///   been folded into `registry`. Events are only ever appended (the log is
///   append-only) or wholesale replaced (deserialize / rebuild). A plain append
///   of event `n+1` is `n → n+1`; any reset drops it to `0` so the next read
///   folds from scratch. `folded_len` is always `<= events.len()`. **When the
///   log is only ever appended through [`apply_event`](Self::apply_event), or
///   wholesale replaced by a fresh map, equality means the registry's derived
///   state is identical to a full fold of `events`.** The one deliberate
///   exception is an in-place mutation of the `pub` `events` field that bypasses
///   both: the `folded_head_id` fingerprint catches a
///   shrink or a changed fold-boundary id, but not an interior edit that keeps
///   the boundary id. That residue is out of scope for O(1) healing (sound
///   detection would need an O(prefix) re-hash per read, defeating the cache);
///   it is *not* detected at runtime: no in-crate path mutates `events` in place
///   behind the append seam, so the residue is prevented by convention (route
///   every mutation through [`SessionEventMap::append_event`] or call
///   [`reset`](Self::reset)). The load-path `ProjectionCacheMatchesDerived`
///   invariant separately guards the cache wrapper's *apply semantics* against
///   `derive_messages` (it is rebuilt fresh on load, so it cannot observe a stale
///   live cache).
///
/// The cache is deliberately **not persisted**: `events` is the single
/// authoritative record. A persisted cache would be a second authoritative
/// state that could silently diverge — exactly the dual-source hazard this
/// event log exists to avoid. On load the cache starts empty (`folded_len == 0`)
/// and the first read folds (or a catch-up folds the new suffix).
pub struct ProjectionCache {
    /// Running derived state for every builtin projection.
    registry: ProjectionRegistry,
    /// Number of leading events already folded into `registry`.
    folded_len: usize,
    /// `event_id` of the last folded event (`events[folded_len - 1]`), or `None`
    /// when nothing is folded. This is an O(1)-memory fingerprint of the folded
    /// prefix boundary: if `events[folded_len - 1]` no longer carries this id, the
    /// prefix was replaced in place behind the append seam (the `events` field is
    /// `pub`), so [`ensure_folded`](Self::ensure_folded) refolds instead of
    /// trusting a stale prefix. Catches a replacement/truncation whose boundary
    /// event differs — which a shrink-length check alone misses.
    folded_head_id: Option<crate::session::EventId>,
}

impl Default for ProjectionCache {
    /// A cache holding the builtin projection set with nothing folded yet.
    /// `Default` matches [`builtin`](Self::builtin) so every path that
    /// constructs a `SessionEventMap` (derive `Default`, deserialize, literal)
    /// gets a usable cache that folds correctly on first read.
    fn default() -> Self {
        Self {
            registry: ProjectionRegistry::builtin(),
            folded_len: 0,
            folded_head_id: None,
        }
    }
}

impl Clone for ProjectionCache {
    /// A clone is deliberately **empty** (nothing folded). The cache is pure
    /// derived state, so a cloned `Session`'s cache is recomputed lazily from
    /// its own `events` on first read — correct and O(1)-to-clone, instead of
    /// deep-copying the folded transcript (which would make every one of the
    /// many `Session::clone()` sites — fork, review, transfer, overnight, video
    /// export — pay an O(transcript) copy for data it usually never reads).
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl std::fmt::Debug for ProjectionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectionCache")
            .field("folded_len", &self.folded_len)
            .field("folded_head_id", &self.folded_head_id)
            .field("registry", &self.registry)
            .finish()
    }
}

impl ProjectionCache {
    /// The builtin projection set, nothing folded yet. Identical to
    /// [`default`](Self::default); named for call-site readability.
    pub fn builtin() -> Self {
        Self::default()
    }

    /// How many leading events are currently folded into the registry.
    pub fn folded_len(&self) -> usize {
        self.folded_len
    }

    /// Borrow the running registry (typed access via [`ProjectionRegistry::get`]).
    pub fn registry(&self) -> &ProjectionRegistry {
        &self.registry
    }

    /// The live transcript length from the cache, folding lazily first but
    /// **without cloning** the transcript. Consumers that only need the count
    /// (e.g. an index watermark) use this instead of
    /// [`projected_messages`](crate::session::SessionEventMap::projected_messages)
    /// so a length query does not pay for a full `Vec<StoredMessage>` clone.
    pub fn transcript_len(&mut self, events: &[SessionEvent]) -> usize {
        self.ensure_folded(events);
        self.registry
            .get::<LiveTranscriptProjection>()
            .expect("builtin projection registry must contain LiveTranscriptProjection")
            .len()
    }

    /// Fold every event up to `events.len()` into the registry, catching up only
    /// the unfolded suffix. Idempotent: calling it repeatedly with no appended
    /// events is a no-op.
    ///
    /// On first use (`folded_len == 0`) this is a full fold from the empty
    /// default. Afterwards it applies only `events[folded_len..]`, so a read
    /// that follows one append folds one event.
    ///
    /// Self-healing: if `events` shrank below the watermark (`folded_len >
    /// events.len()`) OR the folded prefix boundary no longer matches the event
    /// now at that position, the log was replaced/truncated in place behind the
    /// append seam, so the folded state no longer corresponds to `events`. Rather
    /// than read a stale prefix, this resets and refolds from scratch. Callers
    /// that replace the whole vector directly should prefer [`reset`](Self::reset);
    /// this is the defensive backstop for the case they forget.
    ///
    /// The boundary check is exact for any in-place replacement whose event at the
    /// fold boundary differs from what was folded (the normal case). A pathological
    /// replacement that reuses the *same* `event_id` at every boundary position with
    /// different content would still evade it (event ids are not content hashes);
    /// no in-crate path produces that, so the residue is prevented by convention
    /// rather than detected at runtime (route every mutation through
    /// [`append_event`](crate::session::SessionEventMap::append_event) or call
    /// [`reset`](Self::reset)).
    ///
    /// Postcondition: `folded_len == events.len()` and the registry's derived
    /// state equals a full fold of `events`.
    pub fn ensure_folded(&mut self, events: &[SessionEvent]) {
        // Detect an in-place mutation of the vector behind the append seam: either
        // the log shrank below the watermark, or the event now sitting at the fold
        // boundary is no longer the one we folded (its id changed). Both mean the
        // folded prefix is stale, so refold from empty.
        let prefix_stale = self.folded_len > events.len()
            || (self.folded_len > 0
                && self.folded_head_id.as_ref() != Some(&events[self.folded_len - 1].event_id));
        if prefix_stale {
            self.registry = ProjectionRegistry::builtin();
            self.folded_len = 0;
            self.folded_head_id = None;
        }
        // Fold only the unfolded suffix. When nothing is new the cache is already
        // at the head and this is the idempotent no-op read, so guard the whole
        // update: re-cloning the head `event_id` on every no-op read would make the
        // documented "clone-free" `transcript_len`/`projected_messages_len` path
        // allocate a `String` per call for no state change.
        if self.folded_len < events.len() {
            for event in &events[self.folded_len..] {
                self.registry.apply(event);
            }
            self.folded_len = events.len();
            self.folded_head_id = events.last().map(|e| e.event_id.clone());
        }
    }

    /// Apply one just-appended event incrementally and advance `folded_len`.
    ///
    /// Callers must only invoke this for the event being appended to the log tail,
    /// at the moment the cache is exactly at the current head
    /// (`folded_len == events.len()`, i.e. the new event is `events[folded_len]`).
    /// [`SessionEventMap::append_event`](crate::session::SessionEventMap::append_event)
    /// and `push_event` satisfy this by calling it *before* pushing the event,
    /// guarded on that equality. If the invariant does not hold (e.g. the cache is
    /// stale or behind), call [`ensure_folded`](Self::ensure_folded) instead.
    pub fn apply_event(&mut self, event: &SessionEvent) {
        self.registry.apply(event);
        self.folded_len += 1;
        self.folded_head_id = Some(event.event_id.clone());
    }

    /// Drop the cache's folded state and refold the whole log.
    ///
    /// The normal append seam keeps the cache current, and every wholesale
    /// replacement in this crate (`rebuild_event_map`, `fork_event_log`)
    /// constructs a *fresh* `SessionEventMap` whose cache is already empty — so
    /// today this is a defensive primitive rather than a hot-path call. It exists
    /// for callers that mutate `events` in place (the field is `pub`) and is the
    /// explicit counterpart to the self-healing branch in
    /// [`ensure_folded`](Self::ensure_folded).
    pub fn reset(&mut self, events: &[SessionEvent]) {
        self.registry = ProjectionRegistry::builtin();
        self.folded_len = 0;
        self.folded_head_id = None;
        self.ensure_folded(events);
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

/// The incremental **append-seam cache** must agree byte-for-byte with the
/// on-demand fold, *when the cache is current* (`folded_len == events.len()`).
///
/// [`ProjectionMatchesDerived`] re-folds from scratch via `project_map`, so it
/// proves the shared `apply` semantics but never reads the cache's running state
/// — the path `Session::projected_messages()` actually takes in production. This
/// check closes that gap: it compares the cache's *cached* transcript against
/// `derive_messages()`, catching a `folded_len` bookkeeping bug (e.g. a stale
/// prefix after an in-place log mutation) that a fresh fold would mask.
///
/// A cache that is merely behind (not yet caught up) is not a violation — it
/// legitimately refolds on the next read — so the check passes when the cache is
/// not at the head.
pub struct ProjectionCacheMatchesDerived;

impl LogInvariant for ProjectionCacheMatchesDerived {
    fn name(&self) -> &'static str {
        "session.projection_cache_matches_derived"
    }

    fn check(&self, map: &SessionEventMap) -> Result<(), InvariantViolation> {
        let Some(cached) = map.cached_transcript_at_head() else {
            // Cache is behind the log head (or empty); it will refold on the next
            // read, so there is nothing to compare yet.
            return Ok(());
        };
        let derived = map.derive_messages();
        // `StoredMessage` is deliberately not `PartialEq`; compare canonical
        // serialized forms (identical transcripts is the property that matters).
        let a = serde_json::to_vec(cached).map_err(|e| {
            InvariantViolation::new(
                "session.projection_cache_matches_derived",
                format!("cached transcript failed to serialize: {e}"),
            )
        })?;
        let b = serde_json::to_vec(&derived).map_err(|e| {
            InvariantViolation::new(
                "session.projection_cache_matches_derived",
                format!("derived transcript failed to serialize: {e}"),
            )
        })?;
        if a != b {
            return Err(InvariantViolation::new(
                "session.projection_cache_matches_derived",
                format!(
                    "incremental cache ({len} msgs) diverged from derive_messages ({len2} msgs) at log head",
                    len = cached.len(),
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
        r.add(ProjectionCacheMatchesDerived);
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
    fn cache_invariant_passes_at_head_and_skips_when_behind() {
        // The incremental-cache invariant must pass when the cache is current,
        // and must NOT flag a merely-lagging cache (it refolds lazily).
        //
        // Seed the log *without* the append seam so the cache is genuinely behind
        // (`folded_len == 0 < 2`) — going through `append` would keep it at head
        // and never exercise the skip branch.
        let mut map = SessionEventMap::default();
        for (i, id) in ["e1", "e2"].iter().enumerate() {
            map.events.push(SessionEvent {
                timestamp: chrono::Utc::now(),
                event_id: (*id).to_string().into(),
                op: SessionEventOp::AppendMessage {
                    message_id: (*id).to_string().into(),
                    message: text_msg(&format!("m{i}")),
                },
                parent_id: None,
                version: 1,
            });
        }
        assert_eq!(map.projection_folded_len(), 0);
        assert_eq!(map.events.len(), 2);

        let check = ProjectionCacheMatchesDerived;
        // Behind (folded_len == 0 < 2) → skipped (Ok), not a violation.
        assert!(check.check(&map).is_ok());

        // Advance the cache to the head via the real read path.
        let _ = map.projected_messages();
        assert_eq!(map.projection_folded_len(), map.events.len());
        assert!(
            check.check(&map).is_ok(),
            "cache invariant must pass when the cache is at the log head"
        );
    }

    /// The linchpin guard must actually *detect* divergence, not just pass. Drive
    /// the cache to the log head, then mutate an event's payload in place while
    /// keeping the length — and the fold-boundary id — identical, which is the
    /// exact residue the `folded_head_id` fingerprint cannot heal. `cached_*`
    /// still reports the *stale* transcript at the head, so the invariant must
    /// fail loudly. This proves the guard compares real content (a non-vacuous
    /// check), so a green result at the load seam means something.
    #[test]
    fn cache_invariant_flags_a_stale_prefix_at_the_head() {
        let mut map = SessionEventMap::default();
        append(&mut map, "e0", text_msg("m0"));
        append(&mut map, "e1", text_msg("m1"));
        let _ = map.projected_messages();
        assert_eq!(map.projection_folded_len(), map.events.len());

        let check = ProjectionCacheMatchesDerived;
        assert!(check.check(&map).is_ok(), "must be green before the in-place edit");

        // Swap the FIRST event's message but keep every event_id (so the boundary
        // id "e1" is unchanged): only the interior content differs.
        map.events[0] = SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "e0".into(),
            op: SessionEventOp::AppendMessage {
                message_id: "m0".into(),
                message: text_msg("m0-CHANGED"),
            },
            parent_id: None,
            version: 1,
        };
        // Length is unchanged, so `folded_len` still equals `events.len()` and the
        // guard compares (and must observe the divergence).
        assert_eq!(map.projection_folded_len(), map.events.len());
        assert!(
            check.check(&map).is_err(),
            "the cache invariant must flag a stale cached transcript at the head"
        );
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
        // The documented None contract: an unregistered projection yields None
        // (no silent default), so a consumer that requires it must expect/unwrap.
        assert!(empty.get::<LiveTranscriptProjection>().is_none());

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

    /// f1: the append-seam cache folds lazily and stays identical to a full fold
    /// of the log, catching up only the unfolded suffix.
    #[test]
    fn projection_cache_folds_lazily_and_catches_up() {
        let mut map = SessionEventMap::default();
        let mut cache = ProjectionCache::builtin();

        // Empty log: nothing folded, first read is a no-op fold.
        assert_eq!(cache.folded_len(), 0);
        cache.ensure_folded(&map.events);
        assert_eq!(cache.folded_len(), 0);

        // Append through the real seam, then a read catches up.
        for i in 0..4 {
            append(&mut map, &format!("e{i}"), text_msg(&format!("m{i}")));
            // The standalone cache is not wired to `map`, so it lags until read.
            cache.ensure_folded(&map.events);
            assert_eq!(cache.folded_len(), map.events.len());
        }

        // The cached transcript equals the full on-demand fold.
        let cached = cache
            .registry()
            .get::<LiveTranscriptProjection>()
            .expect("present")
            .clone();
        assert_eq!(ids(&cached), ids(&map.derive_messages()));
    }

    /// f1: at the append seam (`folded_len == events.len()`), a single new event
    /// is folded incrementally by `apply_event` and the length advances by one.
    #[test]
    fn projection_cache_applies_incrementally_at_head() {
        let mut map = SessionEventMap::default();
        let mut cache = ProjectionCache::builtin();
        append(&mut map, "e0", text_msg("a"));
        cache.ensure_folded(&map.events);
        assert_eq!(cache.folded_len(), 1);

        // Append one more; the map would call `apply_event` at this seam.
        let msg = text_msg("b");
        let event = SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "e1".to_string().into(),
            op: SessionEventOp::AppendMessage {
                message_id: msg.id.clone().into(),
                message: msg,
            },
            parent_id: None,
            version: 1,
        };
        assert_eq!(cache.folded_len(), map.events.len());
        cache.apply_event(&event);
        map.events.push(event);
        assert_eq!(cache.folded_len(), map.events.len());

        let cached = cache
            .registry()
            .get::<LiveTranscriptProjection>()
            .expect("present")
            .clone();
        assert_eq!(ids(&cached), ids(&map.derive_messages()));
    }

    /// f1: the append seam must NOT fold an event onto a cache that is *behind*
    /// the head (`folded_len != events.len()`) — doing so would advance the
    /// watermark past unfolded events and silently drop them. The guard skips the
    /// incremental apply, and the next read must fold the whole log and still
    /// agree with `derive_messages`.
    #[test]
    fn projection_cache_skips_incremental_apply_when_behind() {
        let mut map = SessionEventMap::default();
        // Put two events in the log WITHOUT going through the append seam, so the
        // cache stays folded_len == 0 while events.len() == 2 (a behind cache —
        // e.g. a freshly deserialized map whose cache is empty, then mutated).
        for (i, id) in ["e0", "e1"].iter().enumerate() {
            map.events.push(SessionEvent {
                timestamp: chrono::Utc::now(),
                event_id: (*id).to_string().into(),
                op: SessionEventOp::AppendMessage {
                    message_id: (*id).to_string().into(),
                    message: text_msg(&format!("m{i}")),
                },
                parent_id: None,
                version: 1,
            });
        }
        assert_eq!(map.projection_folded_len(), 0);
        assert_eq!(map.events.len(), 2);

        // Append a third through the real seam; the guard must skip the
        // incremental apply (cache is not at the head). Folding `e2` onto the
        // empty cache here would set folded_len = 1, so the next read would fold
        // only `events[1..]` — losing `e0` and duplicating `e2`. The guard keeps
        // folded_len at 0 so the next read refolds the whole log.
        append(&mut map, "e2", text_msg("c"));
        assert_eq!(
            map.projection_folded_len(),
            0,
            "append must not advance a behind cache's watermark"
        );

        // The next read folds the whole log and matches the derived transcript.
        let cached = map.projected_messages();
        assert_eq!(map.projection_folded_len(), map.events.len());
        assert_eq!(ids(&cached), ids(&map.derive_messages()));
    }

    /// f1: a wholesale log replacement (deserialize / rebuild / fork) produces a
    /// *fresh* `SessionEventMap` whose cache starts empty (`folded_len == 0`), so it
    /// can never shadow a different event vector — the next read folds the new log.
    #[test]
    fn projection_cache_starts_empty_after_replacement() {
        // The property under test: a fresh map's cache starts empty.
        let fresh = SessionEventMap::default();
        assert_eq!(fresh.projection_folded_len(), 0);
        assert!(fresh.events.is_empty());

        // Fold a map to the head, then replace it wholesale with a fresh map
        // carrying a *different* log (exactly what rebuild/fork do). The new map
        // starts empty and folds its own events — it cannot inherit the other
        // map's watermark and shadow the new log.
        let mut original = SessionEventMap::default();
        for i in 0..3 {
            append(&mut original, &format!("e{i}"), text_msg(&format!("m{i}")));
        }
        assert_eq!(original.projection_folded_len(), 3);

        let mut replaced = SessionEventMap::default();
        assert_eq!(replaced.projection_folded_len(), 0);
        append(&mut replaced, "only", text_msg("only"));
        assert_eq!(ids(&replaced.projected_messages()), vec!["only".to_string()]);
    }

    /// f1: `reset` drops the folded state and refolds the (new) log, so a caller
    /// that mutated `events` in place can rebuild the cache explicitly.
    #[test]
    fn projection_cache_reset_refolds_in_place_replacement() {
        let mut map = SessionEventMap::default();
        for i in 0..3 {
            append(&mut map, &format!("e{i}"), text_msg(&format!("m{i}")));
        }
        let mut cache = ProjectionCache::builtin();
        cache.ensure_folded(&map.events);
        assert_eq!(cache.folded_len(), 3);

        // In-place replacement with an equal-length log that REUSES the same
        // boundary event id ("e2") but changes the messages. The folded-prefix
        // fingerprint cannot catch this (it only compares the boundary event id),
        // so `ensure_folded` would trust the stale prefix — only an explicit
        // `reset` rebuilds it. This is exactly the residue `reset` exists for.
        let replace = |id: &str, msg_id: &str| SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: id.into(),
            op: SessionEventOp::AppendMessage {
                message_id: msg_id.into(),
                message: text_msg(msg_id),
            },
            parent_id: None,
            version: 1,
        };
        map.events = vec![
            replace("e0", "n0"),
            replace("e1", "n1"),
            replace("e2", "n2"),
        ];
        cache.reset(&map.events);
        assert_eq!(cache.folded_len(), 3);
        let cached = cache
            .registry()
            .get::<LiveTranscriptProjection>()
            .expect("present")
            .clone();
        assert_eq!(ids(&cached), ids(&map.derive_messages()));
        // Prove the reset really refolded to the NEW messages (not a no-op): ids
        // changed from m*/e* to n*, so a stale cache would still show the old ids.
        assert_eq!(ids(&cached), vec!["n0".to_string(), "n1".to_string(), "n2".to_string()]);
    }

    /// f1: the folded-prefix fingerprint makes `ensure_folded` self-heal even on an
    /// **equal-length** in-place replacement of `events` (which the shrink check
    /// alone cannot detect) — the caller need not remember to call `reset`.
    #[test]
    fn projection_cache_ensure_folded_self_heals_on_equal_length_replacement() {
        let mut map = SessionEventMap::default();
        for i in 0..3 {
            append(&mut map, &format!("e{i}"), text_msg(&format!("m{i}")));
        }
        let mut cache = ProjectionCache::builtin();
        cache.ensure_folded(&map.events);
        assert_eq!(cache.folded_len(), 3);

        // Replace all three events in place, keeping the same length. The boundary
        // event id changes ("e2" -> "n2"), so the fingerprint detects the swap.
        map.events = (0..3)
            .map(|i| SessionEvent {
                timestamp: chrono::Utc::now(),
                event_id: format!("n{i}").into(),
                op: SessionEventOp::AppendMessage {
                    message_id: format!("n{i}").into(),
                    message: text_msg(&format!("n{i}")),
                },
                parent_id: None,
                version: 1,
            })
            .collect();

        // No reset: the next fold must self-heal and return the NEW transcript.
        cache.ensure_folded(&map.events);
        assert_eq!(cache.folded_len(), 3);
        let healed = cache
            .registry()
            .get::<LiveTranscriptProjection>()
            .expect("present")
            .clone();
        assert_eq!(ids(&healed), ids(&map.derive_messages()));
    }

    /// f1: `ensure_folded` self-heals when the log shrinks behind the append seam
    /// (an in-place replacement the caller forgot to `reset`): instead of reading a
    /// stale prefix (or panicking), it refolds from empty.
    #[test]
    fn projection_cache_ensure_folded_self_heals_on_shrink() {
        let mut map = SessionEventMap::default();
        for i in 0..5 {
            append(&mut map, &format!("e{i}"), text_msg(&format!("m{i}")));
        }
        let mut cache = ProjectionCache::builtin();
        cache.ensure_folded(&map.events);
        assert_eq!(cache.folded_len(), 5);

        // Shrink the log in place without reset; the next fold must self-heal.
        map.events.truncate(2);
        assert_eq!(cache.transcript_len(&map.events), 2);
        assert_eq!(cache.folded_len(), map.events.len());
        let cached = cache
            .registry()
            .get::<LiveTranscriptProjection>()
            .expect("present")
            .clone();
        assert_eq!(ids(&cached), ids(&map.derive_messages()));
    }

    /// f1: `ensure_folded` is idempotent — calling it twice with no change must
    /// not double-apply events (which would duplicate the transcript).
    #[test]
    fn projection_cache_ensure_folded_is_idempotent() {
        let mut map = SessionEventMap::default();
        append(&mut map, "e0", text_msg("a"));
        append(&mut map, "e1", text_msg("b"));
        let mut cache = ProjectionCache::builtin();
        cache.ensure_folded(&map.events);
        cache.ensure_folded(&map.events);
        cache.ensure_folded(&map.events);
        let cached = cache
            .registry()
            .get::<LiveTranscriptProjection>()
            .expect("present")
            .clone();
        assert_eq!(ids(&cached), vec!["a".to_string(), "b".to_string()]);
    }

    /// f1: a cloned cache is deliberately *empty* (the cache is pure derived state,
    /// so it refolds lazily from the clone's own events). This keeps `Session::clone`
    /// — used widely by fork/review/transfer/overnight paths — from deep-copying the
    /// folded transcript, while still yielding the same derived state on first read.
    #[test]
    fn projection_cache_clone_is_empty_and_refolds() {
        let mut map = SessionEventMap::default();
        append(&mut map, "e0", text_msg("a"));
        append(&mut map, "e1", text_msg("b"));
        let mut cache = ProjectionCache::builtin();
        cache.ensure_folded(&map.events);
        assert_eq!(cache.folded_len(), 2);

        let mut cloned = cache.clone();
        // Empty on clone (no transcript deep-copy)...
        assert_eq!(cloned.folded_len(), 0);

        // ...but refolds to the same transcript on first read.
        cloned.ensure_folded(&map.events);
        assert_eq!(cloned.folded_len(), 2);
        let cached = cloned
            .registry()
            .get::<LiveTranscriptProjection>()
            .expect("present");
        assert_eq!(ids(cached), vec!["a".to_string(), "b".to_string()]);
    }

    /// f1: `transcript_len` folds lazily and returns the same count as the full
    /// fold, without cloning the transcript.
    #[test]
    fn projection_cache_transcript_len_matches_fold() {
        let mut map = SessionEventMap::default();
        let mut cache = ProjectionCache::builtin();
        assert_eq!(cache.transcript_len(&map.events), 0);
        for i in 0..5 {
            append(&mut map, &format!("e{i}"), text_msg(&format!("m{i}")));
        }
        assert_eq!(cache.transcript_len(&map.events), map.derive_messages().len());
        assert_eq!(cache.folded_len(), map.events.len());
    }

    fn ids(messages: &[StoredMessage]) -> Vec<String> {
        messages.iter().map(|m| m.id.clone()).collect()
    }
    /// f1: a raw tail append to the `pub` `events` field (bypassing the seam)
    /// after the cache is already at head is caught up by the next read: the
    /// folded prefix is still valid, so `ensure_folded` folds only the new suffix.
    /// This is the documented backstop for a caller that appends to `events`
    /// directly instead of through `append_event`.
    #[test]
    fn projection_cache_catches_up_after_raw_tail_push() {
        let mut map = SessionEventMap::default();
        for i in 0..3 {
            append(&mut map, &format!("e{i}"), text_msg(&format!("m{i}")));
        }
        // Fold to head so the cache is current before the raw mutation.
        let _ = map.projected_messages();
        assert_eq!(map.projection_folded_len(), 3);

        map.events.push(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: "raw".to_string().into(),
            op: SessionEventOp::AppendMessage {
                message_id: "mraw".to_string().into(),
                message: text_msg("mraw"),
            },
            parent_id: None,
            version: 1,
        });

        let cached = map.projected_messages();
        assert_eq!(cached.len(), 4, "the raw tail append must be folded");
        assert_eq!(ids(&cached), ids(&map.derive_messages()));
    }

}
