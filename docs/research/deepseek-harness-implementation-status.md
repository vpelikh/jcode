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

## Takeaway #6 (prune / summarize separation)

Delivered. The deterministic, model-free reclamation layer is now a distinct,
named `prune` stage, separated from the model-driven summarizer, with a shared
policy and report so every consumer stays in lockstep.

- **`crates/jcode-compaction-core/src/prune.rs`** — the new stage:
  - `PrunePolicy` with `node_caps()` (per-node caps: run-every-step budget) and
    `payload_413()` (aggregate byte budgets for request-too-large recovery).
  - `PruneReport { images_stripped, tool_results_truncated }` + `is_empty()`.
  - `prune_contents(&mut [&mut Vec<ContentBlock>], &PrunePolicy)` runs the
    deterministic single-node replacements in a fixed order: aggregate image
    budget (oldest-first) → aggregate tool-result budget (largest-first, only if
    no image was reclaimed) → per-node caps. It reuses the existing
    `strip_large_images_in_contents` and
    `prune_truncate_tool_results_in_contents` as building blocks, so the
    numbers and the escalation order are behavior-preserving.
- **`jcode-base` reusable seam:** `Session::prune_transcript(&PrunePolicy) ->
  PruneReport`, re-exported through `jcode_base::compaction::prune`. It records
  the mutation as a single `ReplaceMessages` event (event-sourced log stays the
  source of truth) and invalidates the provider-message cache when anything
  changed.
- **De-duplication of the 413 recovery paths.** The three previously duplicated
  call sites (`jcode-app-core` `agent/compaction.rs` and both `jcode-tui`
  `model_context.rs` sites) now call
  `prune_transcript(PrunePolicy::payload_413())` instead of tracing
  `strip_oversized_images` then `emergency_truncate_tool_results`. The policy
  and escalation order now live in exactly one place.
- **`/prune` slash command.** `jcode-tui` `commands.rs` + `input_help.rs`: runs
  the model-free node-caps pass on demand (policy built from the configurable
  caps), reports `PruneReport`, and is discoverable via `/help prune`. 2 dispatch tests.
- **Scheduled per-step prune.** Wired at the top of both agent loops
  (`agent/turn_streaming_mpsc.rs`, `agent/turn_loops.rs`), before each next API
  call, over the prefix preceding the latest assistant response. Fresh tool
  results, screenshots and interrupts appended after the latest assistant are
  in the unconsumed suffix and remain intact until the model has read them once.
  Both the **image** and **tool-result** per-node caps run on **every** step: a
  consumed oversized node from a prior turn must be reclaimed even on pure-text
  follow-ups, or it lingers in every subsequent prompt. No-op when within caps.
  A prune invalidates the provider session/cache but preserves the locked tool
  surface (`note_prune_applied`).
- **Configurable per-node caps.** `CompactionConfig` gains
  `prune_tool_result_max_bytes` / `prune_image_max_bytes` (defaults 16384/1024,
  `#[serde(default)]` so existing configs parse) and `PrunePolicy::node_caps_with`
  builds the policy from them, falling back to the built-in defaults on zero.
  Wired at all 4 live call sites (`/prune`, agent, streaming Point-D, headless).

  **Scope note on `/prune`: remote wire support delivered.** `/compact` works
  over SSH/remote via `Request::Compact` → `ServerEvent::CompactResult`. `/prune`
  now has the same parity: `Request::Prune` → `ServerEvent::PruneResult` gets a
  server-side `Agent::request_manual_prune` + `handle_prune` + lifecycle route, a
  TUI `backend.prune()` transport + remote key-handling + `ServerEvent::PruneResult`
  handler, and iOS `Wire.swift`/`SessionReducer` mirrors. The scheduled per-step
  prune already ran server-side regardless of client; the on-demand `/prune`
  command now works locally and over SSH/remote alike.

**Branch review fixes (2026-09-13):**

