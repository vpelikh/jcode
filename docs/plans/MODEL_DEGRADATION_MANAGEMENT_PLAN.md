# Model Degradation Management Plan

## Status

**In progress.** Slices 1-4 landed on `feat/model-degradation-management`. Slice 1
(watchdog subscription surface), Slice 2 (degradation tracker), and Slice 3
(auto-compaction at the `Compact` rung) are complete. Slice 4 adds the gated
route-fallback rung: a `[degradation]` config section (off by default) that, when
enabled with a `fallback_model`, switches a persistently-degrading session onto
the fallback; otherwise the session escalates to `Escalated` and surfaces the
situation instead of changing the user's model. The plan proposes turning an
existing but passive set of detection signals into an active
degradation-management system.

**Known limitations of the landed Slices 1-4 (not yet closed):**
- The tracker is not persisted; state resets on disconnect/reload.
- `watchdog::subscribe()` (Slice 1) has no production consumer yet; the system
  "load" axis is documented as a future extension rather than wired state.
- The route-fallback action auto-switches without an interactive per-turn
  confirmation. The plan's Slice-4 exit criteria called for confirming a switch
  when the current route was explicitly user-chosen; the landed implementation
  instead relies on the strict opt-in gate (`route_fallback_enabled` must be
  true), which is treated as the user's standing authorization. Confirmation is
  not implemented.

**Closed on `feat/degradation-tracker-wiring` (2026-09):**
- `record_stall(StalledPromise)` used to fire only *after* a single turn
  exhausted its continuation budget
  (`MAX_STALLED_PROMISE_CONTINUATION_ATTEMPTS`). A session that emitted
  valid interleaved continuations between filler hits — the exact DeepSeek
  long-context pattern — never exhausted a per-turn budget, so the route
  tracker stayed `Healthy` and no mitigation ever fired. It now records one
  stall at the first stalled-promise detection of each turn (the per-turn
  continuation counter resets to 0 at the top of every turn), so such sessions
  accumulate toward `Watch`/`Compact`/`RouteFallback`. The count is per-turn,
  not per-distinct-filler-blip-within-a-turn: two filler episodes separated by
  a tool call in the same turn undercount to one, which errs toward
  under-detection (safe); `record_clean_turn` on real tool progress decays the
  rung on legitimate recovery.
- The streaming turn loop now records a clean turn (`record_clean_turn`) after
  committing tool results, so a genuine tool-progress recovery decays the rung
  instead of pinning a stale escalation. This is intentionally STRICTER than
  the non-streaming loop, which records a clean turn on ANY "no tool calls,
  returning" completion (including turns that just exhausted the stalled-
  promise budget). The streaming placement avoids labeling a degraded turn as
  clean. The asymmetry is safe because `record_clean_turn` only decays
  stall-window records older than the 5-minute window (`prune_expired`), so a
  just-recorded stall is never erased — confirmed end-to-end by
  `degraded_session_accumulates_to_compact_in_non_streaming_loop`.
