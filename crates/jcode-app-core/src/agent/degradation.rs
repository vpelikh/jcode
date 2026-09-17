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
//! The signal it consumes is **turn-content stalls** (`record_stall`):
//! stalled-promise filler turns, empty turns, unfulfilled tool requests. These
//! indicate *model* degradation on a long context and can be mitigated by
//! re-prompting, compacting, or switching the route. (A separate system-level
//! "load" axis from `watchdog.stall` is a documented future extension; it is
//! not represented here yet to avoid carrying dead state.)

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
    Escalated,
}

impl Rung {
    /// Whether this rung still recommends autonomous mitigation (as opposed to
    /// surfacing to the user).
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
    #[allow(dead_code, reason = "constructed by future empty-turn wiring; test-only for now")]
    EmptyTurn,
    /// The model said it would invoke a tool but did not.
    #[allow(dead_code, reason = "constructed by future unfulfilled-tool wiring; test-only for now")]
    UnfulfilledToolRequest,
}

impl StallKind {
    #[allow(dead_code, reason = "used only by describe() for diagnostics")]
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
    #[allow(dead_code, reason = "reporting only; later kinds will populate it")]
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
}

impl Default for DegradationConfig {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(300),    // 5 minutes
            watch_threshold: 1,
            compact_threshold: 2,
            fallback_threshold: 3,
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
    /// Currently recommended mitigation rung. Recomputes from the live stall
    /// count; decays on recovery and is terminal at `Escalated` (only
    /// [`Self::reset`] clears that).
    rung: Rung,
    /// Recommended compact is one-shot: record the fact we already recommended
    /// it so we do not re-fire on every turn while the session is mid-compact.
    compact_recommended: bool,
}

#[allow(
    dead_code,
    reason = "diagnostic/read API (route, stall_count, describe) exercised by unit tests and used by callers for logging; kept as the stable tracker surface"
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

    /// Record a turn-content stall of the given kind.
    pub fn record_stall(&mut self, kind: StallKind) {
        self.prune_expired();
        self.stalls.push_back(StallRecord {
            kind,
            at: Instant::now(),
        });
        self.recompute_rung();
    }

    /// Record a clean, healthy turn. Prunes stale stall-window entries and
    /// recomputes the rung from the live count, so a genuine recovery decays the
    /// rung back toward Healthy instead of pinning it at a stale escalated level.
    /// `Escalated` remains terminal (only [`Self::reset`] clears it).
    pub fn record_healthy_turn(&mut self) {
        self.prune_expired();
        // Recompute from the live (pruned) count so a clean turn after a
        // recovery can decay the rung instead of pinning it at an old level.
        self.recompute_rung();
    }

    /// Reset the escalation cycle: clear stall records and drop back to Healthy.
    /// Called after a successful mitigation (e.g. a compaction that restored
    /// progress) or an explicit user reset.
    pub fn reset(&mut self) {
        self.stalls.clear();
        self.rung = Rung::Healthy;
        self.compact_recommended = false;
    }

    /// Reset the escalation cycle AND re-key the tracker to a new route. Used
    /// when the provider/model route changes, so the tracker both stops
    /// carrying the prior route's stall history and reports the correct route in
    /// diagnostics thereafter.
    pub fn reset_for_route(&mut self, route: RouteKey) {
        self.route = route;
        self.reset();
    }

    /// Escalate to the terminal `Escalated` rung: auto-mitigation is exhausted
    /// or disabled, so the caller should surface the situation to the user
    /// rather than keep acting. Idempotent.
    pub fn escalate(&mut self) {
        self.rung = Rung::Escalated;
    }

    /// Promptly promote to `RouteFallback`, skipping the intermediate rungs.
    /// Used when an earlier rung is genuinely unavailable (e.g. the provider
    /// does not support compaction), so the session still reaches the
    /// fallback/escalation decision instead of silently spinning.
    pub fn promote_to_route_fallback(&mut self) {
        // Treat a compaction as "attempted" so the RouteFallback gate opens.
        self.compact_recommended = true;
        self.rung = Rung::RouteFallback;
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
    }

    fn recompute_rung(&mut self) {
        // `Escalated` is terminal: once we surface to the user, only an
        // explicit reset() clears it. Do not let a healthy stretch silently
        // un-escalate.
        if self.rung == Rung::Escalated {
            return;
        }
        let count = self.stalls.len() as u32;
        // RouteFallback is deliberately gated on a compaction having been
        // attempted and acknowledged. A count alone reaching the fallback
        // threshold is NOT enough to justify switching the user's model: we
        // first compact (which mostly resolves long-context degradation) and
        // only escalate past it if the window STILL has enough stalls.
        let next = if count >= self.cfg.fallback_threshold && self.compact_recommended {
            Rung::RouteFallback
        } else if count >= self.cfg.compact_threshold {
            Rung::Compact
        } else if count >= self.cfg.watch_threshold {
            Rung::Watch
        } else {
            Rung::Healthy
        };
        // Rederive from the live count so a genuinely clean stretch (e.g. after
        // a successful compaction prunes the stall window) decays the rung back
        // toward Healthy instead of pinning it at a previously-escalated level.
        self.rung = next;
        if self.rung < Rung::Compact {
            self.compact_recommended = false;
        }
    }

    /// Human-readable snapshot for logging/UI.
    pub fn describe(&self) -> String {
        let stall_types = self.stalls.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>();
        format!(
            "route={} rung={:?} stall_count={} kinds=[{}]",
            self.route,
            self.rung,
            self.stalls.len(),
            stall_types.join(",")
        )
    }
}