- **Fresh content loss:** scheduled pruning previously stripped newly generated
  screenshots and truncated tool output before the next model request. Both
  loops now call `Session::prune_consumed_transcript`, which protects the suffix
  starting at the latest assistant response. No assistant response means no
  eligible prefix. The regression failed against full-transcript pruning and
  passed after the boundary fix, including replay and idempotence checks.
  Runtime evidence: `scheduled_prune_preserves_fresh_oversized_results_at_runtime`
  drives the real streaming loop through a bash tool call that returns a 6202-char
  result and asserts that fresh oversized result survives the loop-head prune
  while a consumed oversized image is replaced. This closes the loop-wiring coverage gap
  end-to-end rather than relying only on the session-unit boundary test.
- **Manual persistence and errors:** local and server `/prune` now save changes.
  A failed save reports an error instead of success, and a subsequent no-op
  retries persistence. Isolated temporary-home tests exercise real disk reloads,
  blocked storage paths, and the server handler's request-ID/error response.
- **413-recovery persistence:** the HTTP request-too-large prune paths (TUI
  manual + auto-retry, and server `try_recover_after_payload_too_large`) save
  the pruned transcript immediately, so the mutation is not lost if the retry
  is interrupted or the user does not resubmit.
- **Stale provider sessions:** changes invalidate native provider session IDs
  and agent cache/tool state. Local TUI pruning clears its materialized message
  cache and reseeds the compaction view. The agent disk-reload regression checks
  the cleared native session ID as well as the pruned transcript.
- **Discoverability:** `/prune` was missing from the registered command catalog
  and help overlay. Both are now populated, and autocomplete is tested.
- Added Swift request/result codec and reducer tests. `swift test` passed all
  73 JCodeKit tests on macOS. This is not a device/iOS app acceptance run.

**Review regression gate:** `prune` filters passed 16 app-core, 6 base and 5 TUI
 tests. Full compaction-core/config-types/protocol libraries passed 29/19/82.
 App-core loop/streaming/compaction filters passed 39/7/7. These filters overlap
 and include unrelated tests, so their counts must not be summed as unique
 prune acceptance cases. Live SSH transport and device UI remain outside this
 pass; the server handler, persistence, Rust wire and Swift reducer were tested.

The review build (`scripts/dev_cargo.sh build --profile selfdev -p jcode
--bin jcode`) and direct `target/selfdev/jcode --version` smoke check passed.
The SSH-mode command guard test also passed with `/prune` in its allowed wire
command list. The shared daemon was not reloaded.

**Fresh validation (2026-09-11/12):**

- Full `jcode-compaction-core`, `jcode-config-types`, and `jcode-protocol`
  library suites passed: 29, 19, and 82 tests respectively. The final
  `prune` filter passed 15 app-core, 5 base, and 4 TUI tests.
- The small-cap regression first failed: a configured cap of 1 produced a
  56-byte result. The per-node path now reserves space for a compact marker
  when the historical recovery marker cannot fit. UTF-8 inputs at byte caps
  1, 2, 3, 16, 64, 128, and 4000 stay bounded and a second pass is a no-op.
  The aggregate 413 recovery path is unchanged.
- Added explicit policy tests for custom caps and zero-value fallback, plus a
  Rust wire roundtrip test for `prune` requests and both no-op and changed
  `prune_result` responses.
- Targeted `prune` tests passed in base and TUI. The new direct agent test
  checks provider-view refresh, event-log replay, and second-pass idempotence.
  Broad name filters also match unrelated tests and are not proof of remote
  end-to-end coverage.
- App-core filters `turn_loops` (39), `turn_streaming` (7), and `compaction`
  (7) passed. These are regression coverage, not dedicated acceptance tests
  for every scheduled-prune branch.
- `scripts/dev_cargo.sh build --profile selfdev -p jcode --bin jcode` passed
  in this worktree. The shared daemon was not replaced or restarted, so this
  build is not a live runtime acceptance result.

**Earlier validation limits (superseded in part by the review above):** Live
SSH command execution, iOS compilation, and save/restart persistence were not
exercised in the September 11/12 pass. Wire roundtrips and
agent-level tests provide component evidence, not proof of those workflows.
The full-suite counts and flake descriptions below are historical observations,
not fresh full-suite results.

