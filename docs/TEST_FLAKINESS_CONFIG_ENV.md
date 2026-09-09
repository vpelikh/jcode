# Test flakiness: stale env-driven config via the config cache throttle

Status: fixed (2026-09-09), branch `research/test-flakiness`.

## Root cause

`jcode_base::config::config()` reloads its process-global `&'static Config` on a
throttle, `CONFIG_CACHE_CHECK_INTERVAL`:

```rust
const CONFIG_CACHE_CHECK_INTERVAL: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_millis(500)
};
```

`cfg!(test)` is only true when compiling `jcode-base`'s **own** test harness. When
`jcode-base` is a dependency of another crate's test binary (e.g. the
`jcode-app-core --lib` tests), it is compiled with `cfg!(test) == false`, so the
interval is **500ms**, not zero.

Within that 500ms window the `config()` fast path returns the previously loaded
config even when a tracked env var changed. Any test that sets a config env var
(`JCODE_WAKE_MODE`, `JCODE_ANIMATION_FPS`, `JCODE_HOME`, ...) and then
immediately reads `config()` observes the **stale** value.

Deterministic repro before the fix:

```
cargo test -p jcode-app-core --lib server::tests:: -- --test-threads=1
```

failed
`server::tests::external_background_task_wake_emits_request_without_starting_turn`
every run: `emit_external_wake` read `wake_mode=Internal` while the process env
carried `JCODE_WAKE_MODE=external`, so the wake fell through to a live turn (a
`KvCacheRequest`) instead of emitting `WakeRequested`. It passed in isolation
and only failed with sibling tests, i.e. order-dependence from config-cache
state, not a logic bug. Confirmed pre-existing on base master (the un-pushed
`server(session)` refactor was not the cause; its `emit_external_wake` logic is
unchanged).

## Fix

The env fingerprint is an in-memory scan of the process environment (no file
I/O), so compare it on **every** `config()` call, independently of the throttle.
Keep the throttle only for the config-file `fs::metadata` stat.

- `config()` computes `config_env_fingerprint()` up front and compares it to
  `ConfigCacheFingerprint.env` (the env snapshot already stored in the cache's
  `fingerprint`). If it changed, the fast-path throttle is bypassed and the
  config reloads immediately.
- The snapshot is taken only after `CONFIG_CACHE` is initialized
  (`LazyLock::force`). Config::load() can set env vars itself (e.g.
  copilot_premium -> JCODE_COPILOT_PREMIUM); snapshotting before init would
  miss those and spuriously reload on the very first `config()` call.
- After reloading, `fingerprint` is refreshed (which re-reads the env snapshot),
  so the next comparison sees the post-load environment.

Preserves the throttle's purpose (avoid re-statting config.toml on every read)
while making env-driven runtime config immediate, which is the correct behavior
for both tests and live process env overrides.

## Validation

- `cargo test -p jcode-app-core --lib -- --test-threads=1`: 1460 passed, 0 failed.
- `cargo test -p jcode-app-core --lib server:: --` (parallel): 459 passed.
- `cargo test -p jcode-base --lib`: 1557 passed, 0 failed.

## Second issue: HOME mutation race in bash gate tests

Three tests mutate the process `HOME` env var via `std::env::set_var` without
taking the shared test-env lock, so under the default parallel harness another
test that reads `HOME` can observe a mid-mutation value:

- `crates/jcode-app-core/src/tool/bash_tests.rs`:
  `bash_refuses_to_delete_the_home_directory`,
  `indirect_dispatch_paths_cannot_bypass_the_gate`
- `crates/jcode-app-core/src/tool/apply_patch_tests.rs`:
  `apply_patch_refuses_to_delete_a_protected_path` (same Issue #604 gate class)
- `crates/jcode-app-core/src/agent/provider.rs`:
  `resolve_working_dir_tests::tilde_expands_to_home`

Each now takes `crate::storage::lock_test_env()`, matching the established
convention used by every other HOME/`JCODE_HOME`-mutating test in the crate.
Validated: clean parallel runs of the affected modules and of
`cargo test -p jcode-app-core --lib server::tests:: -- --test-threads=1`.

Note: `jcode-base/src/auth/tests.rs` also mutates `HOME`/`JCODE_HOME`, but there
the mutation lives in a helper invoked only by tests that already hold
`lock_test_env()`, so no change was needed there.

## Residual known flake (not fixed)

`channel::tests::test_session_picker_menu_flow` intermittently fails under full
parallel load. The observed failure was at `assert_eq!(keyboard.len(), 1, ...)`
(line ~2852): the `/list` picker rendered more than one keyboard row, i.e. the
recent-session index returned extra sessions beyond the single one the test
inserts. (An earlier note that described this as "`/list` returns a non-empty
text reply" was inaccurate: that branch always returns `String::new()`.) The
exact leak mechanism is undiagnosed; it happens even though the test holds
`lock_test_env()`, and it passes in isolation and in a 3x module run, appearing
only in the ~1400-test full parallel suite under load, so it is best treated as
a low-probability cross-test recent-session-index state leak rather than a
deterministic bug. It was already catalogued in
`docs/research/deepseek-harness-implementation-status.md`.
