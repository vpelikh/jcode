# Test execution time: investigation & options

Status: reviewed record (2026-09-06). Branch `jc/test-speed`.

This is the write-up for the question "can we improve tests execution time?".
It records what was measured, why the most obvious levers do *not* apply, and
which options are genuinely open. Caveat on the measurements: they were taken on
a heavily shared/overloaded machine (load average 50+; other worktrees' test
binaries idle on the box) and one run was interrupted by a server reload, so the
numbers below are directional, not definitive. Confirm the serial counts/timings
on a clean runner before acting on them.

## Scale

- ~8,200 `#[test]` fns across the workspace. The four largest crates alone hold
  most of them: `jcode-tui` 2453, `jcode-base` 1471, `jcode-app-core` 1017,
  `jcode-desktop2` 767 (these four sum to 5708; the remainder live in `src/`,
  `tests/`, and the many smaller crates).
- The dominant cost is **compilation/linking**, not test execution. From the
  rust-action log (`~/.jcode/logs/rust-actions.jsonl`), most single-filter runs
  were 12–60s of build dominating wall time; full lib-suite runs hit
  140–424 **seconds** (e.g. `test -p jcode-app-core --lib`).

## Measured baseline (this machine, `jcode-tui --lib`)

Ran the already-cached test binary directly, `--test-threads=1` (the config CI
uses):

```
running 2427 tests
test result: FAILED. 2406 passed; 3 failed; 18 ignored … finished in 162.68s
```

Three failures even at `--test-threads=1`:

1. `test_prompt_entry_shimmer_color_moves_across_positions` — already
   `--skip`-ped in CI (known flake).
2. `test_background_task_markdown_is_suppressed_even_if_role_was_lost`
3. `test_real_draw_click_on_body_anchored_image_label_cycles_level`

Both #2 and #3 **pass in isolation** (0.47s / 0.40s) but fail only in the full
serial run, i.e. they are order-dependent. Caveat: these two failures were
observed on a heavily loaded shared machine (where dozens of other worktrees'
test processes were idle), so they are *local evidence* that some process-global
state can be shared/reset across tests, not proof of a regression. Neither this
machine nor a clean CI runner isolated the exact mechanism; see the
reproducibility note below.

Reproducibility (2026-09-06): a second, full run of the CI-equivalent serial
command (mirroring CI's `--test-threads=1` with `--skip`
`test_prompt_entry_shimmer_color_moves_across_positions` and
`right_fact_stack_uses_neutral_gray_except_for_context_usage`) failed the
**same two tests at the same assertion lines** — `test_background_task_markdown_*`
and `test_real_draw_click_*` (2405 passed / 2 failed / 2 filtered, 166s). Both
observed full serial runs failed the same two tests at the same assertion lines.
That is only n=2, so "reproducible across two runs" is the accurate claim, not
"deterministic"; it is nonetheless consistent with cumulative prior-test state
rather than random noise, though a clean runner is still needed to confirm the
exact mechanism.

Note: the doc `docs/TUI_TEST_FLAKINESS.md` claims ~2006/2006 pass serially in
~12s, measured 2026-07-27 with a smaller suite. The suite has since grown to
2427 tests; the serial runtime measured on this box was ~163s.

## Why the obvious levers do NOT safely apply here

### 1. `cargo-nextest` / parallel per-crate execution
Backed by nextest being absent, the repo's `docs/TUI_TEST_FLAKINESS.md` (#592),
and `scripts/test_ci_suites.py`'s own rationale: the `lib-bins` suite defaults
to `--test-threads=1` not just for `jcode-tui` render state but because
"several tests exercise process-wide environment and server state" (a
cross-crate determinism concern). So parallel execution is not broadly safe
across the workspace, and the largest crate alone must stay serial. On top of
that, nextest's real win is parallel *execution*, which is exactly what these
constraints block; and compilation, not execution, is the dominant cost.

**Direct observation (2026-09-06):** running the built `jcode-tui` test binary
with the default parallel harness on this box **hung** — a whole block of
`tui::app::tests::*` tests stuck at "running for over 60 seconds" with ~0% CPU
for 5m34s before being killed, and no `test result:` was ever printed. Whether
this is a true shared-state deadlock or load-induced starvation cannot be
separated on this machine, but either way the parallel run did not complete and
produced no result — which is enough to conclude that `jcode-tui` parallel
execution is unreliable here and `--test-threads=1` should remain the default.
This is observed through the real harness, not inferred from inspection.