**Flake note (the full-suite run surfaced a 4th timing-sensitive test).** Under
parallel load the app-core suite is missing 2 (not 3) timeout wall-clock tests,
both pre-existing and unrelated to prune/compaction. The previously-documented
three (`channel::session_picker_menu_flow`,
`server::external_background_task_wake…`, `tool::bash::test_detached_promoted_command_…`)
plus a fourth, `server::debug_command_exec::tests::
debug_tool_selfdev_reload_returns_promptly_for_direct_execution` (asserts a
selfdev/reload signal is acked within a 2-second wall clock). It passes in
isolation (~0.4 s) and exercises none of the prune paths. No action needed for
this work; flagged so a future full-suite run is not mistaken for a regression.

**Full TUI suite (also surfaced pre-existing failures, none prune-related).**
A full `jcode-tui` lib run shows 3 failures, all confirmed unrelated to the
prune change:
- `helpers_tests::build_resume_command_uses_imported_jcode_session_for_codex`
  — passes in isolation (env-home / parallel-load flake).
- `commands::tests::worktree::main_repo_root_from_a_linked_worktree_points_to_the_main_checkout`
  — passes in isolation (shells out to real `git`; host/env parallel-load flake).
- `idle_self_drive_debounces_rapid_repeats` — fails **deterministically and
  identically on a clean `master` worktree at the base commit `ec676907e`**
  (`0 passed, 1 failed`, "first idle poll must run"), so it is a pre-existing
  review-loop failure, independent of this branch. The prune change touches no
  review-loop code (`git show --name-only` of the prune commits lists no
  `review_loop*`/`commands_review*` file).

  Note: the *full* TUI run also stalls indefinitely on
  `ssh_remote_startup_ignores_colliding_local_session_and_onboarding`
  (`crates/jcode-tui/src/tui/app/tests/ssh_remote.rs`, "has been running for
  over 60 seconds"), blocking the suite from emitting a final line. That file
  is not touched by the prune commits either; it is an environment-dependent
  SSH/hang in the same pre-existing class as the others.

**Interpretation noted (scheduled prune now delivered).** The doc's literal
recommendation is to run `prune` *on a cheap cadence* (every step). This is now
delivered: a scheduled per-step prune using `PrunePolicy::node_caps()` runs at
the streaming loop's Injection Point D and in the headless `run_turn`, before
each next API call, shrinking eligible already-consumed history. A `/prune`
slash command exposes the same node-caps pass on demand. It remains a cheap,
model-free per-node-cap pass (no aggregate surgery) and a no-op when within
caps. The build-level and token-accounting interaction with the summarizer on
very large sessions is flagged as an observation follow-up, not an unimplemented
recommendation.

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
  fail-closed sandbox (confinement, not just classification); P2 #11 durable
  inbox, #14 waterfall hooks; P3 #17 layered config. These are large,
  cross-cutting, and benefit from a steer before work begins. **#18 postmortem
  culture, #15 goals domain, and the #10 session/background-running slice are no
  longer open here** — see F5, F6 and F7 below.
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
- **F7 — #10 session/background-running projection (the concrete slice of the
  jobs seam that powers the busy indicator).** ✅ **Delivered.**
  `BackgroundTaskManager::running_snapshot_for_session(session_id)` gives the
  per-session **live** count of background jobs, derived from each task's
  status-file `Running` status rather than from the in-memory map size. It
  also fixes a latent over-count: before this, `running_snapshot()` returned
  `tasks.len()` regardless of whether a task had already reached a terminal
  state, and was global (it mixed every session's background work into the
  focused session's busy indicator). Both the global and the new per-session
  snapshot now filter by live `Running`, and the TUI info widget shows the
  focused session's live count — and, when there is no focused session (a brief
  remote-startup window), the indicator is hidden rather than showing a
  misleading global aggregate. A unit test covers session scoping and
  terminal-status exclusion,
  and an end-to-end TUI integration test exercises the real global background
  manager + `TuiState::info_widget_data`: a spawned live task surfaces as the
  focused session's `background_info.running_count`, does not leak into a
  different session either direction, and an idle session shows no background
  indicator. `jcode-base` background suite: 21 passed, 0 failed. This bounds
  #10 to the background-running projection the takeaway names; a *unified
  multi-executor `jobs` registry* spanning bash `&` + terminal + subagent
  remains broad follow-up, tracked under F2's remaining items.

