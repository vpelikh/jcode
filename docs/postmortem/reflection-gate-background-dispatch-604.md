# Postmortem: reflection gate skipped on background dispatch (#604)

**Summary.** The `bash` tool's `run_in_background` path returned *before* the
rest of `execute`, which made it the one route a destructive command could take
that bypassed the newly-landed gate. Guardrail: the destructive-command gate now
runs **before** the background-dispatch early return, pinned by a canary test
that asserts a backgrounded `rm` is refused.

## What happened

As part of `#604` the `bash` tool gained a destructive-command gate. The gate's
position in `execute` mattered: `run_in_background` had an early return that
exited the function before the main body ran. The gate sat *after* that return,
so a command that specified `run_in_background: true` reached the process
without ever being assessed.

This is exactly the class of escape-route bug that the `#604` review was
hunting — a single control-flow branch that silently short-circuits a safety
gate. It was found and pinned before it shipped.

## Root-cause chain

- The destructive-command gate was added to `ToolRegistry::execute`.
- `bash`'s `execute` contains an early-return path for background dispatch
  (`run_in_background`), which returns before the main body.
- The gate was wired after that path, so backgrounded commands skipped it.
- No test covered the combination of `run_in_background` + a destructive
  command — the one code path in `execute` that returns before the main body.

## Safety nets that failed (in order)

1. **Positioning discipline** — the gate's author did not enumerate every
   early-return path in `execute` before choosing where the gate sat.
2. **Test coverage** — the background path was the single path that returned
   before the main body, and that is precisely the one not covered.

## Guardrail added

- The gate now executes **at the top of `execute`, before the
  `run_in_background` early return**, so every control-flow branch is gated.
- A canary test (`test(bash): pin that background dispatch is gated too`) uses a
  file system canary to assert a backgrounded destructive command is refused.

## Why this is worth a postmortem of its own

It is small, but it is a distinct *class*: a safety gate is only as strong as
its position relative to every early return / alternative control flow. Listing
it separately makes the convention concrete — when adding a gate, enumerate all
paths that leave `execute` early and assert each one is covered.

## Guardrail home

- `crates/jcode-app-core/src/tool/bash.rs` (gate before background dispatch)
- `crates/jcode-app-core/src/tool/bash_tests.rs` (background canary test)

## Related

- `docs/postmortem/destructive-command-gate-bypasses.md` — the parent class
  (wrapper/pipeline/redirect escape routes).
- `docs/postmortem/protected-path-deletion-second-route-604.md` — the sibling
  class (independent deletion path in `apply_patch`).