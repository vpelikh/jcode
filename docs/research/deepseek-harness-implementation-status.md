# deepseek-harness plan: implementation status

This file records what has actually been implemented and landed from
`docs/research/deepseek-harness-takeaways.md`, so the state of the plan is
traceable at a glance. Commit SHAs are on the branch named unless otherwise
noted; nothing here is merged to `master` yet.

## Already on `master` (prior work)

These landed via the `jc/event-sourced-log` merge and are part of the event
log scaffolding:

| Takeaway | Deliverable |
|---|---|
| #3 invariants registry | `session/invariants.rs` (`InvariantRegistry`, `CompactionBracket`, `LogInvariant`, `enforce`) |
| #4 projection seam | inline `compaction_event_index` cache + `rederive_all_*` |
| #5 bracket design | `CompactionStart`/`CompactionEnd`, `orphaned_compaction()`, `current_compaction()`, `compact_transcript_with_bracket()` |
| #13 event escape hatch | `SessionEventOp::Unknown { type, data }` uniform `{"op","data"}` envelope |

## Branch `jc/live-compaction-bracket` — takeaway #7 (loop hygiene)

Fully implemented, tested, and committed.

| Commit | Change |
|---|---|
| `515eb1471` | **repeat-tool reminder guard.** New pure `crates/jcode-app-core/src/agent/guard.rs`: canonical (order-insensitive recursive JSON-key) tool-call signature, consecutive-identical-tail detector, `repeat_reminder_from_transcript`. Wired into the streaming loop's Injection Point D and the headless `run_turn`. 9 unit tests. Model-free, no API cost. |
| `d4e0698da` | **opt-in per-call tool timeout.** `jcode-tool-core::Tool::execution_timeout()` (default `None`), `execute_with_deadline()` mapping a hung call to a readable `"timed out after Ns"` error. Wired at the single `Registry::execute` choke point in `jcode-app-core/src/tool/mod.rs` (no per-construction-site bloat). tokio `time` feature. 4 unit tests incl. model-visible timeout. |

  **Review note (2026-09-06):** the per-call timeout *capability* is delivered,
  tested, and wired, but it is currently **dormant in production** — no tool
  overrides `execution_timeout()`, so every live call passes `None` and no
  deadline is ever applied. This was a deliberate call: the tools most likely to
  hang (`bash`, `bg`) already manage their own timeout/background-resume flows,
  and a registry-level deadline would conflict with them. To realize the benefit,
  a specific tool whose execution is externally cancellable must opt in with a
  declared `execution_timeout()`. Until then, the capability is a tested
  extension point, not an active guard.

## Branch `jc/live-compaction-bracket-migrate` — takeaway #5 (live bracket)

The live producer (`jcode-app-core`) uses a **virtual** summary model: it
records compaction state but never physically rewrites `session.messages`,
letting the provider view prepend the summary at request time. The full
`compact_transcript_with_bracket` (which physically replaces the transcript)
is unsafe there, because it would shift `CompactionManager.compacted_count`
offsets and corrupt reload/background accounting.

So the migration adds a **non-message-rewriting bracket seam**:

| Commit | Change |
|---|---|
| `9d7921a2a` | `Session::set_compaction_with_bracket(id, state)`: records the state write inside a balanced `CompactionStart → SetCompaction → CompactionEnd`. Crash-safe (orphan detectable via `orphaned_compaction`), replayable, touches no messages. Falls back to a plain validating `set_compaction` on an invalid state; completes an existing orphan on retry. 3 new tests in `session_events_test.rs`. Wires `sync_session_compaction_state_from_manager` to use it. |
| `51665ff60` | `apply_openai_native_compaction` also writes through the bracket for consistency. |
| `c4cae0c3b` | jcode-tui's mirror writers (`sync_session_compaction_state_from_manager`, `apply_openai_native_compaction`) use the bracket too. |

### Interpretation noted (deviation from the literal doc)

The takeaways doc's "still open" text says the live producer should
`compact_transcript_with_bracket()` — i.e. **physically consolidate**
`session.messages` into a summary + recent tail. What this branch actually
implemented is the **log bracket without physical collapse** (a state-only
bracket). That is a deliberate judgment call made because the physical
collapse would shift `CompactionManager.compacted_count` offsets and corrupt
reload/background accounting; it is **not** a faithful execution of the doc's
literal recommendation. If the user's intent was specifically the physical
consolidation, this branch does **not** deliver it — that remains open as a
separate, documented follow-up.

### Why not physical consolidation

The physical collapse — `compacted_count` rebase + persisted-state realign so
reload resolves against the shorter vector — is broad and behavior-changing,
entangled with forking, ambient runners, recovery, and the provider view. This
branch deliberately delivers the takeaway's headline property **(log-bracketed,
replayable compaction)** on the live path without taking that risk. Physical
consolidation, if ever wanted, is a separate, documented follow-up with its own
accounting design (see "Interpretation noted" above).

## Verification

- `jcode-base` full lib: 1524 passed, 0 failed (session_events_test: 86).
- `jcode-app-core`: agent (58), compaction (4), native-compaction (3),
  messages_for_provider (2) all green.
