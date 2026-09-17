//! Degradation tracker for a session's provider/model route.
//!
//! This is the decision engine behind the model-degradation management plan. It
//! observes stall-family events on a session and recommends an escalating
//! mitigation rung when the session stops making forward progress.
//!
//! It is intentionally a *pure* module: it holds no global or static state, does
//! not persist by itself, and does not reach into `Agent` or `Session`. Callers
//! (the agent turn loop, provider control) feed it `&mut` with discrete events
//! and read back a [`Rung`] recommendation. Persistence and the reactive side
//! effects are layered on by the caller, which keeps this logic deterministic
//! and unit-testable in isolation (see `tests` below).
//!
//! The two signals it consumes are:
//! - **Turn-content stalls** (`record_stall`): stalled-promise filler turns,
//!   empty turns, unfulfilled tool requests. These indicate *model* degradation
//!   on a long context and can be mitigated by re-prompting, compacting, or
//!   switching the route.
//! - **System-level stalls** (`record_system_stall`): `watchdog.stall` events,
//!   which indicate CPU/memory oversubscription, NOT model degradation. A system
//!   stall is recorded but does not by itself promote the route rung; it is the
//!   separate "load" axis named in the plan. (Escalation for the load axis is a
//!   future slice; here we only track it for observability.)

use super::*;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A human-stable name for the provider/model this tracker keys on. Formed by
/// the caller from `provider_name()` and `provider_model()`; kept in one string
/// so the tracker has no dependency on the provider layer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RouteKey(pub String);

impl std::fmt::Display for RouteKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The escalating mitigation rung the tracker currently recommends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rung {
    /// No degradation observed; no mitigation.
    Healthy,
    /// Re-prompts (existing per-turn continuation) are active but should be
    /// watched. The tracker is still within the healthy window.
    Watch,
    /// Recommend a forced compaction: the session is producing stall-family
    /// turns at a rate that suggests long-context degradation.
    Compact,
    /// Recommend a route fallback: compaction also failed to restore progress.
    RouteFallback,
    /// Escalated to the user / stopped; the tracker will not auto-mitigate
    /// further. Reached only after the higher rungs were tried and failed.
    #[allow(dead_code, reason = "reached in the route-fallback slice (Slice 4)")]
    Escalated,
}

impl Rung {
    /// Whether this rung still recommends autonomous mitigation (as opposed to
    /// surfacing to the user).
    #[allow(dead_code, reason = "used by the route-fallback slice (Slice 4)")]
    pub fn is_autonomous(self) -> bool {
        matches!(self, Self::Watch | Self::Compact | Self::RouteFallback)
    }
}

/// Why a turn was classified as a stall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallKind {
    /// Dense "let me …" filler with no tool call (`is_stalled_promise_text`).
    StalledPromise,
    /// A turn ended with no tool call and no useful content.
    #[allow(dead_code, reason = "wired from the empty-turn path in Slice 3")]
    EmptyTurn,
    /// The model said it would invoke a tool but did not.
    #[allow(dead_code, reason = "wired from the unfulfilled-tool path in Slice 3")]
    UnfulfilledToolRequest,
}

impl StallKind {
    #[allow(dead_code, reason = "used by describe() logging; kept for later slices")]
    fn as_str(self) -> &'static str {
        match self {
            Self::StalledPromise => "stalled_promise",
            Self::EmptyTurn => "empty_turn",
            Self::UnfulfilledToolRequest => "unfulfilled_tool_request",
        }
    }
}

/// Observational record for a single stall event.
#[derive(Debug, Clone)]
struct StallRecord {
    #[allow(dead_code, reason = "reporting only; more kinds land in Slice 3")]
    kind: StallKind,
    at: Instant,
}

/// Tunables for the tracker. Defaults are reasonable for a long-running
/// developer session; tests vary these to avoid wall-clock waits.
#[derive(Debug, Clone)]
pub struct DegradationConfig {
    /// Stall-family turn events within this window count toward promotion.
    pub window: Duration,
    /// Number of stall events within [`Self::window`] to reach `Watch`.
    pub watch_threshold: u32,
    /// Number of stall events within [`Self::window`] to reach `Compact`.
    pub compact_threshold: u32,
    /// Number of stall events within [`Self::window`] to reach `RouteFallback`.
    pub fallback_threshold: u32,
    /// Load concern surfaced by watchdog.stall events. Does not promote the
    /// route rung but is tracked for observability and the load axis.
    #[allow(dead_code, reason = "load-axis alarm lands with Slice 5 observability")]
    pub system_stall_load_concern_threshold: u32,
}

impl Default for DegradationConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(300),    // 5 minutes
            watch_threshold: 1,
            compact_threshold: 2,
            fallback_threshold: 3,
            system_stall_load_concern_threshold: 2,
        }
    }
}

