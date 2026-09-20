# jcode postmortems

Status: active process practice
Adopted: 2026-09-09

A postmortem is a one-page account of a bug that reached a user, a merge, or a
release, written so the *next* occurrence of the same *class* is cheaper to
prevent. The point is not blame and it is not a changelog entry. The point is
turning a one-off fix into a durable convention: record why the process let the
bug through, what safety net caught it (or failed to), and the guardrail added
so the class does not silently recur.

## How to write one (30 seconds to read)

Every postmortem opens with a short executive summary — the bug in one or two
sentences, what it cost, and the one-line guardrail that now prevents it. It
then gives the exact root-cause chain and lists every safety net that failed,
in order, so a reader can see where the head escaped the defense-in-depth.
It closes with the concrete guardrails added and (when the class is systemic)
a pointer to the code that implements them.

Length is intentionally bounded. If a postmortem needs more than a page, the
class it describes probably needs its own design doc — link to that instead.

## Linking guardrails, not one-off fixes

Each postmortem names a stable, reviewable home for the guardrail it added.
That may be a crate (`crates/jcode-command-risk`), a named code path, a test
suite, or a documented invariant. The test or enforcement that would have
caught the class *before it shipped* is the most valuable guardrail of all.
When a guardrail is fully effective, a follow-up happening later and forcing
authors to rediscover the same class.

## What this archive is for

jcode has produced exactly these classes of escape — a destructive command
reaching the user's data (`#604`), and a safety gate with bypass routes that
only surfaced under automated review. They belonged in institutional memory
rather than in the head of the engineer who fixed them. This archive is that
memory: it does not replace code review or tests, it makes the *why* behind
existing guardrails discoverable.

## Index

- [destructive-command-gate-bypasses.md](./destructive-command-gate-bypasses.md)
  — `#604`: `rm -rf ~` reached a user's home; the reflection gate landed, then a
  review found and closed three more bypass route classes. Guardrail:
  `jcode-command-risk` blast-radius classify + reflection gate + catastrophic
  deny.
- [reflection-gate-background-dispatch-604.md](./reflection-gate-background-dispatch-604.md)
  — the `run_in_background` early-return path was the one place a destructive
  command could bypass the gate. Guardrail: gate lives *before* the background
  dispatch, pinned by a canary test.
- [protected-path-deletion-second-route-604.md](./protected-path-deletion-second-route-604.md)
  — `apply_patch` deleted by absolute path without touching the bash gate, and
  exact-match credential-store protection missed key files inside. Guardrails:
  catastrophic tier applied to `apply_patch`; credential stores protected
  recursively.

## Sources

- `crates/jcode-command-risk/` — the deterministic blast-radius classifier and
  the reflection gate (`src/gate.rs`).
- `crates/jcode-app-core/src/tool/bash.rs` + `bash_destructive_gate.rs` — the
  bash tool's gate wiring (#604).
- `crates/jcode-app-core/src/tool/apply_patch.rs` — the second protected-path
  route class.
- `changelog/v0.61.0.json` — release notes describing the reflection gate and
  the bypasses closed "found in review".

## Related

- [`docs/research/deepseek-harness-takeaways.md`](../research/deepseek-harness-takeaways.md)
  takeaway #18 proposes this archive (P3, but "high ROI / process").
- [`docs/SAFETY_SYSTEM.md`](../SAFETY_SYSTEM.md) is the operational safety
  runbook; the postmortems here explain *why* those guards exist.