- A compaction that is actually APPLIED (not just requested) now resets the
  route-scoped degradation tracker (`note_compaction_applied` -> `reset()`).
  Previously a successful compaction left the rung elevated with
  `compact_recommended` set, so the next minor stall (which per-turn first-
  detection recording surfaces quickly) immediately escalated to
  `RouteFallback` and switched the user's model right after a compaction that
  already restored the context. Regression-tested by
  `successful_compaction_resets_degradation_rung` (mutation-validated), and
  re-arms detection afterwards (a later re-degradation re-reaches Compact). The
  reset is guarded to fire from the `Compact` rung and from a pre-switch
  `RouteFallback` rung (per the plan's L3 -- recovered --> Idle), so an applied
  compaction restores Idle rather than continuing toward a stale model switch. A
  `Watch` session (single stall, below mitigation threshold) keeps its
  accumulation so an unrelated routine compaction cannot delay detecting a slow-
  degrading session (`compaction_apply_preserves_watch_level_accumulation`,
  mutation-validated); and the terminal `Escalated` rung is preserved so an
  unrelated compaction cannot dismiss a user-surfaced "switch models yourself"
  escalation (`compaction_apply_does_not_clear_terminal_escalated`,
  mutation-validated).
- An end-to-end test (`degraded_streaming_session_reaches_compact_and_mitigation_
  fires`) runs two degraded turns through the public streaming loop and asserts
  the loop's own mitigation checkpoint consumes the Compact one-shot — the
  exact durable chain that this wiring, plus `record_stall` on first detection,
  re-establishes.

**Integration/landing note (2026-09):** the branch merges cleanly into current
`master` with one trivial additive conflict in `crates/jcode-base/src/config.rs`
(both this feature's `DegradationSettings` and master's `LoopGuardConfig` are
added to the same `pub use jcode_config_types` list and the `Config` struct). The
resolution keeps both entries; it was validated in a throwaway merge that builds
`jcode-base` and `jcode-app-core` cleanly. No other files conflict.

> **Trigger for this plan (2026-09):** a long-running session with a live
> language model degraded into a classic stalled-promise loop. The session
> stayed `Active`
> but made no forward progress — its last assistant message was several
> "Let me add the import…" filler turns with no tool call, and it left an
> in-flight, non-compiling edit behind. Nothing in the product
> noticed or reacted: the log recorded the stall, but no subscriber acted on
> it, and the session kept "continuing" without escalating or changing course.

## Purpose

Detect when a model session starts degrading (stalling, filler-looping, or
freezing), then automatically escalate through an escalating mitigation ladder —
re-prompt, compact, then route-fallback — instead of silently spinning and
leaving broken intermediate state behind.

At every rung the goal is the same: **stop the degradation before it wastes
tokens, wall-clock time, and the user's attention**, with a clear audit trail
and no destructive auto-actions taken without a user-visible notice.

## What already exists (building blocks — no new detection needed for the core signals)

The detection primitives below are already implemented, validated, and covered by
tests. This plan is largely about **wiring them together reactively**, not writing
new detectors.

| Signal | Location | Current behavior | Gap |
|---|---|---|---|
| `is_stalled_promise_text(text)` | `crates/jcode-app-core/src/agent/response_recovery.rs` | Detects the specific DeepSeek-V4-Flash degradation: dense "Let me grep … Let me run …" filler with no tool call. Doc comment names the exact observed offender. | Detection only — used in `turn_loops` to re-continue, but no cross-turn state or escalation. |
| `maybe_continue_stalled_promise(...)` + `stalled_promise_continuations` counter | `crates/jcode-app-core/src/agent/turn_loops.rs` | Re-injects a prompt and continues, bounded per turn. | Bound is per-turn and flat; no totalizer over the route, no compaction/fallback when repeats exceed a threshold. |
| `watchdog` (`beat`, `WorkGuard`, `watchdog.stall`/`recovered`/`alive`) | `crates/jcode-logging/src/watchdog.rs` | Process-level stall watchdog; logs a thread dump + resource snapshot when no beat arrives while work is in flight. | **Log-only.** No subscriber acts on `watchdog.stall`. |
| Session status marking | session persistence | Sessions record `Active` / `Crashed` etc. | A session that is `Active` but stalled is not distinguishable from a healthy `Active` one without external sampling. |
| `set-route-model` / route aliasing | provider/route layer | Can retarget a session onto a different model/route. | Manual only. Not invoked programmatically from degradation signals. |

**Conclusion of the audit:** the product is good at *detecting* stalls and
degradation but has **no feedback loop**. Every rung below is about closing that
loop.

---

## Design

### Layered model

Degradation is handled at three escalating levels. Each level has its own trigger,
action, and rollback, and is only entered once the levels below it have demonstrably
failed to restore forward progress.

```mermaid
flowchart TD
  S[Session makes a turn] --> D{Detect degradation}
  D -- clean --> Idle[Healthy - reset counters]
  D -- stalled-promise / empty-turn / retry-loop --> L1{L1 re-prompt}
  L1 -- recovered --> Idle
  L1 -- still degraded xN --> L2{L2 compaction}
  L2 -- recovered --> Idle
  L2 -- still degraded --> L3{L3 route fallback}
  L3 -- recovered --> Idle
  L3 -- degraded too --> Escalate[Surface to user / stop]

  W[watchdog.stall event] --> Obs[Record system-level stall]
  Obs --> perhapsL2{Long context?}
  perhapsL2 -- yes --> L2
  perhapsL2 -- no --> SurfaceOnly[Notice only - likely CPU/mem oversubscription]
```

### Core concepts

1. **Degradation state keyed by route.** A small persistence record keyed by
   `route_key` (provider + model + alias) accumulates recent degradation events:
   stall type, turn number, timestamp, and the active mitigation rung. This makes
   degradation a *property of the route/session over time*, not a one-shot flag.

2. **Threshold-driven promotion.** When the event count crosses a threshold (e.g.
   3 stalled-promise hits within N turns, or 2 `watchdog.stall` events within an
   hour while work was in flight), the session routes to the next mitigation rung.

3. **Mitigation ladder (only escalate on provable non-recovery):**
   - **Rung 1 — Re-prompt.** Inject a refocus/compression prompt and continue
     (reuses `maybe_continue_stalled_promise`). Low cost, no context loss.
   - **Rung 2 — Compaction.** Force a compaction of the transcript (the observed
     root cause of the DeepSeek degradation is very long context). Preserves
     intent, shrinks the context the model degrades under.
   - **Rung 3 — Route fallback.** Retarget the session to a pinned non-offending
     model/route. This is the rung that actually stops a repeating degradation
     loop. It is the **only destructive-ish step** and must be gated: a config
     flag, an allow-list of candidate fallback routes, and a user-visible notice
     (and by default a confirmation before switching away from an explicitly
     user-chosen model).

4. **System-level stall is a separate axis.** A `watchdog.stall` that coincides
   with high load / memory pressure (the `research/session-freeze` investigation)
   is CPU/mem oversubscription, not model degradation. It should surface a notice
   and suggest bounding concurrent work, **not** trigger a route switch. Route
   fallback only makes sense for turn-content degradation (stalled-promise /
   empty-turn / compact-unfulfilled-tool-request).

### Where the wiring lives

- **State:** a new degradation tracker (counters + current rung + persistence),
  alongside the existing per-session state. Suggested home: a small module under
  `crates/jcode-app-core/src/agent/` or `server/`, exposed through the relevant
  service handle, consistent with the service-split direction.
- **Detection: keep where it is** (`response_recovery.rs`, `turn_loops.rs`).
  Change the flat per-turn `stalled_promise_continuations` counter into a call
  into the degradation tracker so repeats are totalized across turns and escalated.
- **Watchdog subscription:** expose `watchdog.stall` / `watchdog.recovered` as a
  subscription surface (it is currently log-only) so app-core can record stalls and
  correlate them with load.
- **Alerting:** emit a `ServerEvent`-style notice (or TUI banner) on promotion to
  each rung so the degradation is visible, not just logged.

---

## Migration / implementation slices

Each slice must leave the tree compiling and the existing `jcode-app-core` and
`jcode-base` suites green.

### Slice 1 — Watchdog subscription surface (foundation, non-destructive)

- Add a way for app-core to subscribe to `watchdog.stall` / `watchdog.recovered`
  / `watchdog.alive` instead of the events being log-only.
- Add a test proving a subscriber receives the event when a real stalled thread
  emits it (extend the existing `jcode-logging/tests/watchdog_stall_probe.rs`).
- **Exit:** `jcode-logging` exposes a subscription API; app-core test verifies
  receipt; no behavior change for existing consumers.

### Slice 2 — Degradation state tracker

- Add a tracker keyed by route/session that accumulates stall-type events with
  timestamps and current rung.
- Wire the existing per-turn `stalled_promise_continuations` / re-prompt path
  into the tracker so repeats are totalized across turns.
- Persist the tracker alongside the session so state survives a disconnect/reload.
- **Exit:** unit tests for threshold-triggered promotion L1→L2; existing suites green.

### Slice 3 — Mitigation ladder: compaction

- On promotion past L1 (re-prompt already in place), trigger a forced compaction.
- Add the "still degraded after compaction" promotion path to L3.
- **Exit:** integration test showing repeated stalled-promise turns trigger
  compaction once, recover, and reset the tracker.

### Slice 4 — Mitigation ladder: route fallback (gated, most invasive)

- On promotion past L2, retarget session to a pinned fallback route from a
  config allow-list.
- Default off behind a config flag; require a user-visible notice; by default
  require confirmation when the current route was explicitly user-chosen.
- **Exit:** a degraded session on the default route switches to the fallback and
  resumes; a user-chosen-route session surfaces a confirmation instead of
  auto-switching.

### Slice 5 — Alerting + session-status distinction

- Surface each rung promotion and each system-level stall as a visible notice,
  not just a log line.
- Optionally mark a degraded-but-not-crashed session distinctly (e.g. a status
  facet beyond `Active`) so the session picker shows "degrading, mitigating…"
  instead of a misleading healthy `Active`.
- **Exit:** promotion events are observable in the TUI/`ServerEvent`; a stalled
  `Active` session is visually distinguishable.

### Final gate — convergence check

- The per-turn stall counters are fully replaced by the route-scoped tracker.
- `watchdog.stall` has at least one real reactive consumer.
- Full `cargo check -p jcode-base -p jcode-app-core` clean; full suite green.
- Document the mitigation ladder + config flags in the relevant docs.

---

## Non-goals / explicitly out of scope (for now)

- **Do not** auto-change models without a config gate + user notice. Auto route
  fallback is the endgame, not the first slice.
- **Do not** treat CPU/mem oversubscription as model degradation. A
  `watchdog.stall` under load should surface a load notice, not trigger a switch.
- **Do not** attempt destructive actions (dropping a session, sending to
  maintainer, paying) automatically — those belong to the "surface to user / stop"
  terminal.
- **Do not** build new stall detectors unless a real gap shows up. Reuse
  `is_stalled_promise_text`, the empty-turn / guardrail / incomplete-response
  paths, and `watchdog`.

---

## Related material

- `docs/plans/SERVER_SERVICE_SPLIT_PLAN.md` — the shared-server ownership split;
  this plan's state should sit behind a service handle consistently with it.
- `RESEARCH/session-freeze-investigation.md` (branch `research/session-freeze`) —
  the CPU/mem oversubscription freeze, treated here as the separate "system
  stall" axis, **not** conflated with model degradation.
- `crates/jcode-logging/src/watchdog.rs` — existing process stall watchdog.
- `crates/jcode-app-core/src/agent/response_recovery.rs` — existing degradation
  detectors (stalled-promise, compact-unfulfilled-tool-request).
- `crates/jcode-app-core/src/agent/turn_loops.rs` — where the per-turn re-prompt /
  continuation logic lives today.