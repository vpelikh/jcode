# Session Handoff Plan

Status: **Slice 1 implemented** (2026-09-10). Automatic per-session handoff capture
and boot injection are live.

## Problem

Large plans (e.g. `SERVER_SERVICE_SPLIT_PLAN.md`) cannot be implemented in one
session. Today the user must open new sessions and re-explain context from
scratch each time. There is no automatic record of "where I stopped."

## Goal

When a session with unfinished work ends, capture a small structured snapshot
that a later session in the *same project* can boot from — without re-reading
the whole transcript or requiring the user to re-explain.

## Design decisions

1. **Mechanical, not LLM.** The snapshot is assembled from state that already
   exists: the `todo` plan (`user_intention`), the open todo list, and the tail
   of the transcript. It deliberately does *not* summarize the whole transcript;
   that is the memory extractor's heavier job. This keeps the close path cheap
   and non-blocking.
2. **Only when there is open work.** A session with no non-terminal todos has
   nothing to hand off and would only create noise. `capture()` returns `None`
   for such sessions.
3. **Keyed by portable project identity.** The handoff must survive a change of
   working-dir path (a different checkout directory, or a future remote
   handoff). We key by the git remote `origin` URL when available, falling back
   to the absolute working dir. This keeps the same project in one bucket across
   machines.
4. **Separate store from `goals/`.** Handoffs are transient per-session scratch;
   `initiative` goals are curated, durable, user-facing. A handoff can be
   promoted into a goal once it proves durable, but the stores stay separate to
   avoid polluting the curated workspace and to keep the write patterns (server
   close path vs. model tool) independent.
5. **Injected only on the first user message of a fresh conversation.** The
   handoff is prepended to the very first user message (when the conversation is
   empty), so it is seen on turn one and not re-announced on later turns. This
   must happen *before* the message is added to the session: `build_system_prompt_split`
   runs only after the first user message is already present, so gating on
   message count there can never see an empty conversation.

## Slice 1 (implemented)

| Piece | Location |
| --- | --- |
| `handoff` module (capture, index, project identity, boot render) | `crates/jcode-base/src/handoff.rs` |
| Module registration | `crates/jcode-base/src/lib.rs` |
| Session-close hook | `crates/jcode-app-core/src/server/client_disconnect_cleanup.rs::cleanup_client_connection` |
| First-message injection | `crates/jcode-app-core/src/agent/turn_execution.rs::append_user_context_message_with_display_role` |
| Unit + integration tests | `crates/jcode-base/src/handoff_tests.rs`, `crates/jcode-app-core/src/agent_tests.rs` |

### Storage

- Per-session snapshot: `~/.jcode/handoffs/<session_id>.json`
- Index (latest per project): `~/.jcode/handoffs/index.json`

### Snapshot shape

```json
{
  "session_id": "...",
  "project_key": "git:https://github.com/.../repo.git",
  "ended_at": "<utc>",
  "disposition": "closed | crashed | reloading",
  "working_dir": "...",
  "intent": "<TodoPlan.user_intention>",
  "open_todos": [{ "id", "content", "status", "group", "confidence" }],
  "last_assistant_text": "...",
  "initiative_id": null
}
```

## Validation

Behavior is exercised through the public `handoff` API with an isolated
`JCODE_HOME` (`crates/jcode-base/src/handoff_tests.rs`):

- **Main workflow end to end** (`full_workflow_capture_boot_render_promote`):
  capture on close → boot-render from a *different* checkout of the same repo →
  promote into a project-scoped initiative loadable from the other checkout.
- **Integration boundary / portability** (`same_git_origin_buckets_across_paths`):
  two checkouts with the same git origin land in one project bucket; a different
  origin differs.
- **Failure mode** (`corrupt_index_does_not_fail_capture`): a corrupt `index.json`
  resets cleanly and still records — it never panics or aborts the close path.
- **Edge cases**: no handoff when all todos are completed/cancelled; path-based
  fallback when git is absent; assistant-text tail extraction.
- **Injection** (`first_user_message_injects_handoff_once` in
  `crates/jcode-app-core/src/agent_tests.rs`): a fresh agent with a prior
  handoff for its working dir prepends the block to the first user message and
  does not re-inject on subsequent messages.
- **Close-path integration** (`cleanup_persists_handoff_for_session_with_open_todos`
  in `crates/jcode-app-core/src/server/client_disconnect_grace_tests.rs`): the
  real `cleanup_client_connection` close path persists a readable handoff.

### Final validation pass (2026-09-11)

Validated implementation commit `d71d290b8`:

- `cargo test -p jcode-base --lib`: 1567 passed, 0 failed, 2 ignored.
- Handoff-filtered tests: 12 base tests and 6 app-core tests passed. The latter
  includes the actual disconnect cleanup and first-message injection tests
  (and four unrelated reload/socket tests matching the filter).
- Full app-core run: 1468 passed, 1 failed, 24 ignored. The failure was
  `channel::tests::test_session_picker_menu_flow` (four rows instead of one).
  Its exact isolated rerun passed. This is a known order-dependent shared-state
  failure, not evidence of a fully green full-suite run or a random flake.
- Base and isolated picker reruns preserved Cargo exit status and both exited 0.
  The earlier full-suite shell pipeline masked Cargo failure with `tail` exit 0,
  so the test summary above, not that shell status, is authoritative.
- The current-channel binary reported `d71d290b8`, matching implementation HEAD,
  and the shared-server symlink resolved to that version. These checks verify
  installed artifacts, not a live process's executable or a fresh TUI workflow.

Public-API and integration coverage validates capture, index, portability,
module wiring, first-message injection and disconnect persistence. A fresh
interactive TUI end-to-end workflow was not exercised in this pass.

## Slices 2+ (future)

- ~~`promote to initiative` action on a handoff (bridge into `goal`)~~ — **done** (`handoff::promote_to_initiative`).
- `/handoff` picker for manual selection, plus `/handoffres` command in the TUI.
- Pruning policy (e.g. keep last N per project) beyond the index cap.
- Optional degraded-mode fallback for `REMOTE_HANDOFF.md`: read handoff files on
  the target host when migration/attach cannot complete, so a refusal becomes a
  soft landing ("migration failed, but here's your summary + next steps").
- A handoff promotes into `goals/` when it proves durable across sessions.

## Relationship to `REMOTE_HANDOFF.md`

`REMOTE_HANDOFF.md` moves a *live session's full state* (ownership lease,
transcript, tool state) across hosts. This handoff is a *lightweight summary
recorded at session close*. They are not substitutes. The designs are kept
independent; handoff is a possible future degraded-mode fallback for remote
handoff, never a building block of it.