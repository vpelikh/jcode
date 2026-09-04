# Review Rounds After Work Is Complete

Status: implemented (branch `jc/review-rounds`, worktree `.worktrees/review-rounds`)

## Implementation notes

Phases 1–6 are implemented and unit-tested on branch `jc/review-rounds`.

- Report-contract parser, lens definitions, fingerprint/stall helpers, and persisted
  state live in `crates/jcode-session-types/src/review.rs` (engine types) and the
  pure state machine in `crates/jcode-tui/src/tui/app/review_loop.rs` (with
  `review_loop_tests`).
- `AutoReviewConfig.loop_mode` / `max_stalled_turns` are in `jcode-config-types`.
- `Session.review_loop`, `SessionJournalMeta`, and the snapshot diff term are
  wired in `jcode-base`.
- Harness glue (per-lens reviewer spawn, child-session polling, `turn.rs` entry
  hook, `input.rs` turn-end followup arm, `/review-loop` command, double-review
  guard, mutual exclusion) is in `crates/jcode-tui/src/tui/app/commands_review.rs`.

The loop entry is gated behind `loop_mode` and the normal-TUI scope (the manual
`/review-loop` also runs in remote server-clients); only replay is excluded. The
final confirmation pass re-runs all six lenses against the final code state;
only an all-CLEAN final pass converges.