/// Reactive mitigation integrated into the `Agent` turn loop.
impl Agent {
    /// The degradation route key for the agent's CURRENT provider/model. Kept in
    /// one place so `build_base` and the route/model-switch re-keying agree.
    pub(crate) fn current_route_key(&self) -> RouteKey {
        RouteKey(format!("{}/{}", self.provider.display_name(), self.provider_model()))
    }

    /// Advance the degradation mitigation ladder at a safe turn-loop checkpoint.
    ///
    /// Called at the turn-loop head (before the next provider request). Returns
    /// a user-visible message when we acted on a mitigation (e.g. triggered a
    /// compaction or a gated route fallback); `None` when nothing needed doing.
    ///
    /// Rungs handled:
    /// - `Compact` (pending): request a manual compaction through the existing
    ///   `request_manual_compaction` mechanism and acknowledge it (one-shot).
    /// - `RouteFallback`: switch to the configured fallback model when
    ///   `degradation.route_fallback_enabled` is true and a `fallback_model` is
    ///   set; otherwise `escalate()` (surface to the user) and report that
    ///   auto-mitigation is exhausted.
    pub(crate) fn maybe_mitigate_degradation(&mut self) -> Option<String> {
        let rung = self.degradation.rung();
        match rung {
            Rung::Compact if self.degradation.compact_pending() => {
                // If the provider cannot compact, this rung cannot help: promptly
                // skip to the fallback/escalation decision rather than spinning
                // on a compaction that will always fail.
                if !self.provider.supports_compaction() {
                    crate::logging::warn(&format!(
                        "Model degradation: {} reached Compact rung but provider does not support compaction; escalating to route fallback",
                        self.degradation.describe()
                    ));
                    self.degradation.promote_to_route_fallback();
                    return self.maybe_fallback_route();
                }
                crate::logging::warn(&format!(
                    "Model degradation: {} reached Compact rung; triggering compaction",
                    self.degradation.describe()
                ));
                let (message, success) = self.request_manual_compaction();
                // Mark compaction as attempted regardless of outcome: this is
                // what un-gates the RouteFallback escalation, whose whole point
                // is "compaction did not (or could not) restore progress". One
                // attempt per escalation cycle so we do not re-fire every turn.
                self.degradation.acknowledge_compact();
                if success {
                    Some(format!(
                        "Model degradation detected; {}",
                        message.lines().last().unwrap_or("context compaction started")
                    ))
                } else {
                    // Compaction could not run now (busy / nothing to compact).
                    // We still recorded the attempt so the fallback rung can
                    // escalate; log and carry on this turn.
                    crate::logging::warn(&format!(
                        "Deferred degradation compaction (unsupported or busy): {message}"
                    ));
                    None
                }
            }
            Rung::RouteFallback => self.maybe_fallback_route(),
            Rung::Compact
            | Rung::Watch
            | Rung::Healthy
            | Rung::Escalated => None,
        }
    }

