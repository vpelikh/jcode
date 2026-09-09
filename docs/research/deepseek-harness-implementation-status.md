# deepseek-harness plan: implementation status

This file records what has actually been implemented and landed from
`docs/research/deepseek-harness-takeaways.md`, so the state of the plan is
traceable at a glance.

## Already on `master` (prior work)

These landed via the event-sourced-log merge and are part of the event log
scaffolding:

| Takeaway | Deliverable |
|---|---|
| #3 invariants registry | `session/invariants.rs` (`InvariantRegistry`, `CompactionBracket`, `LogInvariant`, `enforce`) |
| #4 projection seam | inline `compaction_event_index` cache + `rederive_all_*` |
| #5 bracket design | `CompactionStart`/`CompactionEnd`, `orphaned_compaction()`, `current_compaction()`, `compact_transcript_with_bracket()` |
| #13 event escape hatch | `SessionEventOp::Unknown { type, data }` uniform `{"op","data"}` envelope |

## Takeaway #7 (loop hygiene)

Fully implemented, tested, and committed.

- **repeat-tool reminder guard.** New pure `crates/jcode-app-core/src/agent/guard.rs`: canonical (order-insensitive recursive JSON-key) tool-call signature, consecutive-identical-tail detector, `repeat_reminder_from_transcript`. Wired into the streaming loop's Injection Point D and the headless `run_turn`. 9 unit tests. Model-free, no API cost.
- **opt-in per-call tool timeout.** `jcode-tool-core::Tool::execution_timeout()` (default `None`), `execute_with_deadline()` mapping a hung call to a readable `"timed out after Ns"` error. Wired at the single `Registry::execute` choke point in `jcode-app-core/src/tool/mod.rs` (no per-construction-site bloat). tokio `time` feature. 4 unit tests incl. model-visible timeout.

  **Review note (updated 2026-09-09):** the per-call timeout *capability* is
  delivered, tested, and wired. It was previously **dormant** — no tool
  overrode `execution_timeout()`, so every live call passed `None`. That gap is
  now closed for a real, hang-prone tool: **`websearch` opts in** with a 30s
  whole-call deadline (`WebSearchTool::EXECUTION_TIMEOUT_SECS`, documented on
  `execution_timeout()`). This is a deliberate, targeted opt-in and still
  respects the original call: the tools most likely to *run long or background*
  (`bash`, `bg`) are **not** wrapped, because they already manage their own
  timeout/background-resume flows and a registry deadline would conflict. Only
  tools whose execution is externally cancellable and which otherwise have no
  whole-call bound should opt in. `websearch` qualifies: its engine requests
  rely only on the shared client's 15s connect timeout with no bound on the
  body read, and the async reqwest futures drop cleanly on cancellation. Two
  new tests pin the declared timeout and the model-visible timeout error
  contract.

## Takeaway #5 (live bracket)

The live producer (`jcode-app-core`) uses a **virtual** summary model: it
records compaction state but never physically rewrites `session.messages`,
letting the provider view prepend the summary at request time. The full
`compact_transcript_with_bracket` (which physically replaces the transcript)
is unsafe there, because it would shift `CompactionManager.compacted_count`
offsets and corrupt reload/background accounting.

So the migration adds a **non-message-rewriting bracket seam**:

- `Session::set_compaction_with_bracket(id, state)`: records the state write inside a balanced `CompactionStart → SetCompaction → CompactionEnd`. Crash-safe (orphan detectable via `orphaned_compaction`), replayable, touches no messages. Falls back to a plain validating `set_compaction` on an invalid state; completes an existing orphan on retry. 3 new tests in `session_events_test.rs`. Wires `sync_session_compaction_state_from_manager` to use it.
- `apply_openai_native_compaction` also writes through the bracket for consistency.
- jcode-tui's mirror writers (`sync_session_compaction_state_from_manager`, `apply_openai_native_compaction`) use the bracket too.

### Interpretation noted (deviation from the literal doc)

The takeaways doc's "still open" text says the live producer should
`compact_transcript_with_bracket()` — i.e. **physically consolidate**
`session.messages` into a summary + recent tail. What was actually
implemented is the **log bracket without physical collapse** (a state-only
bracket). That is a deliberate judgment call made because the physical
collapse would shift the `compacted_count` offsets and corrupt
reload/background accounting; it is **not** a faithful execution of the doc's
literal recommendation. If the user's intent was specifically the physical
consolidation, it is **not** delivered — that remains open as a
separate, documented follow-up.