- `jcode-tui` type-checks clean.
- Under full parallel-suite load, `jcode-app-core` shows 3 timing-flaky tests
  (`channel::session_picker_menu_flow`, `server::external_background_task_wake…`,
  `tool::bash::test_detached_promoted_command_…`). All pass in isolation and are
  untouched by this work (pre-existing flake).

## Branch `jc/branded-session-ids` — takeaway #12 (branded IDs)

Brands the event-sourced-log's structural identity fields at the type level so
an `EventId` can never be passed where a `MessageId` or `CompactionId` is
expected (and vice versa), catching ID-mismatch bugs at compile time rather than
at log-inspection time.

| Commit | Change |
|---|---|
| `6e7b9cdc2` | **Branded identity types.** New `crates/jcode-base/src/session/branded.rs` defines `EventId` (`SessionEvent.event_id` + `parent_id`), `MessageId` (`AppendMessage.message_id`), and `CompactionId` (`CompactionStart.compaction_id`) as `#[repr(transparent)]` newtypes over `String` with **`#[serde(transparent)]`** — the on-disk/on-wire format is byte-identical to the previous raw `String` fields, so persisted event logs round-trip unchanged. `SessionEventError::InvalidEventId`/`InvalidMessageContent` now carry the branded types too. Wired through the `SessionEventMap` producers (`session.rs`, `event_types.rs`), the app-core `SetCompaction` site (`agent.rs`), and every test/`invariant`s construction site. 4 new unit tests. |
| `05b0546c2`, `4a3de549e` | **Backward-compat locks.** `05b0546c2` proves a literal pre-branding `SessionEventOp`/`SessionEvent` JSON payload (raw-string ids) deserializes and re-serializes with the same semantic payload. `4a3de549e` proves the real `Session::load` path hydrates a journal written in the pre-branding shape (bare-string `append_events` ids) and re-derives the transcript + compaction correctly — no migration needed for existing persisted sessions. |

  **Interpretation noted.** Takeaway #12's shortlist mentions branding
  session/tool-call/job/compaction ids broadly. This branch scopes branding to
  the **event-sourced-session-log identity domain** (event / message / compaction
  ids) — the subsystem the prior plan work (`#3/#4/#5/#7/#13`) has been focused
  on — rather than sprawling `Branded` newtypes across every `String` id in the
  codebase (`SessionId`, `ToolCallId`, `JobId`, … remain `String`). Extending the
  same `branded_id!` macro to those domains is a per-domain follow-up, not part
  of this branch.

  **Tightened surface (review, no backcompat constraint).** The branch was
  refined to remove the remaining ergonomics trade-offs now that backward
  compatibility is not a concern:
  - The `impl Into<String>` leak on `set_compaction_with_bracket` /
    `compact_transcript_with_bracket` was changed to `impl Into<CompactionId>`
    (returning `CompactionId`), so the branded type is never round-tripped
    through `String`.
  - The escape-hatch impls (`Deref`, `AsRef<str>`, `From<Id> for String`,
    `From<&Id> for String`) were removed; `as_str()` is the only string
    extraction path, so a branded id cannot be silently treated as (or
    round-tripped through) a plain `String`.
  - `validate_message` no longer fabricates a `MessageId` from a containing
    `EventId` (a type-crossing); it reports the message's own id, or a synthetic
    `"<no-id>"` marker. The now-dead `event_id.is_empty()` branch (event_id is
    validated non-empty before the op match) was dropped.
  - The manual `Serialize`/`Deserialize` impls were replaced with
    `#[derive(Serialize, Deserialize)] #[serde(transparent)]` — same bare-string
    wire format, less code (net -77 lines).
  These changes are behavior-preserving (1555 jcode-base lib tests green;
  app-core/tui build).

## Follow-ups (open, awaiting steer)

These are explicitly open and are tracked as follow-ups, not delivered work:

- **F1 — physical `session.messages` consolidation (doc-literal #5).** The
  state-only bracket (this branch) records the log bracket but does NOT
  physically collapse `session.messages` into a summary + tail, as the takeaways
  doc's "still open" note literally asks. Delivering that requires rebasing
  `CompactionManager.compacted_count` and the persisted compaction state so
  reload resolves against the shorter vector. It is a separate, riskier,
  cross-crate change and is deliberately parked pending a steer.
- **F2 — remaining plan items (large refactors).** P1 #2+#8 execution-world seam +
  fail-closed sandbox (confinement, not just classification); P2 #10 jobs
  seam, #11 durable inbox, #14 waterfall hooks; P3 #15 goals domain, #17
  layered config, #18 postmortem culture. These are large, cross-cutting, and
  benefit from a steer before work begins.
- **F3 — extend `branded_id!` beyond the event log.** The `branded_id!` macro
  now exists in `jcode-base`; applying it to `SessionId`, `ToolCallId`, `JobId`
  would carry takeaway #12's protection wider. Per-id, mechanical, benefits from a
  steer on scope (the event-log branding shipped in `6e7b9cdc2` is the seeded
  pattern).