    /// Handle the `RouteFallback` rung: switch to the gated fallback model or,
    /// when fallback is disabled/unconfigured, escalate to surface to the user.
    fn maybe_fallback_route(&mut self) -> Option<String> {
        let settings = crate::config::config().degradation.clone();
        let current = self.provider_model();
        if !settings.route_fallback_enabled {
            crate::logging::warn(&format!(
                "Model degradation: {} reached RouteFallback rung but route fallback is disabled; escalating",
                self.degradation.describe()
            ));
            self.degradation.escalate();
            return Some(format!(
                "Model {} is degrading after compaction; route auto-fallback is disabled. Switch models manually to continue.",
                current
            ));
        }
        let Some(fallback) = settings.fallback_model.clone() else {
            crate::logging::warn(
                "Model degradation: route fallback enabled but no fallback_model set; escalating",
            );
            self.degradation.escalate();
            return Some(format!(
                "Model {} is degrading; route fallback enabled but no fallback_model configured.",
                current
            ));
        };
        if fallback.trim().is_empty() || fallback == current {
            crate::logging::warn(
                "Model degradation: fallback_model empty or equals the current model; escalating",
            );
            self.degradation.escalate();
            return Some(format!(
                "Model {} is degrading; configured fallback_model is invalid.",
                current
            ));
        }
        crate::logging::warn(&format!(
            "Model degradation: {} reached RouteFallback; switching {} -> {}",
            self.degradation.describe(),
            current,
            fallback
        ));
        // Use the auth (automatic) selection source, NOT the user source: this
        // is an automated mitigation, not a deliberate user choice. Marking it
        // User would bump selection_generation and make provider-control auth
        // reconciliation treat the auto-switch as a sticky user preference.
        match self.set_model_from_auth(&fallback) {
            Ok(()) => {
                // set_model_from_auth already resets the degradation cycle since
                // the route changed to the fallback; nothing else to do here.
                Some(format!(
                    "Model degradation detected; switched route from {} to {}",
                    current, fallback
                ))
            }
            Err(e) => {
                crate::logging::warn(&format!(
                    "Model degradation: failed to switch to fallback model {fallback}: {e}; escalating"
                ));
                self.degradation.escalate();
                Some(format!(
                    "Model {} is degrading and the fallback switch to {} failed: {e}",
                    current, fallback
                ))
            }
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
        // RouteFallback is gated on a compaction having been attempted: a 3rd
        // stall alone must not escalate to a model switch.
        t.record_stall(StallKind::StalledPromise);
        assert_eq!(
            t.rung(),
            Rung::Compact,
            "fallback must not fire without an acknowledged compaction"
        );
        // After acknowledging the compaction, a persistent stall escalates.
        t.acknowledge_compact();
        t.record_stall(StallKind::StalledPromise);
        assert_eq!(t.rung(), Rung::RouteFallback);
    }

    #[test]
    fn rung_decays_after_a_clean_stretch_and_reset_clears() {
        let cfg = DegradationConfig {
            window: Duration::from_secs(60),
            watch_threshold: 1,
            compact_threshold: 2,
            fallback_threshold: 3,
        };
        let mut t = DegradationTracker::with_config(RouteKey("p/m".into()), cfg);
        // Escalate to Compact and acknowledge the compaction.
        t.record_stall(StallKind::StalledPromise);
        t.record_stall(StallKind::StalledPromise);
        assert_eq!(t.rung(), Rung::Compact);
        t.acknowledge_compact();
        // A clean stretch after the stall window has expired (simulate pruning
        // by clearing the records, which is what would follow a recovery) lets
        // the rung decay back to Healthy instead of pinning at a stale level.
        t.stalls.clear();
        t.record_healthy_turn();
        assert_eq!(t.stalls.len(), 0);
        assert_eq!(t.rung(), Rung::Healthy, "a clean stretch should decay the rung");
        assert_eq!(t.stall_count(), 0);
        // Escalated is terminal: only reset() clears it.
        t.record_stall(StallKind::StalledPromise);
        t.record_stall(StallKind::StalledPromise);
        t.record_stall(StallKind::StalledPromise);
        t.escalate();
        assert_eq!(t.rung(), Rung::Escalated);
        t.record_healthy_turn();
        assert_eq!(t.rung(), Rung::Escalated, "Escalated is terminal");
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
    fn reset_for_route_rekeys_and_resets() {
        let mut t = DegradationTracker::new(RouteKey("p/old".into()));
        t.record_stall(StallKind::StalledPromise);
        t.record_stall(StallKind::StalledPromise);
        t.escalate();
        assert_eq!(t.rung(), Rung::Escalated);

        // Re-key to a new route clears both the stall history and the terminal
        // rung, and reports the new route in diagnostics.
        t.reset_for_route(RouteKey("p/new".into()));
        assert_eq!(t.route().0, "p/new");
        assert_eq!(t.rung(), Rung::Healthy);
        assert_eq!(t.stall_count(), 0);
        assert!(t.describe().contains("route=p/new"));
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