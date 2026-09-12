# Session Handoff Plan

## Purpose

Preserve unfinished work across sessions without requiring the user to repeat
context or the agent to reread an entire transcript. When a session ends with
open todos, save a structured snapshot that a fresh session in the same project
can use on its first turn.

## Current scope

- Automatic snapshot capture during session disconnect cleanup.
- A project-keyed index for finding the latest unfinished handoff.
- First-message context injection, including image-first conversations.
- Promotion of a saved handoff into a durable project initiative.

Manual selection, snapshot pruning, and remote fallback are future work.

## Lifecycle

### Capture

`handoff::capture()` reads the session's todo plan, open todos, optional assistant
text, and attached initiative. Completed and cancelled todos are excluded. If no
open work remains, no new snapshot is created and that session's index entries
are retired. Its archived snapshot remains available for explicit promotion.

The disconnect hook schedules capture on the blocking pool after releasing the
global connection lock. It awaits persistence before returning, so completed
cleanup is a useful persistence boundary without blocking other clients from
attaching. Capture failures are logged rather than failing session cleanup.

### Resume context

A fresh conversation looks up its project's latest handoff before appending the
first visible user message. The rendered block is prepended to that message,
not added to the system prompt. Subsequent messages do not perform a handoff
lookup or repeat the context.

The block contains the intent, open work, optional assistant text, and linked
initiative. Rendering is capped at 8192 bytes, with a notice when the overall
block is truncated. Individual fields and the number of rendered todos are also
bounded. The original user text and image blocks are preserved.

### Promotion

`handoff::promote_to_initiative()` creates a project-scoped initiative using the
snapshot's intent, open todos, and optional assistant text, then records an
opening checkpoint. Creation and checkpoint errors are returned to the caller.
Handoffs and initiatives remain separate stores: snapshots represent session
state, while initiatives represent durable, curated goals.

## Project identity

Project keys use the git `origin` URL when available, otherwise the absolute
working-directory path:

- `git:<origin-url>` groups checkouts with the same origin.
- `path:<absolute-directory>` supports directories without an origin.

Origin is resolved on each lookup rather than cached for the process lifetime,
so repository initialization and origin changes take effect without restarting
the server. This costs a local git lookup during capture and first-message
injection. Later messages perform no handoff lookup.

Origin-based identity supports different checkout paths, but branches sharing
an origin also share a handoff bucket. Different URL spellings remain distinct.
Portability of the key does not itself transfer snapshot files between machines.

## Storage

Files live under `$JCODE_HOME/handoffs`, defaulting to `~/.jcode/handoffs`:

| File | Purpose |
| --- | --- |
| `<session_id>.json` | Per-session snapshot |
| `index.json` | Latest handoff per project, capped at 64 project entries |
| `.lock` | Cross-process serialization of store writes |

Session IDs are validated before use as filenames. Reserved names and invalid
IDs are rejected rather than rewritten into potentially colliding filenames.

A store lock serializes capture and index updates. Timestamp comparisons prevent
older writes from replacing newer entries. Moving a session between projects
removes its old project entries, and context rendering verifies the loaded
snapshot's identity and project before using it. An unreadable index is treated
as empty so a new capture can register itself.

The lock preserves the existing JSON layout without introducing a database or
migration. Unlike a process-only mutex, it coordinates multiple daemons, at the
cost of serializing writes. Snapshot and index files are written separately,
not as a crash-atomic multi-file transaction.

### Snapshot fields

| Field | Content |
| --- | --- |
| `session_id` | Source session identifier |
| `project_key` | Git-origin or absolute-path identity |
| `ended_at` | UTC capture timestamp |
| `disposition` | `closed`, `crashed`, or `reloading` |
| `working_dir` | Source working directory, when available |
| `intent` | User intention from the todo plan |
| `open_todos` | Item ID, content, status, group, and confidence |
| `last_assistant_text` | Optional assistant text, capped at 4096 bytes |
| `initiative_id` | Optional attached initiative identifier |

## Design rationale

Capture is mechanical rather than LLM-generated. It works without a provider
request, token cost, or model-dependent latency. The trade-off is incomplete
context when important decisions are absent from the todo plan and assistant
text. Full transcript summarization remains the memory extractor's responsibility.

Automatic injection uses only the latest unfinished handoff for the current
project. This keeps startup context small, but does not replace a future picker
for choosing among multiple work streams.

## Implementation map

| Component | Location |
| --- | --- |
| Capture, identity, index, rendering, promotion | `crates/jcode-base/src/handoff.rs` |
| Module registration | `crates/jcode-base/src/lib.rs` |
| Disconnect hook | `crates/jcode-app-core/src/server/client_disconnect_cleanup.rs` |
| First-message injection | `crates/jcode-app-core/src/agent/turn_execution.rs` |
| Storage and public-API tests | `crates/jcode-base/src/handoff_tests.rs` |
| Injection tests | `crates/jcode-app-core/src/agent_tests.rs` |
| Disconnect integration tests | `crates/jcode-app-core/src/server/client_disconnect_grace_tests.rs` |

## Testing

```bash
cargo test -p jcode-base --lib handoff::tests
cargo test -p jcode-app-core --lib first_user_message_injects_handoff_once
cargo test -p jcode-app-core --lib cleanup_persists_handoff_for_session_with_open_todos
```

Coverage includes capture and index persistence, concurrent writers, timestamp
ordering, terminal-todo retirement, cross-checkout portability, origin changes,
project isolation, invalid filenames, corrupt-index recovery, bounded rendering,
initiative promotion, text/image-first injection, and cleanup lock release.
Tests use temporary storage and restore the prior environment.

## Future work

- A `/handoff` picker and `/handoffres` command for manual selection.
- Snapshot pruning beyond the index's project-entry cap.
- Optional fallback after a failed live-session migration, using handoff files
  already available on the target host.

## Relationship to remote handoff

`REMOTE_HANDOFF.md` describes moving a live session's full state across hosts.
This feature records a lightweight snapshot at session close. The mechanisms
are independent. A saved snapshot may eventually provide a fallback when live
migration fails, but it is not a substitute for transferring live session state.
