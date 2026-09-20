# What jcode can take from `deepseek-ai/deepseek-harness`

**Context.** `deepseek-harness` (`dsh`) is DeepSeek's open-source agent harness. It is
TypeScript, built on [Cordis](https://github.com/cordiverse/cordis) ("everything is a
plugin"). jcode is Rust, multi-crate. The two projects solve the same problem (an
autonomous coding agent) with parallel subsystems, so the transferable value is mostly
*architecture and contract design*, not code.

**Scope note.** The **Event-Sourced Session Log** is a design direction jcode is moving
toward: `crates/jcode-base/src/session/event_types.rs` already carries the scaffolding
(`SessionEventMap`, `SessionEventOp`, `SessionEvent`), but the feature is **not finished** —
the session layer does not yet persist an append-only event log end-to-end, and compaction
is not yet log-bracketed and replayable. This report therefore treats the event-sourced log
as a target to build toward and focuses on (a) shaping that design so it matches dsh's
proven patterns from the start and (b) the subsystems jcode either lacks or implements more
rigidly.

**How the event-sourced log relates to these takeaways.** Rather than treating any subsystem
below as "done," each takeaway states whether it should be designed into the event-sourced
log **from the start** or layered on **later**. Items that shape the log itself are flagged as
"designs the event-sourced log"; items that add behavior on top are flagged as "complements
the event-sourced log." This keeps the report useful while the feature is still in motion.