/// Per-route degradation state.
#[derive(Debug)]
pub struct DegradationTracker {
    route: RouteKey,
    cfg: DegradationConfig,
    /// Most recent stall events within the window, oldest first.
    stalls: VecDeque<StallRecord>,
    /// Rung reached in the current escalation cycle. Rises monotonically until
    /// [`Self::reset`] (e.g. after a successful mitigation) or expiry.
    rung: Rung,
    /// Recommended compact is one-shot: record the fact we already recommended
    /// it so we do not re-fire on every turn while the session is mid-compact.
    compact_recommended: bool,
    /// Load-axis count (watchdog.stall) within the window.
    system_stalls: VecDeque<Instant>,
}

#[allow(
    dead_code,
    reason = "read-side mitigation/observability API consumed by the compaction, route-fallback, and observability slices; already covered by unit tests"
)]
impl DegradationTracker {
    /// Start a tracker for the given route with the default configuration.
    pub fn new(route: RouteKey) -> Self {
        Self::with_config(route, DegradationConfig::default())
    }

    /// Start a tracker for the given route with a custom configuration (used by
    /// tests and by callers that override thresholds).
    pub fn with_config(route: RouteKey, cfg: DegradationConfig) -> Self {
        Self {
            route,
            cfg,
            stalls: VecDeque::new(),
            rung: Rung::Healthy,
            compact_recommended: false,
            system_stalls: VecDeque::new(),
        }
    }

    /// The route this tracker observes.
    pub fn route(&self) -> &RouteKey {
        &self.route
    }

    /// The currently recommended mitigation rung.
    pub fn rung(&self) -> Rung {
        self.rung
    }

    /// Whether the tracker recommends an autonomous mitigation action.
    pub fn recommends_mitigation(&self) -> bool {
        self.rung.is_autonomous()
    }

    /// Whether a `Compact` recommendation is pending and not yet acknowledged.
    pub fn compact_pending(&self) -> bool {
        self.rung == Rung::Compact && !self.compact_recommended
    }

    /// Mark that a compaction was recommended (or performed) so it does not
    /// re-fire. Call after acting on a [`Rung::Compact`] recommendation.
    pub fn acknowledge_compact(&mut self) {
        self.compact_recommended = true;
    }

    /// The loaded count of stall-family events in the current window.
    pub fn stall_count(&self) -> u32 {
        self.stalls.len() as u32
    }

    /// The loaded count of system-level (watchdog) stalls in the window.
    pub fn system_stall_count(&self) -> u32 {
        self.system_stalls.len() as u32
    }

    /// Record a turn-content stall of the given kind.
    pub fn record_stall(&mut self, kind: StallKind) {
        self.prune_expired();
        self.stalls.push_back(StallRecord {
            kind,
            at: Instant::now(),
        });
        self.recompute_rung();
    }

    /// Record a system-level (watchdog) stall. Does not move the route rung up,
    /// but is tracked for load observability.
    pub fn record_system_stall(&mut self) {
        self.prune_expired();
        self.system_stalls.push_back(Instant::now());
    }

    /// Record a clean, healthy turn. Pushes back the stall window so a flurry of
    /// stalls long ago stops counting; does not reset an already-escalated rung.
    pub fn record_healthy_turn(&mut self) {
        self.prune_expired();
    }

    /// Reset the escalation cycle: clear stall records and drop back to Healthy.
    /// Called after a successful mitigation (e.g. a compaction that restored
    /// progress) or an explicit user reset.
    pub fn reset(&mut self) {
        self.stalls.clear();
        self.system_stalls.clear();
        self.rung = Rung::Healthy;
        self.compact_recommended = false;
    }

    fn prune_expired(&mut self) {
        let cutoff = Instant::now() - self.cfg.window;
        while self
            .stalls
            .front()
            .is_some_and(|rec| rec.at < cutoff)
        {
            self.stalls.pop_front();
        }
        while self
            .system_stalls
            .front()
            .is_some_and(|at| *at < cutoff)
        {
            self.system_stalls.pop_front();
        }
    }

    fn recompute_rung(&mut self) {
        let count = self.stalls.len() as u32;
        // Recompute from the raw count so a rung can only rise, never drop,
        // mid-cycle (matches the monotonically-escalating model).
        let next = if count >= self.cfg.fallback_threshold {
            Rung::RouteFallback
        } else if count >= self.cfg.compact_threshold {
            Rung::Compact
        } else if count >= self.cfg.watch_threshold {
            Rung::Watch
        } else {
            Rung::Healthy
        };
        self.rung = next.max(self.rung);
        if self.rung < Rung::Compact {
            self.compact_recommended = false;
        }
    }

    /// Human-readable snapshot for logging/UI.
    pub fn describe(&self) -> String {
        let stall_types = self.stalls.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>();
        format!(
            "route={} rung={:?} stall_count={} system_stalls={} kinds=[{}]",
            self.route,
            self.rung,
            self.stalls.len(),
            self.system_stalls.len(),
            stall_types.join(",")
        )
    }
}

