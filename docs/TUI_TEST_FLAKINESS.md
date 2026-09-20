# jcode-tui test flakiness: root cause

`cargo test -p jcode-tui --lib` fails 1-4 tests per run, with a varying set.
This is a parallelism race on process-global state, not a logic bug.

## Evidence

- `cargo test -p jcode-tui --lib -- --test-threads=1` passes **2006/2006** (16 ignored).
  Re-measured 2026-09-06 (suite grown): 2427 ran; 2–3 order-dependent failures
  reproduced even at `--test-threads=1` on a heavily loaded shared machine, and
  the same failing tests pass in isolation.
- The failing set changes between runs at the default thread count.
- Individually, each failing test passes when run alone.

### Caveat (2026-09-06): how much of the render state is still shared

The root-cause prose below is older and reads as if all of the touched render
state is process-global. It is **not** all still shared: under `cfg(test)` the
layout snapshots, scroll positions, copy targets, and prompt positions now live
in `TEST_*` **thread-locals**, with the production atomics gated behind
`cfg(not(test))`. The part tests still genuinely share at process scope is in
`crates/jcode-tui/src/tui/ui_frame_metrics.rs` — the frame-perf, slow-frame, and
flicker histories (`OnceLock<Mutex>` statics). Details and the residual-state
question are in `docs/TEST_SPEED_INVESTIGATION.md`.

Two root-cause details below are also stale: (1) `clear_test_render_state_for_tests`
now takes the render-state lock (via `with_render_state_lock`, with per-thread
nested detection), so "clears it *without* taking the lock" no longer holds; and
(2) "~810 call sites" and the "959 `tui::app::tests::` tests" counts are from an
earlier suite size. The core point — that multiple tests mutate shared frame
state and can race on it — remains valid.

Counts were taken on 2026-07-27 and will drift as tests are added. Reproduce
on an otherwise idle machine: under memory pressure (this host has 15 GiB and
was running concurrent workspace builds) `cargo` gets SIGTERMed mid-compile,
which is a different failure from the race described here.

## Root cause

`create_test_app()` (and its `create_named_provider_test_app` sibling) in
`crates/jcode-tui/src/tui/app/tests/support_failover/part_01.rs` calls:

```rust
crate::tui::ui::clear_test_render_state_for_tests();
```

That wipes **process-global** render state: the flicker frame history, layout
snapshots, status-area snapshots, copy targets, and scroll positions.

Rendering tests guard exactly that state with `render_state_test_lock()`. But
`create_test_app` clears it *without* taking the lock, so any of its ~810 call
sites can reset a concurrently-running render test's state mid-assertion.

The mechanism for the most frequent victim
(`test_changelog_overlay_repeated_renders_are_stable`) is documented in
`clear_test_render_state_for_tests` itself: a recorded flicker event adds a
"⚠ flicker detected" notification line to later renders, shifting every
layout-sensitive assertion by a row.

### Bisected proof

Bisecting the 959 `tui::app::tests::` tests against the changelog test
identifies `test_tui_login_providers_have_real_tui_handlers`, which calls
`create_test_app()` in a loop (once per login provider). Running just those two
does not reproduce; the race needs enough concurrent load to interleave, which
is why it presents as order-dependent flakiness.

## What does not work

**Taking `render_state_test_lock` inside `create_test_app`.** This is correct
but serializes all ~810 call sites: suite runtime goes from ~12s to over 10
minutes. Measured, then reverted.

**Asserting a floor instead of an exact count** in the changelog test's
`buffered_samples` check, and **calling `clear_test_render_state_for_tests`**
at the top of that test. Both measured over 5 runs: the test still failed 5/5
with *and* without the change. Reverted rather than committed as churn.

## Suggested direction

The real fix is to stop sharing this state across tests rather than to
serialize access to it. Since the caveat above was written, part of this
direction has landed: the `ui.rs` render state (layout/status snapshots, scroll
positions, copy targets, prompt positions) is now **thread-local under
`cfg(test)`**. What remains process-global and shared across tests is the
`ui_frame_metrics.rs` frame-perf/slow-frame/flicker history, so the remaining
steps are:

1. Apply the same thread-local treatment to the residual `ui_frame_metrics`
   process-global histories under test, being careful that some are part of the
   production data path (written by the render thread, read by
   slower-frame/flicker diagnostics). See `docs/TEST_SPEED_INVESTIGATION.md`.
2. Alternatively, have `create_test_app` skip the render-state clear entirely.
   Only rendering tests depend on it, and they already clear it under the lock.
   This needs an audit of which app tests implicitly rely on the current clear.

Option 1 (thread-localize the residual `ui_frame_metrics` state) is preferred:
it removes the shared mutable state instead of adding coordination around it.

## Scope note

This is pre-existing and independent of the render-path performance work in
commits `0ba0154c6`, `2b8e78e34`, `8b44fc83b`, `8142f1a0b`. Verified by
stashing those changes and reproducing the same failure rate.