### Why not physical consolidation

The physical collapse — `compacted_count` rebase + persisted-state realign so
reload resolves against the shorter vector — is broad and behavior-changing,
entangled with forking, ambient runners, recovery, and the provider view. The
headline property **(log-bracketed, replayable compaction)** was delivered on
the live path without taking that risk. Physical consolidation, if ever
wanted, is a separate, documented follow-up with its own accounting design
(see "Interpretation noted" above).

## Verification

- `jcode-base` full lib: 1555 passed, 0 failed (session_events_test: 91).
- `jcode-app-core`: compaction (+ native-compaction, 413-recovery) green.
- `jcode-tui` type-checks clean.
- Under full parallel-suite load, `jcode-app-core` shows 3 timing-flaky tests
  (`channel::session_picker_menu_flow`, `server::external_background_task_wake…`,
  `tool::bash::test_detached_promoted_command_…`). All pass in isolation and are
  untouched by this work (pre-existing flake).

## Takeaway #12 (branded IDs)

Brands the event-sourced-log's structural identity fields at the type level so
an `EventId` can never be passed where a `MessageId` or `CompactionId` is
expected (and vice versa), catching ID-mismatch bugs at compile time rather than
at log-inspection time.

- **Branded identity types.** New `crates/jcode-base/src/session/branded.rs`
  defines `EventId` (`SessionEvent.event_id` + `parent_id`), `MessageId`
  (`AppendMessage.message_id`), and `CompactionId` (`CompactionStart.compaction_id`)
  as `#[repr(transparent)]` newtypes over `String` with **`#[serde(transparent)]`**
  — the on-disk/on-wire format is a bare string identical to the previous raw
  `String` fields, so persisted event logs round-trip unchanged.
  `SessionEventError::InvalidEventId`/`InvalidMessageContent` now carry the
  branded types too. Wired through the `SessionEventMap` producers (`session.rs`,
  `event_types.rs`), the app-core `SetCompaction` site (`agent.rs`), and every
  test/`invariant`s construction site. 4 new unit tests.
- **Wire-format + journal regression locks.** A literal pre-branding
  `SessionEventOp`/`SessionEvent` JSON payload (raw-string ids) deserializes and
  re-serializes with the same semantic payload. The real `Session::load` path
  hydrates a journal whose `append_events` carry pre-branding bare-string ids
  and re-derive the transcript + compaction correctly.

  **Interpretation noted.** Takeaway #12's shortlist mentions branding
  session/tool-call/job/compaction ids broadly. Branding was first scoped to the
  **event-sourced-session-log identity domain** (event / message / compaction
  ids) — the subsystem this plan's prior work (`#3/#4/#5/#7/#13`) has been
  focused on — rather than sprawling `Branded` newtypes across every `String` id
  in the codebase. The broader per-domain extension to `SessionId`, `ToolCallId`,
  and `JobId` is now delivered as **F3** (see below).

  **Tightened surface (no backcompat constraint).** Branding was refined to
  remove the remaining ergonomics trade-offs:
  - The `impl Into<String>` leak on `set_compaction_with_bracket` /
    `compact_transcript_with_bracket` was changed to `impl Into<CompactionId>`
    (returning `CompactionId`), so the branded type is never round-tripped
    through `String`.
  - The escape-hatch impls (`Deref`, `AsRef<str>`, `From<Id> for String`,
    `From<&Id> for String`) were removed; `as_str()` plus an explicit
    `{}`/`.to_string()` for `Display` are the only string access, so a branded id
    is never *implicitly* coerced to a plain `String`.
  - `validate_message` no longer fabricates a `MessageId` from a containing
    `EventId` (a type-crossing); it reports the message's own id, or a synthetic
    `"<no-id>"` marker. The now-dead `event_id.is_empty()` branch was dropped
    (event_id is validated non-empty before the op match).
  - The manual `Serialize`/`Deserialize` impls were replaced with
    `#[derive(Serialize, Deserialize)] #[serde(transparent)]` — same bare-string
    wire format, less code.
  These changes are behavior-preserving.

## Follow-ups (open, awaiting steer)

These are explicitly open and are tracked as follow-ups, not delivered work:

- **F1 — physical `session.messages` consolidation (doc-literal #5).** The
  state-only bracket records the log bracket but does NOT physically collapse
  `session.messages` into a summary + tail, as the takeaways doc's "still open"
  note literally asks. Delivering that requires rebasing `CompactionManager.compacted_count`
  and the persisted compaction state so reload resolves against the shorter
  vector. It is a separate, riskier, cross-crate change and is deliberately
  parked pending steering.
- **F2 — remaining plan items (large refactors).** P1 #2+#8 execution-world seam +
  fail-closed sandbox (confinement, not just classification); P2 #10 jobs
  seam, #11 durable inbox, #14 waterfall hooks; P3 #17 layered config. These are
  large, cross-cutting, and benefit from a steer before work begins. **#18
  postmortem culture and #15 goals domain are no longer open here** — see F5
  and F6 below.
- **F3 — extend `branded_id!` beyond the event log.** ✅ **Delivered.** The
  `branded_id!` macro moved out of `jcode-base` into a new minimal leaf crate
  `crates/jcode-id-types`, so the identity-bearing `-types` crates can depend on
  it without a dependency cycle (`jcode-base` already depends on several of
  them). All six branded identities now live there and `jcode-base` re-exports
  them unchanged:

  - **infra** — `jcode-id-types` hosts `branded_id!` + `EventId`, `MessageId`,
    `CompactionId`, `SessionId`, `ToolCallId`, `JobId`; 4 unit tests.
  - **SessionId** — `StreamEvent::SessionId` now carries `SessionId`; all
    provider producers and app-core/TUI consumers updated; 2 message-types tests.
  - **JobId** — `DebugJob.id` and the shared `HashMap<JobId, DebugJob>` jobs map
    are branded; 1 app-core test.
  - **ToolCallId** — `ToolCall.id`, `StreamEvent::ToolUseStart.id`,
    `StreamEvent::ToolResult.tool_use_id`, and the persisted
    `ContentBlock::ToolUse.id` / `ContentBlock::ToolResult.tool_use_id` all carry
    `ToolCallId`; app-core keys `sdk_tool_results`/`tool_id_to_name` by
    `ToolCallId`, the TUI tool-call/result tracking sets key by `ToolCallId`, and
    every provider runtime, jcode-base `render`, jcode-tui, and the root CLI
    convert at the wire/`ServerEvent` boundary. The `branded_id!` macro gained a
    `Default` impl (empty string) so `#[serde(default)]`-backed ids still
    deserialize an omitted field exactly as the prior `String` default did. A
    `ToolCall::test()` fixture helper and a `compile_fail` doctest lock the
    cross-type rejection. message-types + id-types tests cover the branding.

  Every wrapper is `#[serde(transparent)]`, so the on-wire/on-disk format is the
  same bare string and persisted data round-trips unchanged (the legacy raw-string
  journal loads verbatim through real persistence). `cargo check --all-targets
  --all-features` and `cargo test --workspace --no-run` are both green;
  `jcode-base` (1554 lib tests, incl. wire-format legacy-load), `jcode-app-core`
  (1446 lib tests; the only failures are the documented pre-existing timing
  flakes), and the provider/compaction suites all pass.
- **F5 — postmortem culture (P3 #18).** ✅ **Delivered.** Added a
  `docs/postmortem/` archive following dsh's structure (executive summary, exact
  root-cause chain, safety nets that failed in order, guardrails added with a
  stable guardrail home). It records the real, shipped `#604` class:
  - `destructive-command-gate-bypasses.md` — `rm -rf ~` reached a home; the
    reflection gate then had three more escape-route classes (wrapper commands,
    piped deletes, conditional recursive flags) closed under review.
  - `reflection-gate-background-dispatch-604.md` — `run_in_background` early
    return bypassed the gate until it moved before the escape path.
  - `protected-path-deletion-second-route-604.md` — `apply_patch` deleted by
    absolute path; exact-match credential protection missed files *inside* a
    store until made recursive.
  Each documents the guardrail's stable home (`jcode-command-risk`,
  `bash_destructive_gate.rs`, `apply_patch.rs`) so the next instance of the
  class is cheaper to prevent.
- **F6 — goals domain (P3 #15).** ✅ **Delivered** (pre-existing on master via a
  different spelling than the doc's `goals`). jcode ships a same-session
  objective domain distinct from the durable todo list: the `initiative` tool
  (`crates/jcode-app-core/src/tool/goal.rs`, `crate::goal`) manages durable,
  steerable initiatives with a progress percentage, milestones, next steps,
  blockers, and checkpoints — separate from the `todo` tool / pinned todos. It
  is registered on the tool registry and has a side-panel Goals view. This
  satisfies the doc's separation of a steered objective from the user's task
  list; the spelling differs ("initiative" vs "goal") but the domain intended by
  #15 is present.