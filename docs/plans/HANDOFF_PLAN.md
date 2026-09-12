# Session Handoff Plan

Status: **Slice 1 implemented** (2026-09-10), under branch review (2026-09-11).
Automatic per-session handoff capture and boot injection are integration-tested.
Live TUI acceptance has not yet been demonstrated.

## Branch review (2026-09-12)

The review fixes supersede the earlier cache design and validation snapshots below.

| Finding | Fix / regression check |
| --- | --- |
| Concurrent index writers can lose projects | Cross-process store file lock serializes read/modify/write. `concurrent_writers_preserve_every_project` checks 24 simultaneous writers. |
| `index` session ID overwrites the index and lossy sanitization aliases IDs | Reject reserved/invalid IDs instead of rewriting them. `rejects_reserved_and_colliding_session_paths`. |
| Older writes replace the newest project handoff | Compare timestamps before replacement. `older_snapshot_cannot_replace_newer_project_entry`. |
| Completing a resumed session leaves stale automatic context | Retire only that session's index entries after confirmed terminal todos. `completed_recapture_retires_only_its_own_index_entry`. |
| Moving a session between projects can leak context | Remove old project entries and verify loaded snapshot identity/project. `moved_session_does_not_leak_context_to_old_project`. |
| Permanent origin cache survives git init/origin edits | Remove the cache. `project_key_observes_origin_changes_in_same_process`. Missing relative directories now get an absolute fallback, tested separately. |
| Every message performs unnecessary handoff reads | Gate lookup itself on the first visible message. Injection test covers text-first, image-first, and no repeated context. |
| Synchronous capture blocks the executor under the global connection lock | Run capture on the blocking pool after releasing the lock, await completion. Cleanup integration test deliberately holds the store lock and proves other clients can acquire the connection lock before persistence completes. |
| Unbounded handoff context expands the first model request | Bound rendered context to 8192 bytes with a truncation notice and cap stored assistant tail. `rendered_context_is_bounded_and_rejects_mismatched_snapshot`. |
| Promotion silently ignores checkpoint errors | Propagate the checkpoint error to the caller. Existing promotion workflow tests cover success. |
| Tests clobber `JCODE_HOME` and reuse fixed directories | Restore prior environment via RAII and use unique temporary projects. |

Trade-offs: a store file lock preserves the existing portable JSON layout without
introducing a database/migration, but serializes writes. A process-only mutex
would not protect multiple daemons. Resolving origin afresh costs a local git
lookup on first-message/capture calls, but avoids stale routing after repository
changes. A TTL cache was considered but would still misroute within its TTL and
add invalidation complexity. Later messages now perform no handoff lookup.
Awaiting blocking-pool capture makes cleanup completion meaningful for callers,
unlike fire-and-forget persistence, while keeping global client locks free.

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
  Its exact isolated rerun passed, suggesting order-dependent or shared-state
  interference. The root cause was not established in that pass, and the
  isolated pass is not evidence of a fully green full-suite run.
- Base and isolated picker reruns preserved Cargo exit status and both exited 0.
  The earlier full-suite shell pipeline masked Cargo failure with `tail` exit 0,
  so the test summary above, not that shell status, is authoritative.
- The current-channel binary reported `d71d290b8`, matching implementation HEAD,
  and the shared-server symlink resolved to that version. These checks verify
  installed artifacts, not a live process's executable or a fresh TUI workflow.

Public-API and integration coverage validates capture, index, portability,
module wiring, first-message injection and disconnect persistence. A fresh
interactive TUI end-to-end workflow was not exercised in this pass.

### Requirement-to-observation matrix

| Explicit requirement / changed output | Concrete check | Observed result |
| --- | --- | --- |
| Per-session JSON and project-keyed index | `capture_only_writes_when_open_todos_exist`, `full_workflow_capture_boot_render_promote` | Passed: open todo and intent survive capture, index resolves session, persisted snapshot can be promoted |
| Close hook alongside final extraction | `cleanup_persists_handoff_for_session_with_open_todos` | Passed through production cleanup: readable handoff persisted |
| Session-start context for current project | `first_user_message_injects_handoff_once`, `boot_context_is_present_for_fresh_session_with_handoff` | Passed: first message includes block, subsequent message does not, unrelated directory has no block. Implemented as first-user-message context, not system-prompt mutation |
| Module registration and dependencies | Full base suite plus compiled app-core integration tests | Passed compilation and public API resolution |
| Capture/index/portability tests | All 12 `handoff::tests` | Passed, including same-origin cross-path identity and different-origin separation |
| TUI build, plan, commit | Prior successful `selfdev build-reload`; installed binary version `d71d290b8`; committed plan | Build/reload tool reported restart, installed version matches implementation; not proof of running process identity |
| Terminal todo filtering | `open_filter_drops_completed_and_cancelled`, empty capture case | Passed: no snapshot for terminal-only or empty work |
| Corrupt index recovery | `corrupt_index_does_not_fail_capture` | Passed: new capture registers after corrupt index |
| Rendered context text | `render_boot_context_produces_block`, `extracts_last_assistant_text` | Passed: marker, intent, open item and extracted assistant tail match assertions |
| Attached initiative and promotion | `build_snapshot_records_attached_project_initiative`, `promote_to_initiative_creates_a_goal` | Passed: attachment ID retained and promoted project goal loadable |
| Cached project identity | Existing origin/path/portability and injection tests | Passed functional outputs; subprocess count and latency are not measured |
| Live TUI continuation | Actual `debug_socket tester:spawn` | Blocked: debug control disabled. No end-user completion claim |

The table maps the requested slice and its public outputs, including explicit
limits rather than treating blocked or unmeasured checks as successes.

### Acceptance follow-through and alternatives

On 2026-09-11, an actual `debug_socket tester:spawn` attempt was refused:
`Debug control is disabled. Set JCODE_DEBUG_CONTROL=1, enable
 display.debug_socket, or start the shared server from a self-dev session.`
Thus live TUI acceptance is externally blocked by server configuration. This
pass did not restart the user's shared daemon or claim a tester ran.

Three production-boundary tests were rerun individually with Cargo status
preserved, all passing: disconnect cleanup persisted a readable handoff,
first-message injection consumed it only once, and the public API workflow
captured, rendered from a second checkout, and promoted into an initiative.
These observations establish automatic carry-forward at the integration
boundaries rather than merely inspecting code. They do not establish a live
TUI or model's successful continuation of unfinished work.

Alternatives evaluated against the current design:

- **LLM summary at close:** offers richer decisions and rationale than a todo
  snapshot, but introduces a provider dependency, token cost and variable close
  latency. Mechanical capture wins for predictable offline persistence. Its
  cost is incomplete context when todos or the last assistant text omit facts.
- **Absolute-path-only identity:** simpler and avoids git lookup, but cannot
  satisfy the passing cross-checkout portability scenario. Origin-based keys
  retain that behavior at the cost of collapsing branches of the same origin
  and treating distinct URL spellings as distinct identities.
- **Uncached git lookup on every session:** sees origin changes immediately,
  unlike the chosen process cache. Caching avoids repeated subprocess work
  after warmup, at the cost of stale identity until restart after origin edits,
  per-directory memory growth and a still-synchronous cold lookup. Concurrent
  cold misses can duplicate lookups. No latency benchmark was performed, so
  this pass does not claim a measured speedup or exactly one subprocess.

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