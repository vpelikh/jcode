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
5. **Injected only at session start.** The handoff is boot context, injected into
   the system prompt's dynamic part only when the conversation is fresh
   (`message_count() == 0`), so it does not re-announce itself each turn.

## Slice 1 (implemented)

| Piece | Location |
| --- | --- |
| `handoff` module (capture, index, project identity, boot render) | `crates/jcode-base/src/handoff.rs` |
| Module registration | `crates/jcode-base/src/lib.rs` |
| Session-close hook | `crates/jcode-app-core/src/server/client_disconnect_cleanup.rs::cleanup_client_connection` |
| Session-start system-prompt injection | `crates/jcode-app-core/src/agent/prompting.rs::build_system_prompt_split` |
| Unit tests | `crates/jcode-base/src/handoff_tests.rs` |

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

## Slices 2+ (future)

- `promote to initiative` action on a handoff (bridge into `goal`).
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