As of the default-on change (`jc/review-flow`), `AutoReviewConfig.enabled` and
`loop_mode` both default to `true`, so the review loop runs as part of the normal
flow without manual opt-in. The one-shot "double-check this turn's weak points"
digest still fires even while the loop is active (it is cheap and verifies the weak
points the agent's own assessments surfaced); delivering it first lets that lighter
verification run before the heavier per-lens loop steps. Auto-seeding is suppressed
only under the unit-test harness, which keeps todo-completion tests deterministic.

Once a session's todos are complete, jcode has nothing between "work done" and
"final response". The completion gates check *assessments*, not the *result*. We
want a second pass that reviews the finished work for bugs, minors, and overlooked
findings, and gets them fixed — so "all todos done" means "nothing left to fix in
scope".

Naive iterative review fails: "review again, anything missed?" makes the model
invent marginal findings to avoid saying "done". A bare `continue` loop
manufactures its own churn.

### Relationship to the RALPH loop

This review loop is a jcode-flavored instance of the *RALPH* (Ralph Wiggum) loop
pattern: evaluate finished work, feed the verdict back as a fix turn, and iterate
until a reviewer reports clean. Two deliberate differences from the canonical
RALPH loop keep it from being a strict match:

- **No context reset.** RALPH spawns a fresh agent instance each iteration to
  fight context collapse. Here reviewers are per-lens and read-only and reuse
  one reviewer window; fixes run in the *parent* session, not a fresh child.
- **"Memory" is the session.** RALPH persists state to files (specs + plan +
  git); jcode persists it in `Session.review_loop`.

So the outer naming and CLI stay "review loop" / `/review-loop`; RALPH is the
pattern this loop implements, not a rename target.

### Auto-entry is once per session

`maybe_enter_review_loop` seeds the loop at most once per session (guarded by
`session.review_loop.is_some()`), so a converged/stopped loop does not restart
after every turn — that would re-run all six lenses as churn. Re-running a
finished loop is a deliberate, manual action via `/review-loop start`. Because
the loop replaces one-shot autoreview when `loop_mode` is on, this is what
"ran automatically only once" means in practice.

## Overview

A **review loop** runs after the existing completion gates pass. It uses the
existing **read-only spawned review session** mechanism: for each of **6 lenses**
(correctness → edges/errors → security → performance → build/leftovers →
requirement-traceability), an independent read-only reviewer inspects the batch
diff, reports findings, and the **main session fixes** them; the loop re-reviews
until the reviewer reports clean. It stops on convergence (CLEAN), a churn cap, or
a finding-fingerprint stall. All existing `/review`, `/autoreview`, `/judge`,
`/autojudge` behavior is preserved; the loop is enabled by `loop_mode`.

## Terminology

- **review lens pass** = one spawned, read-only reviewer session for a **single
  lens**. Each lens gets its own independent reviewer (per-lens: 1 lens = 1 review).
- **round** = one lens pass + the fixes it yields. The reviewer runs once; the main
  session fixes the findings.
- **full review pass** = all 6 lens passes, each an independent spawned reviewer,
  run in order, plus a final confirmation pass.
- **`[autoreview] max_stalled_turns`** — churn cap: max consecutive rounds that
  report no new findings but do not converge before force-stop. Default 3; `0` =
  unlimited (rely on convergence + the finding-fingerprint guard).

## Reviewer (mechanism B, read-only, per-lens)

Each lens gets its own **independent spawned review session**, kept read-only by
its prompt guardrail (tool enforcement is not possible — `subagent`/`Task` is a
provider tool; the spawned reviewer relies on prompt guardrails, same as today).
Per-lens independence means lens N reviews on a clean slate, not biased by lens
N−1's findings.

Each lens reviewer is given **lean context**: the batch diff delta + prior findings
from other lenses, never the whole conversation.

**Report contract.** Each lens reviewer returns a **machine-parseable** report so
the harness can deterministically decide CLEAN vs findings and fingerprint them:

```
VERDICT: CLEAN | FINDINGS
FINDING: <severity>|<file>|<issue text>
FINDING: HIGH|<path>|<concise issue>
...one per finding...
```

`VERDICT` is a required token. This formalizes the existing "severity + file path +
`No issues found`" convention.

## The loop (harness-driven, per-lens, async)

The reviewer runs as a **separate child session/process**, so its results are not
synchronous with the parent turn. The harness captures findings by **reading the
child session's final message** (`Session::load(child)` → last message), not by
parsing the DM (the DM is for the user transcript only).

```
when all todos complete:
    enter review mode (persisted on the session)

for each lens in [correctness, edges, security, perf, build, traceability]:
    repeat until this lens is CLEAN or stall >= max_stalled_turns:
        1. spawn an independent reviewer for this lens (batch diff + prior findings)
        2. poll the child session each parent turn-end until it writes a VERDICT
           (no VERDICT yet -> wait, don't stall)
        3. parse the findings:
             CLEAN     -> lens done
             FINDINGS  -> fingerprint; advance the round
        4. queue a synthetic fix turn in the main session:
             "the reviewer found: <findings>. Fix them."
        5. main session fixes what it can. On cancel, pause/stop cleanly.
        6. can't-fix a finding -> not a stall; keep it open and surface in digest
        7. after fixes -> re-run THIS lens on the new code (post-fix re-check)

final confirmation pass:
    after all 6 lenses are CLEAN, re-run them once against the FINAL code state
    (at minimum any lens whose scope touches files changed since it last ran).
    Convergence = this final pass reports all CLEAN.
```

**Capture path.** `Session::load(child_id)` → its last message → parse `VERDICT`/
`FINDING` via the report contract.

## Convergence, stall, and the finding-fingerprint

- **CLEAN** (VERDICT: CLEAN on the final state) → convergence, stop. Never counts
  against the cap.
- **Stall** = a round reports no new findings but does not converge. Bounded by
  `max_stalled_turns`.
- **Productive** = a round with new findings, then fixed. Never counts against cap.
- **"can't fix"** (blocked) → not a stall. Only repeated identical findings
  increment the stall counter.

**Fingerprint compares the open-findings set, not raw per-round fingerprints.** Under
P6 the lens is re-run after a fix, so a re-run should report prior findings as
resolved. Comparing raw fingerprints round-to-round is wrong: a re-run that
re-raises a just-fixed finding would falsely register as "stuck," and a healthy
post-fix re-run usually changes the fingerprint anyway. So "stuck" means: across a
post-fix re-run, the **same still-open findings persist** (no net shrink) and the
reviewer adds nothing new — computed on the persisted open-findings set.
`VERDICT: CLEAN` after a fix is always convergence.

## Cross-lens invalidation (final confirmation pass)

Each lens reports CLEAN on a different snapshot of the code, and a later lens's fix
can change code an earlier clean lens already approved. So "all 6 lenses CLEAN in
order" does not guarantee the final state is clean in an earlier lens's concern.
The **final confirmation pass** (re-run the affected lenses once on the final
state) is therefore required, and only a full pass CLEAN on the final state counts
as convergence.

## User input and can't-fix

- A real **user message** mid-loop resets the stall counter and re-enters review
  against the new state (prior CLEAN no longer holds). Synthetic continuations
  don't count.
- A main session that **cannot fix** a finding (blocked, missing creds, out of
  scope) does not stall, but the finding stays open and is surfaced in the digest.

## Scope: only what this batch changed

Diff baseline = when the todos became complete. Only the batch changes are
reviewed.

## Digest + record

On convergence the user gets a digest: review/fix rounds, findings fixed,
"can't fix" items, files touched. Persist a per-session record (rounds, findings,
verdicts) that also feeds `session_search`.

## Persistence & resume

Review-loop progress persists on the session as `ReviewLoopState` (in
`jcode-session-types`, mirroring `SessionImproveMode`): `current_lens`,
`stall_turns`, `finished`, plus a reference to the per-session **review record**
where accumulated findings and fingerprints live.

Wiring this field touches:
- the session struct + peer struct + stub/snapshot copies (session.rs),
- crash.rs,
- `SessionJournalMeta` + a `prev.review_loop != current.review_loop` term in
  `metadata_requires_snapshot` (journal.rs),
- serde `default`/`skip_serializing_if` so existing sessions load tolerantly.

The review-record reference is required: without it, a resumed loop would re-raise
prior findings (breaking the finding-fingerprint guarantee). On reload/user-turn,
reload `ReviewLoopState` + the review record and re-enter at `current_lens` with
accumulated findings loaded. If the record is missing/incomplete, restart that
lens rather than pretend to resume.

## Terminal UX (option 2, locked)

- **Auto `/autoreview`** (`loop_mode`): each lens is its own fresh child review
  session (keeps per-lens independence, reuses the one-shot `client-input-<id>`
  startup), but auto **processes the 6 lens sessions in-process (headless)** via
  the existing `Agent`/`AmbientRunner` pattern — no 6-terminal spam. `loop_mode`
  suppresses the one-shot autoreview (loop replaces, not adds).
- **Manual `/review-loop`**: full per-lens independent windows for transparency.

## Command surface

- `/autoreview` = auto; one-shot per turn; with `loop_mode`, batch-scoped loop when
  todos complete.
- `/review` = manual, always one-shot.
- `/review-loop` = manual loop (mirrors `/improve`).
- `/judge` `/autojudge` untouched.

**Mutual exclusion.** Only one loop-mode (review-loop, improve, refactor) active per
session at a time; starting one clears/refuses the others. (Improve/refactor share
`improve_mode`; `review_loop` is a separate field, so this must be enforced.)

**Double-review guard.** The loop hooks `schedule_turn_end_followups`; the existing
one-shot autoreview hooks `maybe_trigger_autoreview_local`. When `loop_mode`, the
one-shot is suppressed so both don't fire on the same turn.

**Session scope.** Auto `/autoreview` and the auto loop run for normal TUI
sessions (server-client, including `is_remote`), matching the manual
`/review-loop` command which already spawned local reviewer windows there.
Only replay sessions are excluded (`is_replay`), and the unit-test harness is
excluded from auto-seeding for determinism.

## Ordering vs completion gates

gates pass → review loop → if review fixed files, re-run gates **once**; if still
failing, surface + stop (no ping-pong).

This re-run is implemented: when the loop converges and its `record.files_touched`
is non-empty (the review changed files after the gates first passed), it re-runs the
ownership and completion-confidence gates once against the post-fix state. A failing
gate in that one re-run surfaces a "review fixed files, but the completion assessment
now disagrees" message, records the same todo-gate telemetry the primary gate path
uses (Ownership / Completion / ConfidenceSpike), and stops; it never re-enters the
review loop, so there is no gates↔review ping-pong.

## Config (on `AutoReviewConfig`)

- `enabled` (existing; default changed to `true` so review runs by default)
- `model` (existing)
- `loop_mode` (new, default true, autoreview-only)
- `max_stalled_turns` (new, default 3, 0 = unlimited)

## Cost

Today `/autoreview` = 1 reviewer spawn. The loop multiplies (6 lenses + re-runs +
fix turns). The stall cap + fingerprint bound rounds, not cost. Recommended: a soft
per-pass budget (`max_review_turns` or unit token cap) that force-stops with a
"review stopped at budget" digest, mirroring `overnight`'s `overnight_poke_budget`.

## Explicitly deferred

- Separate review-only model.
- Full token/cost meter (a soft per-pass budget likely suffices).
- Harness-computed diffs (the reviewer has shell/git).

## Implementation plan

### Phase 1 — scaffolding + persistence

1. Add `review_loop: Option<ReviewLoopState>` to `Session` (session.rs),
   `SessionStartupStub`, and `SessionJournalMeta` (journal.rs).
2. Wire it through `journal_meta()`, `apply_journal_meta()`, the stub→session
   copy in `session_from_startup_stub()`, and the snapshot→session copy.
3. Add a `prev.review_loop != current.review_loop` term in
   `metadata_requires_snapshot()`.
4. Add serde `default`/`skip_serializing_if` so existing sessions load
   tolerantly.
5. Config scaffolding is already done: `AutoReviewConfig.loop_mode` and
   `max_stalled_turns` on the config side, `ReviewLoopState` in
   `jcode-session-types`, and a parsing test in `config_tests.rs`.

### Phase 2 — report-contract parser + fingerprint

1. Add a `ReviewReport` type in `jcode-review-rounds` (or a new small crate
   `jcode-review-types`) with variants `Clean` and `Findings(Vec<Finding>)`.
2. Parse the report contract from a reviewer's last message:
   - `VERDICT: CLEAN` → `Clean`
   - `VERDICT: FINDINGS` + one or more `FINDING: <sev>|<path>|<text>` →
     `Findings(vec![Finding { severity, path, text }])`
3. Implement a fingerprint function: `HashSet<(severity, path)>` derived from
   the open findings set. Compare two fingerprints by set equality — this is
   what determines "stall" (no net shrink across a post-fix re-run).
4. Unit tests for the parser (clean, findings, malformed) and fingerprint
   stability.

### Phase 3 — lens definitions + prompts

1. Define the 6 lenses as an enum or const array in the review module:
   `correctness`, `edges_errors`, `security`, `performance`,
   `build_leftovers`, `requirement_traceability`.
2. Write a prompt template per lens that includes: the batch diff, the lens
   focus, the report contract, and a reminder to only flag issues in the
   changed code.
3. Add a helper that assembles the per-lens prompt from the diff + prior
   findings.

### Phase 4 — review loop engine

1. Add a `spawn_reviewer(lens, diff, prior_findings) -> SessionId` helper
   that creates a read-only child session with the lens prompt.
2. Add a `poll_child_verdict(child_id) -> Option<ReviewReport>` helper that
   loads the child session via `Session::load()` and parses the last message.
3. Implement the loop state machine in a new module
   `crates/jcode-tui/src/tui/app/review_loop.rs`:
   - `enter_review_loop(app)` — called when todos complete and loop_mode is
     enabled; seeds `review_loop` on the session.
   - `step_review_loop(app)` — called from `schedule_turn_end_followups`;
     polls the active reviewer, processes the verdict, queues the fix turn,
     advances the lens, checks stall/convergence.
   - `resume_review_loop(app)` — called on reload/user-turn; re-reads
     `ReviewLoopState` + the review record and re-enters at `current_lens`.
4. The fix turn is a synthetic message injected into the parent session
   (`"the reviewer found: <findings>. Fix them."`).
5. After all 6 lenses are CLEAN, run the final confirmation pass (re-run
   affected lenses once on the final state).
6. On convergence or stall, emit a digest message and set
   `ReviewLoopState.finished = true`.

### Phase 5 — TUI hooks + command surface

1. In `maybe_trigger_autoreview_local()`, gate the one-shot path behind
   `!config.autoreview.loop_mode && !is_in_review_loop()`.
2. In `schedule_turn_end_followups()`, add a `review_loop` arm that calls
   `step_review_loop(app)` when the session is in an active review loop.
3. Add `handle_review_loop_command_local()` in `commands_review.rs` for
   `/review-loop` (manual, mirrors `/improve` — always spawns per-lens
   windows).
4. Add mutual-exclusion logic: starting a review loop clears `improve_mode`
   and vice versa; starting an improve/refactor mode refuses if a review
   loop is active.
5. Scope the auto loop to normal TUI sessions (matching manual `/review-loop`;
   exclude replay and the unit-test harness from auto-seeding).

### Phase 6 — digest + review record

1. Define a `ReviewRecord` type (rounds, findings, verdicts per lens,
   can't-fix items, files touched).
2. Persist the record on the session (new field on `Session`, reference in
   `ReviewLoopState`).
3. On convergence, format a digest message summarizing rounds, findings
   fixed, can't-fix items, and files touched.
4. Feed the review record into `session_search` so it appears in search
   results.

## Design decisions

- **Where should the review engine live?** Recommended: a dedicated
  `jcode-review-types` crate (report contract, `Finding`, review state). It
  keeps TUI/App code decoupled and reusable; dependency churn is bounded by
  semantic versioning. A module in `jcode-tui`/`jcode-app-core` is the
  fallback.
- **Prior-findings inclusion:** only include findings from *completed* lenses
  relevant to the current lens's scope; fetch them on-demand from the review
  record. Do not embed the full prior-findings set in every prompt.
- **"can't fix" findings:** surface as a dedicated digest section (e.g.,
  "Can't Fix Items"), not mixed into the regular review summary; also persist
  in the review record.
- **Minimum viable loop:** start with a 3-lens subset (`correctness`,
  `security`, `requirement-traceability`) for the MVP; 1-2 lenses is fine for
  early testing, then expand to all 6.
