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

- `ConfigCache` gained an `env_fingerprint: Vec<(String, String)>` field.
- `config()` computes `config_env_fingerprint()` up front and compares it to the
  cache's snapshot. If it changed, the fast-path throttle is bypassed and the
  config reloads immediately.
- After reloading, both `fingerprint` and `env_fingerprint` are refreshed.

Preserves the throttle's purpose (avoid re-statting config.toml on every read)
while making env-driven runtime config immediate, which is the correct behavior
for both tests and live process env overrides.

## Validation

- `cargo test -p jcode-app-core --lib -- --test-threads=1`: 1460 passed, 0 failed.
- `cargo test -p jcode-app-core --lib server:: --` (parallel): 459 passed.
- `cargo test -p jcode-base --lib`: 1557 passed, 0 failed.

## Second issue: HOME mutation race in bash gate tests

`crates/jcode-app-core/src/tool/bash_tests.rs` had two tests that mutate the
process `HOME` env var via `std::env::set_var` without taking the shared
test-env lock:

- `bash_refuses_to_delete_the_home_directory`
- `indirect_dispatch_paths_cannot_bypass_the_gate`

Under the default parallel harness another test can read `HOME` mid-mutation, so
the gate detects the wrong HOME and the test intermittently fails. Every other
HOME/`JCODE_HOME`-mutating test in the crate takes
`crate::storage::lock_test_env()`; these two were the exceptions. Fixed by adding
the lock to both, matching the established convention. Validated: 3 clean
parallel runs of `cargo test -p jcode-app-core --lib tool::bash::tests`.

## Residual known flake (not fixed)

`channel::tests::test_session_picker_menu_flow` intermittently fails under full
parallel load (`/list` returns a non-empty text reply), even though it holds
`lock_test_env()` and passes in isolation and in a 3x module run. It appears only
in the ~1400-test full parallel suite and is a low-probability load/timing flake
in the Telegram-mock/recent-session path, unrelated to the two bugs above. It was
already catalogued in `docs/research/deepseek-harness-implementation-status.md`.
