# Postmortem: second route to protected-path deletion via `apply_patch` (#604)

**Summary.** `apply_patch` deletes files by absolute path, and
`ToolContext::resolve_path` passes absolute paths through unchanged — so a patch
could remove any file on disk (including protected ones) without touching the
bash tool or its gate. Review then exposed a policy hole *inside* the fix:
exact-match credential-store protection matched `~/.ssh` but not
`~/.ssh/id_ed25519`. Guardrail: the catastrophic tier now applies to
`apply_patch`, and credential stores are protected **recursively** while config
and document roots stay exact-match.

## What happened

The `#604` destructive-command gate protected `bash` commands. But `apply_patch`
is a *separate* tool that deletes by absolute path. `ToolContext::resolve_path`
passes absolute paths through unchanged, so a patch that removed a file did not
go through the bash tool or its gate at all. The safety model had a second,
independent deletion route the gate did not cover.

The fix itself then exposed a second-order policy hole: the protected home
subpaths were matched **exactly**. `~/.ssh` was protected, but
`~/.ssh/id_ed25519` — a file *inside* a credential store — was not, because the
exact match did not recurse. Testing the `apply_patch` route surfaced this
before it shipped.

## Root-cause chain

- `apply_patch` deletes by absolute path; `resolve_path` passes absolute paths
  through unchanged.
- The `#604` gate was scoped to the `bash` tool only; there was no equivalent
  catastrophic-tier check on `apply_patch`'s delete path.
- The catastrophic-tier path protection used exact matching, so paths *inside*
  a protected credential store (`~/.ssh`) were unprotected (`~/.ssh/id_ed25519`).

## Safety nets that failed (in order)

1. **Tool-scoped gating** — protection was added to `bash` but the identical
   risk existed on every other tool that can delete by path.
2. **Exact-match path policy** — matching a directory exactly does not protect
   its contents; the policy conflated "protect this dir" with "protect this
   exact path."

## Guardrails added

- **Catastrophic tier applied to `apply_patch`.** Only the `Catastrophic`
  (`Deny`) tier, deliberately: deleting ordinary files is `apply_patch`'s
  normal job, so the `Confirm`/reflection flow would be pure noise here. The
  two tools share the same protected-path policy; `apply_patch` does not get a
  second entry point.
- **Recursive protection for credential stores.** `~/.ssh`, `~/.gnupg`,
  `~/.aws`, `~/.kube`, `~/.docker` are now protected **recursively**, so keys
  inside them cannot be removed while the store root stays intact. Config and
  document roots remain exact-match so individual files inside `~/.config`
  stay editable.

## Why this is a distinct class

It is not only about `bash`. The `#604` gate taught the team how to protect a
*verb*; this postmortem extends the lesson to *every tool that can write or
delete by path*. Any tool that resolves to real paths must route its destructive
operations through the same catastrophic-tier policy, and path-safety must be
judged by what the path's *contents* mean (recursive for credential stores) not
by a literal path-equality match.

## Guardrail home

- `crates/jcode-app-core/src/tool/apply_patch.rs` (catastrophic tier on delete)
- `crates/jcode-command-risk/src/paths.rs` (`ProtectedPaths`,
  `SYSTEM_PATHS_PROTECTED_RECURSIVELY`, recursive credential-store protection)

## Related

- `docs/postmortem/destructive-command-gate-bypasses.md` — the parent class.
- `docs/postmortem/reflection-gate-background-dispatch-604.md` — the sibling
  class (a control-flow branch bypassing the same gate).
- `docs/SAFETY_SYSTEM.md` — where the operational safety posture lives.