/// Reactive mitigation integrated into the `Agent` turn loop.
impl Agent {
    /// Advance the degradation mitigation ladder at a safe turn-loop checkpoint.
    ///
    /// Called at the turn-loop head (before the next provider request). Returns
    /// a user-visible message when we acted on a mitigation (e.g. triggered a
    /// compaction); `None` when nothing needed doing.
    ///
    /// Currently handles the `Compact` rung: when the tracker reports a pending
    /// compaction, request a manual compaction through the existing
    /// `request_manual_compaction` mechanism, acknowledge it (one-shot), and
    /// report success/failure via the returned message. Route-fallback
    /// (`Rung::RouteFallback`) and the observability rungs land in later slices.
    pub(crate) fn maybe_mitigate_degradation(&mut self) -> Option<String> {
        let rung = self.degradation.rung();
        match rung {
            Rung::Compact if self.degradation.compact_pending() => {
                crate::logging::warn(&format!(
                    "Model degradation: {} reached Compact rung; triggering compaction",
                    self.degradation.describe()
                ));
                let (message, success) = self.request_manual_compaction();
                self.degradation.acknowledge_compact();
                if success {
                    Some(format!(
                        "Model degradation detected; {}",
                        message.lines().last().unwrap_or("context compaction started")
                    ))
                } else {
                    // Compaction unavailable right now; log and carry on rather
                    // than blocking the turn. The rung stays Compact so a later
                    // checkpoint can retry once progress resumes.
                    crate::logging::warn(&format!(
                        "Deferred degradation compaction (unsupported or busy): {message}"
                    ));
                    None
                }
            }
            Rung::Compact
            | Rung::Watch
            | Rung::Healthy
            | Rung::RouteFallback
            | Rung::Escalated => None,
        }
    }

    /// Record the current turn's healthy/clean completion into the degradation
    /// tracker, so a clean stretch prunes stale stall-window entries. Call on
    /// the loop's clean, tool-call-producing exit.
    pub(crate) fn record_clean_turn(&mut self) {
        self.degradation.record_healthy_turn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_by_default_with_no_stalls() {
        let t = DegradationTracker::new(RouteKey("deepseek/deepseek-v4-flash".into()));
        assert_eq!(t.rung(), Rung::Healthy);
        assert_eq!(t.stall_count(), 0);
        assert!(!t.recommends_mitigation());
    }

    #[test]
    fn stalls_promote_through_watch_compact_fallback() {
        let mut t = DegradationTracker::new(RouteKey("p/m".into()));
        t.record_stall(StallKind::StalledPromise);
        assert_eq!(t.rung(), Rung::Watch);
        t.record_stall(StallKind::StalledPromise);
        assert_eq!(t.rung(), Rung::Compact);
        assert!(t.compact_pending());
        t.record_stall(StallKind::StalledPromise);
        assert_eq!(t.rung(), Rung::RouteFallback);
    }

    #[test]
    fn rung_is_monotonic_until_reset() {
        let mut t = DegradationTracker::with_config(
            RouteKey("p/m".into()),
            DegradationConfig {
                window: Duration::from_secs(60),
                watch_threshold: 2,
                compact_threshold: 3,
                fallback_threshold: 4,
                system_stall_load_concern_threshold: 2,
            },
        );
        for _ in 0..4 {
            t.record_stall(StallKind::EmptyTurn);
        }
        assert_eq!(t.rung(), Rung::RouteFallback);
        // Health events do not drop a reached rung.
        t.record_healthy_turn();
        assert_eq!(t.rung(), Rung::RouteFallback);
        // Reset clears it.
        t.reset();
        assert_eq!(t.rung(), Rung::Healthy);
        assert_eq!(t.stall_count(), 0);
    }

    #[test]
    fn compact_pending_is_one_shot() {
        let mut t = DegradationTracker::new(RouteKey("p/m".into()));
        // Default config promotes to Compact after 2 stalls.
        t.record_stall(StallKind::UnfulfilledToolRequest);
        t.record_stall(StallKind::UnfulfilledToolRequest);
        assert_eq!(t.rung(), Rung::Compact);
        assert!(t.compact_pending());
        t.acknowledge_compact();
        assert!(!t.compact_pending());
        // The pending flag is consumed once, even if healthy turns follow.
        t.record_healthy_turn();
        assert!(!t.compact_pending());
    }

    #[test]
    fn system_stall_does_not_promote_route_rung() {
        let cfg = DegradationConfig {
            window: Duration::from_secs(60),
            watch_threshold: 2,
            compact_threshold: 3,
            fallback_threshold: 4,
            system_stall_load_concern_threshold: 2,
        };
        let mut t = DegradationTracker::with_config(RouteKey("p/m".into()), cfg);
        t.record_system_stall();
        t.record_system_stall();
        assert_eq!(t.system_stall_count(), 2);
        // Route rung stays Healthy: a watchdog stall is load, not degradation.
        assert_eq!(t.rung(), Rung::Healthy);
        assert!(!t.recommends_mitigation());
    }

    #[test]
    fn describe_lists_stall_kinds() {
        let mut t = DegradationTracker::new(RouteKey("p/m".into()));
        t.record_stall(StallKind::StalledPromise);
        t.record_stall(StallKind::EmptyTurn);
        let d = t.describe();
        assert!(d.contains("route=p/m"));
        assert!(d.contains("stall_count=2"));
        assert!(d.contains("stalled_promise"));
        assert!(d.contains("empty_turn"));
    }
}