## F8 — unify tool timeouts on the shared seam (follow-up, parked)

Two parallel timeout systems exist today:

- **Shared seam (takeaway #7):** `Tool::execution_timeout(&self, input)` is wrapped
  by `Registry::execute` with `tokio::time::timeout`; on expiry it **hard-drops**
  the future and returns a model-visible "timed out after Ns" error. `websearch`
  and `session_search` both opt in (the latter input-aware, see below).
- **Native self-managed timeouts:** `bash`, `bg`, and `webfetch` do **not**
  override `execution_timeout()`. They implement their own internal timeout
  inside `execute()`. Notably `bash` **promotes a timed-out command to a
  background task** (dedicated task, own process group, progress handoff,
  `kill_on_drop`) so long work is continued, not killed.

**Follow-up intent:** make the *shared* seam support the richer "promote on
timeout" behavior so `bash`/`bg` can opt in without losing their semantics —
rather than leaving two parallel systems. A concrete design is an opt-in
on-timeout strategy on the `Tool` trait (hard-drop vs promote-to-background)
that routes through the same `Registry::execute` wrap point. This is a
behavior-changing, cross-cutting change (touches the tool trait, the registry,
bash/bg, and the background manager's adoption API) and should get its own
dedicated session with a steering decision on the default for each tool.

**Related (the TUI-info-widget half is done; the tool-timeout broadening is
partial):** the `None`-fallback half of the F8 note covers two distinct items.
The TUI info-widget `background_info` `None`-session fallback was changed to
show no indicator instead of a global aggregate (backward compat is not a goal),
with an integration-test assertion covering the resolved-and-idle and
unresolved-`None` cases. Separately, broadening the tool-timeout opt-in to the
cancellable I/O tools is only partially delivered — session_search opts in (see
below); gmail/conversation_search/ambient remain documented non-opt-ins.

### Broadening the opt-in: session_search (partial)

The concrete `None`-fallback half — give more tools a declared
`execution_timeout()` — is partially delivered:

- **`session_search` now opts in.** `SessionSearchTool` declares a 60s
  whole-call deadline (`EXECUTION_TIMEOUT_SECS`). It is the clean fit: it scans
  up to `MAX_MAX_SCAN_SESSIONS` (10k) session snapshots/journals on
  `spawn_blocking` with no self-managed bound, so a slow disk or a pathological
  exhaustive scan can otherwise hold the turn indefinitely. The scan is
  **read-only and idempotent**, so when the registry wrap point times it out the
  turn returns a clean model-visible "timed out after Ns" error immediately and
  the already-spawned `spawn_blocking` work finishes in the background (wasted
  CPU only, no side effects or corrupted state; default-scope scans are bounded
  by the session-file cap). Tests pin the declared constant, the model-visible
  error contract via the shared `execute_with_deadline` wrap, and the
  input-dependent scale-up (including the raised scan-cap case and the
  negative-cap fallback).

- **Deliberately not opted in (documented, not a gap):**
  - `gmail` — a whole-tool `execution_timeout()` cannot distinguish the
    `connect` action's self-managed ~5-minute browser-approval poll (150×2s)
    from the unbounded HTTP body reads on the other actions. A deadline short
    enough to bind the reads would break `connect`; one long enough for
    `connect` would not bound the reads. Part B now makes an *action-scoped*
    deadline possible (the budget is input-aware), but gmail does not yet opt
    in — wiring an action-specific budget for the non-connect reads is a
    separate, contained follow-up rather than part of the session_search scope
    here.
  - `conversation_search` — sync-dominant (`Session::load` + in-memory search),
    almost no `await` points, so a `tokio::time::timeout` cannot meaningfully
    interrupt it; a declared timeout would be a misleading claim.
  - `ambient`/`schedule` — synchronous queue mutations and runner nudges, no
    network/scan I/O that can hang; no hang risk to bound.

### Seam improvement (Part B): input-dependent budget

The shared takeaway #7 seam was made **input-aware** so a tool can scale its
deadline to the work the call actually requests, instead of one fixed budget per
tool:

- `Tool::execution_timeout(&self)` → `execution_timeout(&self, input: &Value)`.
  The single `Registry::execute` call site passes the raw input it already has.
  The default implementation ignores `input` and returns `None`. `websearch`
  keeps its fixed 30s (its work doesn't scale with input); only `session_search`
  and its overrides were touched (2 override sites, 1 call site, tests), so the
  change is contained to the two crates that implement timeouts.
- **`session_search` scales by scope:** a default-scope call uses 60s
  (`EXECUTION_TIMEOUT_SECS`); an `exhaustive` call (every session, not the
  indexed subset) or an explicit `max_scan_sessions` above the default uses
  120s (`EXHAUSTIVE_EXECUTION_TIMEOUT_SECS`). This removes the earlier
  trade-off that a single fixed budget could not distinguish small from
  expanded scans. Tests pin the scale-up for both the `exhaustive` case and a
  raised scan cap; existing timeout and model-visible-error tests still pass.
- **Rationale:** this is the actual seam fix that addresses the F8 note's
  earlier observation that `execution_timeout()` alone was "too thin" — each
  tool no longer has to re-derive "how do I pick one fixed number."

### Deferred (Part A): abortable scan loop — ✅ **Delivered**

The session_search read-only scan is now **cooperatively abortable** on timeout,
closing the previous trade-off where the blocking scan ran every file to
completion in the background after `execute_with_deadline` dropped the outer
future. The design follows the option this note settled on: a **tool-internal**
cancel flag, not a per-call cancel token threaded through `ToolContext` (which is
constructed at 96 sites, too invasive for one tool's benefit).

- **`session_search.rs`:** a per-call `CheckAbort` (`Arc<AtomicBool>`) is created
  in `execute` alongside an `AbortOnDrop` guard that lives for the whole executing
  future. When `execute_with_deadline`'s timeout fires it drops that future; the
  guard's `Drop` sets the flag, and every scan loop checks `abort.load(...)` at
  each candidate boundary and stops early:
  - raw pre-filter (`filter_candidates_parallel`),
  - index build (`jcode_index_candidates`),
  - scoring deserialize (`score_candidates_parallel`),
  - external JSONL loads (`load_external_candidates_parallel`),
  - claude loads (`load_claude_candidates_parallel`), and
  - the external result fold (`search_external_sessions`),
  plus an **entry-point short-circuit** in `search_sessions_blocking` so a flag
  already set when the blocking call starts skips even file enumeration and
  returns an empty report immediately.
- **Behavior:** the turn still returns immediately with the model-visible
  "timed out after Ns" error (unchanged), but now the already-spawned
  `spawn_blocking` threads are reclaimed at the next candidate boundary instead
  of continuing to the end. The scan stays read-only and idempotent, so bailing
  mid-loop leaves no partial writes. The background index **warmup** path is
  unattended and deliberately uses a never-cancelled flag (no deadline races it).
- **Tests:** two new tests cover the mechanism end to end. A pre-set flag test
  (`pre_abort_flag_short_circuits_the_scan`) returns an empty report with
  `scanned_jcode_sessions == 0` (no file enumeration) and
  `candidate_jcode_sessions == 0` (no scoring) even though matching sessions
  exist; a mechanism test
  (`abort_on_drop_guard_arms_the_flag_when_the_future_is_dropped`) proves the
  `AbortOnDrop` guard, created inside the executing future exactly as `execute`
  does, arms the flag when `execute_with_deadline` drops that future on timeout.
  The existing timeout / model-visible-error / scope-scaling tests still pass
  (session_search suite: 32 passed, 0 failed).

The core F8 "promote-on-timeout" seam for `bash`/`bg`/`webfetch` remains a
separate, behavior-changing follow-up as described above.