**Relevant state as it exists today.** The scaffolding in `event_types.rs` anchors several
takeaways:
- `SessionEventMap` (append-only `events: Vec<SessionEvent>`), a `SessionEventOp` compiled
  union (`AppendMessage`, `ReplaceMessages`, `InsertMessage`, `MemoryInjection`, `ReplayEvent`,
  `SetCompaction`, `ClearAll`), and each `SessionEvent` carrying `event_id`, an optional
  `parent_id` (documented "for merge-extensibility"), and a `version` for conflict resolution.
  This aligns with the log-based compaction shape (takeaway #5) and the merge-extensible event
  union direction (takeaway #13) — but it is scaffolding, not a finished end-to-end log.
- `append_event` validates and skips invalid events with a stderr diagnostic — but there is
  **no dedicated invariants registry** (takeaway #3 is a real, open gap: confirmation found
  no `invariant`/`projection` crate; `grep` hits for "invariant" are unrelated onboarding/
  telemetry code).
- There is **no general projection registry**; only an inline `compaction_event_index` cache
  inside `SessionEventMap` (takeaway #4 is an open gap, though the inline cache is a precedent
  to build the broader seam from).

**How to read this.** Each takeaway lists: the dsh pattern, where it lives in dsh, the
jcode equivalent today, and a concrete recommendation. Each closes with **What this gives
us** — the practical change the pattern enables, in concrete jcode terms rather than
architectural prose. Priority at the end is my judgement of value-vs-effort for jcode, not
dsh's own priority.

## Contents

- [P0 — structural leverage](#prioritized-shortlist-for-jcode)
- 1. [The agent loop is a plugin, not the core](#1-the-agent-loop-is-a-plugin-not-the-core)
- 2. [Capability seams: Definition / Provider / Consumer triple](#2-capability-seams-definition--provider--consumer-triple)
- 3. ["Model-visible means logged" as a runtime invariant](#3-model-visible-means-logged-as-a-runtime-invariant)
- 4. [Projection seam: derive typed state from the log, don't re-read it](#4-projection-seam-derive-typed-state-from-the-log-dont-re-read-it)
- 5. [Compaction as a log-bracketed, replayable operation](#5-compaction-as-a-log-bracketed-replayable-operation)
- 6. [Separate "prune" from "summarize"](#6-separate-prune-from-summarize)
- 7. [Loop-hygiene guards: repeat-tool reminder + per-call timeout](#7-loop-hygiene-guards-repeat-tool-reminder--per-call-timeout)
- 8. [Sandbox fail-closed, with a strictly-wider escalation ladder](#8-sandbox-fail-closed-with-a-strictly-wider-escalation-ladder)
- 9. [Subagent delegation as one provider seam, with continuable children](#9-subagent-delegation-as-one-provider-seam-with-continuable-children)
- 10. [Jobs seam for background work](#10-jobs-seam-for-background-work)
- 11. [Durable inbox: next-turn vs next-step, with claim semantics](#11-durable-inbox-next-turn-vs-next-step-with-claim-semantics)
- 12. [Branded IDs at the type level](#12-branded-ids-at-the-type-level)
- 13. [Extensible event union with an escape hatch (Rust-friendly)](#13-extensible-event-union-with-an-escape-hatch-rust-friendly)
- 14. [Waterfall interception as the primary extension point](#14-waterfall-interception-as-the-primary-extension-point)
- 15. [Goals vs Todos as distinct domains](#15-goals-vs-todos-as-distinct-domains)
- 16. [Session fork/resume/title as derived operations](#16-session-forkresumetitle-as-derived-operations)
- 17. [Composable profiles + live patch reload](#17-composable-profiles--live-patch-reload)
- 18. [A postmortem culture for bug-class escapes](#18-a-postmortem-culture-for-bug-class-escapes)
- [What NOT to take](#what-not-to-take)
- [Sources](#sources)

---

## 1. The agent loop is a plugin, not the core

- **dsh:** `packages/core/agent` declares the public `Agent` interface + `agent/*` events;
  `packages/core/agent-loop` is the *one concrete* driver. Extensions depend on `agent`
  (or `ctx.agents`), never on `agent-loop`, so the loop is swappable. Creation goes through
  `ctx.agents.create/resume` + `setFactory`, and `ctx.agentLoop` is the registered factory.
- **jcode:** `jcode-agent-runtime` plus provider crates. The driver exists but the
  "extension depends on the interface, not the implementation" seam is not a stated contract.
- **Recommendation:** Document and enforce a `jcode-agent-core` trait boundary that
  `jcode-agent-runtime` implements and every hook/extension/UI depends on. This is the
  single highest-leverage structural move because it lets features (review-rounds,
  telegram, swarm) compose without reaching into the loop.

**What this gives us.** If `jcode-agent-runtime` is the *only* thing that touches the concrete loop, then changing the loop internals (better streaming, a new cancellation contract, an async agent) never breaks review-rounds or telegram. Today those features presumably have their own copies of loop state; coupling means each fork diverges silently. A shared trait makes each feature a thin UI over a stable interface — like plugging a different engine into an airplane without redesigning the wings.

## 2. Capability seams: Definition / Provider / Consumer triple

- **dsh:** Every swappable capability is a *seam* with three roles — a Service Definition
  (interface), a Provider (implementation), a Consumer (model-facing tool or host). One
  example: `ctx.fs` (filesystem) has `fs-local`, `fs-sandbox`, `fs-e2b` providers; `ctx.shell`,
  `ctx.subprocess`, `ctx.terminals`, `ctx.lsp` *share the same execution world*, so pointing
  fs+subprocess+shell+terminal at a remote sandbox moves Bash, PTY and LSP together with no
  provider forks.
- **jcode:** Providers are split per LLM vendor (`jcode-provider-*`). But shell/filesystem/
  sandbox/approval are not expressed as one coherent execution-world seam: `jcode-command-risk`
  classifies risk but confinement is separate, and there is no single "execution world"
  abstraction that also relocates LSP/terminal.
- **Recommendation:** Extract a `jcode-execution-world` (or `jcode-sandbox`) seam whose
  providers own fs + subprocess + shell + terminal + lsp together. This directly enables a
  remote-sandbox deployment and gives `jcode-command-risk` a place to live as policy, not
  parser. (Pairs with takeaway #8.)

**What this gives us.** Right now if you wanted to run commands on a remote machine instead of localhost, you'd have to touch every provider that runs shell/subprocess/terminal commands. With a shared execution-world seam, you swap one provider and the whole transport moves. It also lets `jcode-command-risk`'s blast-radius classifier sit inside the seam as *policy* rather than parsing commands in isolation — the only way to guarantee the classifier sees everything.

## 3. "Model-visible means logged" as a runtime invariant

- **dsh:** `ctx.invariants` is a package-owned registry of runtime assertions. The headline
  invariant: anything that reaches a model request must be reconstructable from the session
  log; a failing assertion panics. `invariants` is consumed by `session`, `agent`, `scope`,
  `agent-loop`.
- **jcode:** The event-sourced log is scaffolding in progress; there is no named invariant
  registry asserting log coverage yet.
- **Recommendation (designs the event-sourced log):** Add a small
  `jcode-session-invariants` registry. Seed it with "every request's inputs are reconstructable
  from the log" and "no model-visible message is emitted outside the log path." Make it a
  hard `debug_assert` in dev and a log+metric in release. This is exactly the class of
  check that prevents the subtle replay/telemetry bugs dsh's postmortems are about.

**What this gives us.** When a test passes but a real session replays a different conversation, or when a hook injects model context that never appears in the log, the invariant fires and points you at the exact log boundary that broke. Without this, you have to *guess* whether the discrepancy is in compaction, a hook, or a telemetry write. The invariant doesn't prevent the discrepancy, it makes it *findable* — the difference between "it works on tests but not production" and "a log assertion told me exactly which node diverged."

## 4. Projection seam: derive typed state from the log, don't re-read it

- **dsh:** `ctx.sessionProjections` (`packages/session/session-projection`) owns *projection
  units* that fold committed events **incrementally**. Host consumers read one typed state via
  `stateOf(key)`; carriers batch cropped client views with `snapshot()`. The agent loop
  registers shared `turnBoundary` state. Consumers *require* the service at activation and fail
  explicitly if the key is absent — no silent default.
- **jcode:** Titles, usage overlay, and UI state are re-derived ad hoc (titles via ad hoc
  session-title resolution in `session/crash.rs`, usage/UI by scanning the raw event stream),
  while todos live in a *separate* persisted domain (`jcode-base/src/todo.rs` + `jcode-task-types`,
  `TodoPlan`/`TodoItem`/`TodoGoal`) rather than as a projection of the event log.
- **Recommendation (designs the event-sourced log):** Build a projection registry the
  event-sourced log feeds. Titles, todos, usage overlay, and TUI widgets subscribe to *derived*
  state rather than scanning the raw event stream. This is what makes the event-sourced log
  pay off: one fold, many readers, and replay determinism for free.

**What this gives us.** Right now titles, todos, and usage counters are computed by scanning the raw log or recomputing from messages on every render. When 50 clients open sessions and one reshapes a history, all 50 recompute — which is slow, gives different clients different views, and breaks telemetry (the server can't give one number to two different watchers). With projections, the server computes the state once and pushes a delta. When you fork a session, you get the projected title instantly without scanning the child's log from scratch.

## 5. Compaction as a log-bracketed, replayable operation

- **dsh:** `ctx.compaction` is an abstract seam; `compaction-basic` implements it. The lock is
  a *log marker*, not a mutex: append `compaction/start` → summarize span → replace the span
  with one `user/message` carrying `surfaceOp: replace` (the only surface mutation) → append
  `compaction/end`. A crash between start and end leaves a detectable orphaned lock, never a
  false success. `deriveMessages()` renders the summary and keeps the shadowed events in the raw
  log, so replay reproduces the exact conversation. Edges are snapped to **tool-pairing
  boundaries** (no unanswered `tool/call` crosses the cut).
- **jcode:** `jcode-compaction-core` does token-budget + emergency truncation, but it rewrites
  the message list in place; there is no bracket marker, so replay after a crash mid-compaction
  is undefined. The event-sourced scaffolding points the right way — `SessionEventOp::ReplaceMessages`
  + `SetCompaction` are exactly the dsh "replace the span with a summary, keep the raw events"
  shape, and `SessionEventMap` caches `compaction_event_index` for O(1) current compaction — but
  this was not wired end-to-end when this report was written. So the *bracket* must be designed
  into the log, not assumed.
- **Recommendation (designs the event-sourced log — includes the bracket):** Build compaction
  as a log-bracketed operation from the start. The scaffold's `ReplaceMessages`/`SetCompaction`
  give the right surface shape; the missing design is **crash-safety of the bracket** and
  **edge integrity**:
  1. Add an explicit open/close marker pair (analogous to `compaction/start`→`/end`) so a crash
     mid-summarize leaves a *detectable orphaned lock* rather than a half-applied summary. Today
     `SetCompaction` writes in one shot; if the `ReplaceMessages` for the summary and the
     `SetCompaction` state are not atomic with respect to a crash, replay can observe an
     inconsistent surface.
  2. Add `toolPairingBalanced(start, end)` edge validation (snap cut points to tool-pairing
     boundaries) so a compact can never split an open `tool/call`/`tool/result` pair — the rule
     dsh enforces to keep the derived surface well-formed.
  These are the natural first consumers of takeaway #3 (invariants).

**What this gives us.** Without the bracket, a compaction that crashes leaves the session in an invisible bad state: the session appears compacted (title updated, token count drops) but future log-derived messages include raw tool calls from *before* the crash, which the model sees as if they were called *after*. With the bracket, replay stops at the orphan marker and the user sees an obvious "incomplete compaction" state instead of a silent corruption.

**Current status (2026-09-04).** The bracket is now **designed, durable, and replayable** in the session layer:
- `SessionEventOp::CompactionStart` / `CompactionEnd` are wired through the (de)serializer and the persistence stack; `SessionEventMap::orphaned_compaction()` detects a crashed (open) bracket, and the `CompactionBracket` invariant flags it.
- The event log is persisted **end-to-end**: `event_map` is kept in the snapshot, and the journal carries `append_events` so log-only events (brackets, plugin `Unknown`) survive a crash recovered purely from the journal instead of being collapsed into a rebuild-from-legacy-vectors. Tests cover snapshot round-trip, journal-append reload, and orphan detection after reload.
- Producers now have a seam: `Session::compact_transcript_with_bracket()` emits a balanced `Start` → `replace` → `End` bracket atomically, keeps the legacy `compaction` vector in sync, and on a retry completes an orphaned in-flight bracket (crash-safe) instead of double-opening it.

**Still open (deliberately not forced).** The *live* compaction producer (`jcode-tui`/`jcode-app-core`) currently runs a prefix-summary model — it does **not** physically consolidate `session.messages` into a summary + recent tail, so `compact_transcript_with_bracket()` is the *integration point* to adopt, not something the live path calls yet. Migrating the live producer onto the seam is a behavior-changing, cross-crate change and is tracked separately from this report's design deliverable. This is an explicit, documented follow-up rather than an unimplemented recommendation.

## 6. Separate "prune" from "summarize"

- **dsh:** `ctx.toolResultPruner` rewrites oversized *current* tool results through replayable
  single-node surface replacements *before* summary compaction. It is model-free and
  independent of the summarizer.
- **jcode:** `jcode-compaction-core` mixes truncation into compaction logic
  (`EMERGENCY_TOOL_RESULT_MAX_CHARS` etc.).
- **Recommendation:** Split a `prune` stage (deterministic, model-free, replayable per-node
  replacements) from the `summarize` stage. Cheaper, testable, and reusable for the 413-payload
  recovery path jcode already special-cases.

**What this gives us.** A 4000-character tool result that survives compaction because the emergency cap kicks in still gets re-summarized on every later compaction cycle. If pruning and summarization are separate, you can run `prune` on a cheap schedule (every step) and `summarize` less often (every 5 steps), because the prune is cheap and deterministic. It also makes the summarizer's job smaller — it doesn't need to handle edge cases around tool result sizes, just about summarizing the remaining history.

## 7. Loop-hygiene guards: repeat-tool reminder + per-call timeout

- **dsh:** `guard/` ships two tiny plugins in the base bundle:
  - `repeat-tool-reminder`: when the model repeats the *exact same* tool call, inject a reminder
    to change approach or finish (stops burning tokens in a stuck loop).
  - `timeout-policy`: tool calls that declare a timeout get one; a hung call returns a clear
    timed-out error instead of stalling the session.
- **jcode:** `jcode-command-risk` exists; there is no repeat-call detector and no per-tool
  timeout that returns a clean model-visible error.
- **Recommendation (high value, low effort):** Add a `repeat-tool-reminder` guard — cheap, no
  model call, catches the most common runaway-loop failure mode. Add an optional per-tool-call
  timeout that surfaces as a structured tool error. Both are pure wins.

**What this gives us.** The classic stuck-loop failure mode: model calls `bash("git status")` 15 times, token count explodes, user has to kill the session. `jcode-command-risk` catches the `rm -rf ~` variant by blast radius, but it doesn't catch the 15th identical `git status`. The repeat-tool reminder catches that: after N identical calls, it injects "Please change your approach or finish this step." That's it. No LLM call, no API cost, no latency. For the timeout guard: without it, a stuck tool call (not hang — just slow) blocks the whole session until it returns or the user Ctrl-C's. A per-call timeout gives the model a clean `Error: timed out after 30s` message so it can retry with a smaller scope rather than having to figure out what went wrong from a dead stream.

## 8. Sandbox fail-closed, with a strictly-wider escalation ladder

- **dsh:** `ctx.sandbox` confines same-world subprocesses to a file-effect policy:
  `read-only` / `workspace-write` / `danger-full-access`. Key rules: **fail closed**
  (`SANDBOX_UNAVAILABLE`) — never silently run unconfined; **policy rides the call** (per-call,
  never fixed on the provider); escalation requests a *strictly wider* mode the human approves
  once. Denial/escalation vocabulary is shared so bash and fs cannot drift.
- **jcode:** `jcode-command-risk` classifies blast radius and escalates to a reflection prompt,
  but does not confine; the catastrophic tier is a path-based hard deny.
- **Recommendation (complements #2):** When confinement is added, adopt the fail-closed +
  strictly-wider-escalation contract verbatim. It is the part of dsh's sandbox design that
  actually prevents the "ran unconfined because the sandbox was missing" bug (postmortem-class).

**What this gives us.** Right now `jcode-command-risk` classifies blast radius but does not confine — so a call it "didn't catch" can still run unconfined. The fail-closed rule means a sandbox that cannot enforce a policy errors with `SANDBOX_UNAVAILABLE` rather than silently running unconfined. The strictly-wider escalation means the model asks the user "can I run this broader command?" once and the user can allow-once, not grant a blanket exception. Prevents the postmortem-class bug where the system believed it was safe but was not.

## 9. Subagent delegation as one provider seam, with continuable children

- **dsh:** `ctx.subagents` is one contract with many providers — in-process spawn, in-process
  fork, ACP child, SDK child, real Codex child, real Claude Code child — side by side. Children
  are either one-shot (single result) or **continuable** (durable session accepts later messages,
  interruptible). Discovery (which children exist, mode, lineage, activity) needs no resume.
- **jcode:** `jcode-swarm-core` orchestrates workers; delegation from the main agent is less
  formalized as a single swappable seam.
- **Recommendation:** Model child-agent delegation as a `Provider` seam with both one-shot and
  continuable shapes, so jcode's own swarm, an ACP delegate, or a future remote delegate all sit
  behind `ctx.subagents`/`jcode-subagent`. Reuse the event-sourced log for continuable children.

**What this gives us.** Without a single subagent seam, jcode's swarm, telegram bot, and ACP bridge each invent their own delegation. When one of them wants to run a child agent differently (e.g., remote sandbox vs fork-in-process), they have to touch every consumer. A unified seam means adding a remote-agent provider once and every consumer — swarm, telegram, ACP — can use it without knowing the difference.

## 10. Jobs seam for background work

- **dsh:** `ctx.jobs` is a background-job registry; `job_*` tools collect or stop it. Tool bash,
  terminal, and subagent all register their background work here.
- **jcode:** `jcode-background-types` exists but background work is not unified behind one seam
  that every executor reports into.
- **Recommendation:** Unify background execution (bash `&`, terminal, subagent) under a `jobs`
  seam so one `job_status`/`job_stop` surface covers all of them.

**What this gives us.** When a bash command background `&`, a terminal line goes idle, or a subagent launch fires, there is no single place that says "background work exists." Each executor has its own notification path. A unified jobs seam gives every consumer one stop/collect surface. Telemetry can see all background work. You can implement `session/background-running` as a projection of this seam — "there are 3 background jobs in this session" — which directly powers the TUI usage overlay and the server's busy indicator.

## 11. Durable inbox: next-turn vs next-step, with claim semantics

- **dsh:** The agent owns a two-list durable inbox (`next-turn`, `next-step`). `send` routes to a
  boundary and optionally wakes; `followup`/`steer`/`inject` are fixed aliases. `claim(target)`
  removes the proposed step batch through pure-deletion splices and emits per-message claimed
  notifications. UI projections reconstruct state from durable splices.
- **jcode:** Follow-up/steer exist conceptually but the inbox is not modeled as a *durable,
  claim-able* projection.
- **Recommendation (complements #4):** Represent the pending-input inbox as a durable projection
  of the log so a resume/replay reconstructs exactly what was queued. This matters for telegram
  and review-rounds, which rely on queued follow-ups across turns.

**What this gives us.** Today follow-up and steer exist in the codebase but the pending-input queue is not a first-class log projection. When a session resumes after a crash, pending queued inputs are lost or the driver just doesn't know they were queued. Making the inbox durable means: (1) resume works correctly for users who queue follow-ups, (2) the server can reconstruct inbox state from the log on reload without holding it in memory, (3) a TUI client can read its queued inputs from the log on reconnect.

## 12. Branded IDs at the type level

- **dsh:** `SessionId`, `ToolCallId`, `JobId`, `CompactionId` are *branded* (structurally
  strings, non-interchangeable at compile time). `brandString<T>()`; comparison/logging/JSON
  stay ordinary.
- **jcode:** Likely uses plain `String`/`Uuid` for these identities.
- **Recommendation (cheap, prevents a real bug class):** Introduce a `Branded<...>` newtype
  pattern for session/tool-call/job/compaction ids so a `ToolCallId` can never be passed where a
  `SessionId` is expected. Trivial in Rust with a `#[repr(transparent)]` wrapper.

**What this gives us.** In a multi-crate project, a `ToolCallId` is a plain `String` or `Uuid`. At compile time, `fn handle(id: ToolCallId) -> ToolCallResult` looks identical to `fn handle(id: SessionId) -> Session`. They only fail at runtime when you trace through logs and find a mismatched call. A branded newtype (`newtype!(ToolCallId = String)`) makes the compiler reject the wrong one at the call site. In Rust this is a single `#[repr(transparent)] newtype_alias!` or struct with `#[repr(transparent)]` — roughly 6 lines of code.

## 13. Extensible event union with an escape hatch (Rust-friendly)

- **dsh:** `SessionEvent` is a `…Map → derived-union` built by *declaration merging* — plugins
  add variants without editing the core package.
- **jcode:** Rust has no declaration merging, so the event-sourced log must pick an extension
  strategy. **Note:** jcode's `SessionEvent` scaffold already carries `parent_id` (documented
  "for merge-extensibility") and a `version` for conflict resolution — this points at an
  extensible model — but the *fallback variant* that lets an unknown event kind deserialize
  instead of error is not yet present.
- **Recommendation (designs the event-sourced log):** For the Rust `SessionEvent` enum,
  reserve an `Unknown { type: String, data: Value }` fallback variant (or a plugin event
  registry) so future plugins can append event kinds without a breaking change to the core enum.
  Document "add an event, don't edit the loop" as the extension rule.

**What this gives us.** Rust enums are closed at definition time. Adding a new event variant to `SessionEvent` requires editing the core enum and every `match` arm. If a future plugin needs to append an event kind the core doesn't know about, it either waits for a release or waits for a shared enum extension. An `Unknown { type: String, data: Value }` fallback (parsed first, then matched) lets plugins emit events without waiting for the core package. Combined with the `parent_id` field the scaffold already carries ("this event came from that one") the data model points the right way — it just needs the escape hatch to make it extensible without a release cycle. **Reserved key:** `op` is the wire discriminator, so an `Unknown` payload must not use top-level `op` (the serializer drops it deterministically to keep the wire unambiguous); all other payload fields round-trip losslessly.

## 14. Waterfall interception as the primary extension point

- **dsh:** `agent/pre-step`, `agent/request`, `llm/stream`, `tools/pre-execute`/`execute`/
  `post-execute` are **waterfalls**: listeners must call `next()` to delegate. `agent/turn-stopping`
  is serial (no `next()`). This is how you rewrite claimed messages, reject a step, or swap a tool
  result.
- **jcode:** Hooks exist (`pre_tool`, lifecycle) but the interception model around the turn/step
  is less formally a waterfall with ordered delegation.
- **Recommendation:** Formalize the turn/step pipeline as explicit waterfall hooks with `next()`
  semantics, so a future feature can rewrite/observe a step without forking the loop (ties back
  to #1).

**What this gives us.** Today adding a new interception point around the turn/step means forking the loop code. If there are 5 interception points planned and each one touches the same loop function, you have 5 #ifdefs scattered across 500 lines. A waterfall model means: register a callback for `pre_step`, it receives the claimed batch and returns a `PreStepDecision`, and the loop handles the routing. The loop owns the iteration; the callback owns the decision. This is what lets dsh's hooks rewrite messages, reject steps, or inject context without ever touching `agent-loop` package internals.

## 15. Goals vs Todos as distinct domains

- **dsh:** `ctx.goals` manages a *same-session objective*, continued through `agent/*`; separate
  from the durable todo list.
- **jcode:** `jcode` has todos (the `todo` tool / pinned todos). A same-session objective domain
  is not separated.
- **Recommendation (minor):** Consider a lightweight `goals` domain distinct from todos, so the
  loop can steer toward an objective without conflating it with the user's task list.

**What this gives us.** Having todos and goals mixed together means the TUI can't show "4 of 10 subtasks done (goal)" without the user also having to define todos for a goal they are already tracking. A separate `goals` domain is a single struct with a `progress` field and a `steps` list. The TUI can render it as a progress bar. The loop can inject steering messages toward the goal ("done 60% — next try the error path") without conflating goal steps with user task items. The `todo` tool stays for user-created task lists; a new `goal` tool creates a steerable objective with a different lifecycle.

## 16. Session fork/resume/title as derived operations

- **dsh:** `ctx.sessions.fork(source, boundary?, childSessionId?)`; titles come from the *sole*
  `ctx.sessionTitle` provider (log-backed); resume loads persisted identity. Fork and resume are
  first-class, derived from the log.
- **jcode:** Session fork exists (`fork-prompt-session-*.json` artifacts in the repo root suggest
  recent fork work). Title generation is ad hoc.
- **Recommendation:** Make fork a single derived op on the event-sourced log (cheap given #3/#4)
  and make title generation a swappable provider.

**What this gives us.** Forking today means copying the session file and calling it a new session. If the fork needs a different project directory, different tools, or a different agent, the user has to figure out the composition manually. Making fork a derived operation on the event-sourced log means: (1) the child session header carries the fork boundary (which events are "mine" vs "parent's"), (2) the child can inherit the parent's session context through the projection seam rather than re-scanning the parent's log, (3) title generation becomes a single lookup against `ctx.sessionTitle` rather than a summary of the fork event.

## 17. Composable profiles + live patch reload

- **dsh:** A run is a plugin tree composed at boot from ordered layers (bundles → profile patch →
  home patch → `--patch` overlay). `cordis.patch.yml` targets a row by id and replaces its whole
  config. Custom profiles default to **live patch reload**.
- **jcode:** Config is file-based; runtime recomposition is limited.
- **Recommendation (lower priority for TUI):** Adopt a layered, id-addressable config-patch model
  so deployments can override one plugin/row without forking config. Relevant to the server/SDK
  surfaces more than the TUI.

**What this gives us.** Today changing one server behavior (e.g., "which tools does session X get?") requires editing a config file, restarting the server, and hoping no one is using it. A layered patch model means: a base config, a workspace patch that overrides one row, a user patch that overrides that. Patching row `tools/xyz` replaces just that one plugin's registration. This is table stakes for multi-tenant deployments (different tool sets per workspace) and it is already what dsh's `cordis.patch.yml` does. For jcode, this primarily matters on the server/SDK surfaces, not the TUI.

## 18. A postmortem culture for bug-class escapes

- **dsh:** `docs/postmortem/*` records bugs that reached a user/merge/release, focused on *why the
  process let it through*, each opening with a 30-second executive summary and linking the
  guardrails added. Four exist already (`acp-default-export-drops-inject`,
  `js-expression-disabled-filesystem-tools`, `web-agent-gui-feedback-loop`,
  `landlock-partial-notice-misclassified-child-failures`).
- **jcode:** No equivalent postmortem archive.
- **Recommendation (process, high ROI):** Start a `docs/postmortem/` (or `changelog/postmortem/`)
  for subtle/systemic escapes. jcode has had exactly these classes of bug (reflection-gate and
  config regressions have occurred). Writing them down converts one-off fixes into
  durable conventions.

**What this gives us.** When a config regression slips through to a release, the current practice is a git revert and a follow-up commit. The root cause ("why did this happen?") stays in the author's head. A postmortem says: one paragraph, 30 seconds to read, lists the exact root cause chain and every safety net that failed, and records the guardrails added. dsh has four: `acp-default-export-drops-inject` (a subtle ES module export bug), `js-expression-disabled-filesystem-tools` (a literal `!!js` that disabled a whole category of tools), `web-agent-gui-feedback-loop` (a web server validated the wrong replacement), `landlock-partial-notice-misclassified-child-failures` (a kernel permission model the team didn't fully understand). Each one added a test, a docs page, or a code-level guard. This is not about blame — it's about converting one-time fixes into institutional memory without requiring everyone to rediscover the same class of bug.

---

## Prioritized shortlist for jcode

| Priority | Takeaway | Why |
|---|---|---|
| P0 | #1 agent-loop-as-plugin + #3 invariants registry | Structural leverage; protects the event-sourced log |
| P0 | #4 projection seam + #13 event escape hatch | These are the *consumers* that make the event-sourced log worthwhile; design them into it from the start |
| P0 | #5 compaction as a designed, log-bracketed operation | Design the orphaned-bracket + tool-pairing-edge into the event-sourced log from the start, on top of the existing `ReplaceMessages`/`SetCompaction` scaffold |
| P1 | #7 repeat-tool reminder + per-call timeout | Tiny, no model call, kills the most common runaway loop |
| P1 | #2 + #8 execution-world seam + fail-closed sandbox | Directly addresses the #604 "ran `rm -rf ~`" class; enables remote sandbox |
| P1 | #12 branded IDs | Near-free Rust newtype, removes a real bug class |
| P2 | #9 subagent provider seam, #10 jobs seam, #11 durable inbox | Composition hygiene for swarm/telegram/review-rounds |
| P2 | #6 prune/summarize split, #14 waterfall hooks, #16 fork/title providers | Quality-of-design, incremental |
| P3 | #15 goals domain, #17 layered config patches, #18 postmortem culture | Lower urgency / process |

## What NOT to take

- **Cordis / HMR / live plugin reload.** jcode is Rust; the plugin runtime and hot-reload model
  do not transfer. Keep the *ideas* (swappable seams, layered patches) but not the mechanism.
- **Web/ACP/SDK application launchers.** jcode already has server/SDK/TUI surfaces; dsh's launcher
  taxonomy is not a gap.

## Sources

- **deepseek-harness** (shallow clone at `$JCODE_SCRATCH_DIR/deepseek-harness`):
  `docs/architecture.md`, `docs/capability-seams.md`, `docs/subsystems/core.md`,
  `docs/subsystems/session.md`, `docs/defensive-patterns.md`, `docs/postmortem/README.md`,
  `packages/compaction/compaction/README.md`, `packages/sandbox/sandbox/README.md`,
  `packages/guard/README.md`, `packages/subagent/subagent/README.md`,
  `packages/core/session`, `packages/core/agent` (interfaces referenced in the docs above).
- **jcode files cross-referenced**:
  - `crates/jcode-base/src/session/event_types.rs` — `SessionEventMap`, `SessionEventOp`,
    `SessionEvent` (the event-sourced session log scaffolding, in progress).
  - `crates/jcode-compaction-core/src/lib.rs` — token budget / emergency truncation.
  - `crates/jcode-command-risk/` — risk classification, path-based catastrophic deny.
  - `crates/jcode-agent-runtime/`, `crates/jcode-message-types/`, `crates/jcode-session-types/`.
  - `crates/jcode-background-types/`, `crates/jcode-swarm-core/`.