A second, bounded probe (`--test-threads=4`, hard-killed after 120s) also
**hung**: several `tui::app::tests::*` tests again stuck at "running for over 60
seconds" with no completion. Because even *partial* parallelism (4 threads) hung
rather than just slowing down, the failure is more consistent with
shared-state contention among concurrently-running tests than with simple
machine-load starvation (which would show sluggish but progressive completion),
though neither can be fully ruled out on this box. Either interpretation
supports keeping `--test-threads=1`.

### 2. Parallel `cargo test -p <crate>` in separate shells
Cargo serializes the compile phase across processes sharing one `target/`
(default; this repo has no `build.package-lock = false`), so the only gain is
the execution phase — again blocked for `jcode-tui`.

### 3. Precompiling integration binaries in `test_fast.sh`
Tried, then **reverted**. `test_fast.sh` runs under the `minimal` feature
profile (`JCODE_DEV_FEATURE_PROFILE=minimal`), while `test_e2e.sh` runs under
default features. A `minimal`-compiled `e2e`/`provider_matrix` binary is NOT
reused by `test_e2e.sh`; Cargo rebuilds it under default features. So the
precompile was a functional no-op and was removed.

### 4. The `minimal` feature profile works (positive validation)
The fast-test tooling's biggest compile-time lever is `test_fast.sh`'s default
`JCODE_DEV_FEATURE_PROFILE=minimal`, which passes `--no-default-features` so the
inner loop does not compile the heavy ONNX/embedding stack. This is verified:
`cargo tree -p jcode-tui --no-default-features -i tract-core` matches **no**
package (the entire `tract-core → jcode-embedding → jcode-base` chain is
excluded), whereas the same tree with default features lists `tract-core
v0.23.4` under `jcode-tui`. (An earlier observation of `tract_core` compiling
during a test run is not explained by the dependency graph under `minimal`; the
exact cause was not diagnosed, but the `minimal` profile itself correctly
excludes the stack.) No change needed here.

## Structural lever: the residual shared state is in `ui_frame_metrics.rs`

Checked the actual code, which corrects an earlier draft of this doc: the #592
render-state race is **partly already fixed**. The `ui.rs` render globals
(`last_max_scroll`, prompt positions, layout/status snapshots, copy targets,
flicker clear targets, etc.) are already **thread-local under `cfg(test)`**, with
the production atomics gated behind `cfg(not(test))`. The getters/setters
dispatch to the `TEST_*` thread-locals in test builds.

The residual process-global state in `crates/jcode-tui/src/tui/ui_frame_metrics.rs`
that serial tests can share is:

- `SLOW_FRAME_HISTORY`, `FLICKER_FRAME_HISTORY` (`OnceLock<Mutex<...>>`) — these
  have explicit `_for_tests()` clear functions and are written by the render path
  that tests drive.
- `FRAME_PERF_STATS`, `FRAME_RESOURCE_START` (`OnceLock<Mutex<...>>`), along with
  `frame_input_attribution_slot()` and `draw_call_history()` (per-call
  `OnceLock`s) — frame-perf pipeline state written during render.

Notes: `KEY_TO_PAINT_MS` and `PENDING_KEY_AT` are also process-global `Mutex`
statics in this module, but they are **not** touched by the test render path —
they are fed only by `note_key_event_read()`/`note_frame_painted()`, which fire
on real terminal key events that tests never send — so they are not a source of
test cross-contamination. They are listed here only for completeness.

Unlike `ui.rs`, these are **production** statics (some `OnceLock<Mutex>`, some
plain `Mutex`), not test-only. The module already documents the workaround
("never race two tests on the shared static history", `ui_frame_metrics.rs:1404`):
the tests that touch these histories are deliberately consolidated/coarse so
they do not race each other in the default (serial) harness.

One candidate mechanism is the flakiness doc's "flicker" layout shift:
`ui.rs`'s `clear_test_render_state_locked` calls
`frame_metrics::clear_flicker_frame_history_for_tests()` (ui.rs:1585), and that
history is one of these process-global `OnceLock<Mutex>` statics, so a clear from
one test could leak recorded state to a later render. That said, the exact root
cause of the two locally-observed failures is **undiagnosed**: test
`test_background_task_markdown_is_suppressed_even_if_role_was_lost` failed on a
`╭` box-drawing char appearing in a full-app frame where none was expected, which
the inline flicker-notice text does not obviously produce, and both tests are
timing/render sensitive on this load-average-50+ machine. A load-independent
controlled experiment also argued against the simplest flicker mechanism: running
`notification_spans_include_recent_flicker_warning_and_log_hint` immediately
before `test_background_task_markdown_is_suppressed_even_if_role_was_lost` in one
process (so a populated flicker history precedes the render) passed cleanly. Two
further negative results strengthen this: the *click* test
(`test_real_draw_click_on_body_anchored_image_label_cycles_level`) also passes
after the same flicker-notice test, and after a ~7-test preamble of
`create_test_app`-based state-clearing app tests, in one process. So simple
prior-test state leakage does not deterministically reproduce either failure.
Thus the `ui_frame_metrics` globals are *unconfirmed* shared state worth auditing
on a clean machine, not a proven cause of the observed flakes; the flakes may
equally be an interaction requiring a specific long prior test sequence under
load, or plain machine-load timing sensitivity.

