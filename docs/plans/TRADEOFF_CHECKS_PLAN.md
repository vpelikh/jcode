# Trade-Off Checks on Completed Tasks

## Problem

When a task is completed, jcode's quality gates verify that the agent built a
feedback loop, exercised real acceptance paths, traced every requirement to a
check, and carried the work through delivery. But none of the gates ask whether
the agent actually **considered alternatives and their trade-offs** before
settling on an approach.

A task can clear every existing gate while silently skipping the decision that
matters most: there was a different way to build this, and the chosen design
cost something real. Careless completion skips the comparison entirely; even a
good completion often solves one obvious approach and never confronts the
trade-offs an expert would have weighed. The result is a confident-looking
solution that fails the user's actual constraints.

## What this adds

A new, difficulty-calibrated gate, applied at completion time, that asks the
agent to record:

- **trade_offs** — the meaningful decisions the work required and their
  trade-offs (cost, complexity, performance, compatibility, maintenance),
- **explored_alternative** — whether a credible alternative was actually
  considered and why it lost,
- **considered** — a summary of the alternatives weighed.

Like every existing assessment, these are reported by the model through the
`todo` tool's per-goal `goals` block and checked against a private threshold.
They do **not** leak scores or pass/fail language into the model-visible schema.

## Design

### 1. `TradeOffState` semantic enum (`jcode-task-types`)

```
TradeOffState {
    NoneConsidered  = "none_considered",
    Implicit        = "implicit",
    SomeConsidered  = "some_considered",
    Diligent        = "diligent",
    Exhaustive      = "exhaustive",
}
```

Ordered so `>= SomeConsidered` becomes the ordinary bar and more involved goals
demand `Diligent`. This mirrors how `FeedbackLoopRelevance`, `Coverage`, and
`Traceability` are used. No legacy numeric mapping is required (new field), so
I keep the full `legacy`/`score` surface only for consistency with the macro.

### New `TodoGoal` fields (`jcode-task-types`)

```rust
pub trade_off: Option<TradeOffState>,           // how carefully alternatives were weighed
pub trade_offs: Option<String>,                 // what decisions + trade-offs were considered
pub trade_off_explored_alternative: Option<bool>, // whether a credible alternative was actually explored
```

- `trade_off` + `trade_offs_history` are tool-maintained like the other score
  histories.
- `trade_offs` and `explored_alternative` are descriptive, not gated directly;
  they feed the continuation wording.

### Pass predicates (`jcode-base/src/todo.rs`)

```rust
pub fn trade_off_passes(state: Option<TradeOffState>) -> bool {
    state.is_some_and(|state| state >= TradeOffState::SomeConsidered)
}
```

Difficulty calibration reuses the existing approach: involved goals require
`TradeOffState::Diligent`; simpler goals clear with `SomeConsidered`. This is
folded into `delivery_state_passes` so a marked-complete goal that never
considered alternatives is held at the completion gate.

### Gate continuation message

`build_todo_ownership_continuation_message` gains a per-goal line when the
trade-off check fails, phrased without disclosing scores or thresholds:
"Goal \"x\": consider at least one credible alternative and weigh its trade-offs
before calling the result done."

### `GateObservationKind::TradeOff` + digest wording

A goal whose trade-off check is weak while a group is being completed records a
`GateObservationKind::TradeOff`, surfaced once at turn end in the gate digest.
Wording mirrors the other gates (late-climb friendly).

### Telemetry

Add `TodoGateKind::TradeOff` and a `todo_gate_tradeoff_count` to both the session
and turn telemetry counters, wired through `record_todo_gate`.

## Why now / scope

This is a *completeness* enhancement to the existing completion review, not a
new subsystem. It requires no storage migration (fields are all `Option` with
`skip_serializing_if` and default `None`), so existing sessions and goals stay
valid.

## Out of scope

- Not changing the feedback-loop, relevance, coverage, or traceability gates.
- No UI/UX changes beyond the existing gate digest and ownership continuation.
- No persistence schema version bump.

## Testing

- Unit tests for `delivery_state_passes` including a goal whose alternatives
  were not considered.
- Test that `build_todo_ownership_continuation_message` adds the trade-off line.
- Gate-digest test that a weak trade-off observation is surfaced.
- Telemetry counter test for `TodoGateKind::TradeOff`.