This is why making everything thread-local is **not** a drop-in fix: these
frame metrics are part of the production data path (written by the render
thread, read by slower-frame/flicker diagnostics), so thread-locality could
change behavior for the real app, not just tests. A correct fix needs to decide
per-global whether thread-local is safe (single render thread ⇒ yes for most) or
whether producers/consumers cross threads.

Status: out of scope for this pass (correctness-sensitive, touches a production
data path, and cannot be validated on this overloaded shared machine). This is
the precise, code-level target to hand to a dedicated session, replacing the
vague "make render state thread-local" from the stale doc.

## Considered-but-not-shipped and housekeeping notes

- A `--no-run` precompile in `test_e2e.sh` was considered but is **not** worth
  adding: step 2 already runs a bare `cargo test`, which compiles the `e2e`
  integration binary before step 6 runs it. An explicit precompile would be a
  no-op, so the interactive flow already gets CI's cached compile for free.
- `docs/TUI_TEST_FLAKINESS.md` was stale (2006/2006, ~12s vs the measured 2427
  tests, ~163s serial on this box); this branch appended the current re-measured
  counts/behavior so future readers trust the serial claim.
- Do **not** add the two locally-reproduced flakes to the CI `--skip` list yet:
  they reproduced in both observed full serial runs on this loaded box but their
  behavior on a clean CI runner is unconfirmed (this work did not run on one).
  Only the shimmer test is confirmed flaky on CI and already skipped there. If
  they reproduce on a clean runner, that would justify investigating or skipping
  them; that was not
  established here.

## Options ranked by (speedup × safety)

| Option | Speedup | Risk | Effort | Status |
|--------|---------|------|--------|--------|
| Audit `ui_frame_metrics.rs` (and any other residual process-global state under test) on a clean machine to confirm/deny it causes the serial flakes, then fix and parallelize if confirmed | High if confirmed (execution wall time drops for the serial crate); zero if not | Med–High (correctness; touches production data path) | High | Recommended next; requires a clean machine to reproduce |

## Requirements → observed results (traceability)

| Requirement (from the request) | Concrete check | Observed result |
|---|---|---|
| "show me options" | Each option validated against code/harness | Delivered: options documented, each tested (below) |
| "do work in new worktree" | New git worktree + branch `jc/test-speed` | Delivered: worktree created on branch `jc/test-speed`, docs committed |
| Improve test-execution time | Run real `jcode-tui` suite; seek a speedup | **Not achieved** — no runtime improvement shipped |
| — option: nextest / parallel | Real parallel harness on built binary | **Hangs** (5m34s at ~0% CPU, killed, no result) → blocked |
| — option: partial parallel (threads=4) | Bounded probe, 120s hard kill | **Hangs** (tests stuck >60s) → more consistent with contention than load |
| — option: precompile in `test_fast.sh` | Cargo feature-profile analysis | No-op (minimal vs default mismatch), reverted |
| — option: precompile in `test_e2e.sh` | Step-2 `cargo test` already compiles e2e | No-op, documented |
| — option: `minimal` feature profile | `cargo tree` positive + negative control | Verified working (excludes `tract-core`) |
| Understand the residual 2-test serial flake | Serial baseline + isolated re-runs | Order-dependent, pass in isolation; **reproduced in both observed full serial runs on this box**; exact cause undiagnosed |

Every explicit requirement and claimed option in this doc maps to a concrete
check above with a stated observed result — including the two that failed. The
outcome that matters ("faster tests") was not achieved.

## Conclusion

The intended change (teach existing scripts to parallelize or precompile) turned
out to be a no-op or unsafe on this repo, which is itself a useful finding: the
fast-test tooling (`dev_cargo.sh`, `test_fast.sh`, `test_ci_suites.py`) is
already well-engineered, and compilation dominates. The open levers are (1)
understanding and eliminating the undiagnosed order-dependent flake (residual
process-global state is the leading candidate, mainly `ui_frame_metrics.rs`
production `OnceLock<Mutex>` globals, but the exact cause remains unconfirmed
and needs a clean machine to reproduce), and (2) the cross-crate determinism
constraint documented in `test_ci_suites.py`. Both need dedicated, isolated
follow-up rather than a script-level change. Everything else is marginal.
