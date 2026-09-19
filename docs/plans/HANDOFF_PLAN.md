# Session Handoff Plan

## Status

**Core functionality implemented and validated.** Capture, project indexing,
first-message injection, and initiative promotion are complete. The full
`jcode-app-core` suite passes (1469 tests), the TUI build and reload succeed,
and an isolated real daemon confirmed that disconnect persists pending work and
a fresh session's first message receives the saved handoff exactly once.
`jcode run` and captured turns are included in that first-message injection.
Live model continuation was exercised end to end with a working provider: a
fresh session recovered the exact pending marker from the saved handoff.

**Planned:** wire the new import/export portability into remote fallback. The
interactive `/handoff` overlay is now implemented, and the remote wire surface
now lets an SSH/remote client read and adopt the *server's* handoff store, so
remote-fallback adoption is wired through `import_handoff`. See "Future work"
for what remains. The wire request + server handlers + client methods and their
tests were implemented in 2026-09 on `feat/handoff-remote-fallback`.

**Regression gap closed (2026-09).** This branch re-ran the full `jcode-app-core`
suite to completion: **1540 passed, 0 failed** (the plan's earlier 1469 figure
predates this suite's growth). The new wire surface is additionally validated
end to end by a real-socket integration test that drives `Request::HandoffList`
and `Request::HandoffImport` through the client_lifecycle dispatch to the real
handlers — and then observes the runtime outcome, not just the index: the same
`render_boot_context_and_consume` function first-message injection uses returns
the imported snapshot's intent for a fresh session. The TUI `/handoff` overlay's
`HandoffListed` dispatch arm is covered by `handle_server_event` tests (opens the
picker from the server store when a request id matches; ignores stray events),
and the full `remote_tests` module plus the handoff and session-picker suites run
green. The full shipped `jcode`
binary also builds, links (all four wire tags present in the linked artifact),
and executes cleanly (`jcode --version`).

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
- A `/handoff` picker — an interactive overlay (the session picker generalized
  to a handoff data source) that lists saved handoffs (all snapshots, newest
  first) with arrow-key navigation, filtering, and a preview of each snapshot's
  open todos — plus a `/handoffres <session_id>` command that boots a fresh
  conversation from a selected handoff, overriding the automatic
  latest-for-project injection.

Manual selection is implemented. Snapshot pruning is implemented. Portability
(export/import for remote adoption) is implemented as the foundation for the
remote-fallback future work, which remains open.

## Lifecycle

### Manual selection

`handoff::list_saved_handoffs()` reads the index (newest first, one per
project) and `handoff::list_all_handoffs()` scans the snapshot directory so
archived handoffs that are no longer the latest for their project stay
selectable. Both feed `render_handoff(id)` for a specific snapshot's preview.

`/handoff` now opens an **interactive picker overlay**: the shared
`SessionPicker` is generalized to a handoff data source via
`SessionPicker::for_handoffs(snapshots)`. Each saved snapshot becomes a row
(title = intent, id, "closed <ago>" label, working dir), and its open todos +
trailing assistant text render in the preview pane, reusing the session picker's
list, arrow-key navigation, incremental filter/search, preview scroll, and mouse
support unchanged. `s/S` filter cycling, `d` test-toggle, `Space` multi-select,
and `T` Claude takeover are session-only and are disabled in handoff mode.
Selecting a row emits
`PickerResult::HandoffSelected(session_id)`. The selected snapshot's id is
carried (via a pending-handoff-resume on the App, drained on the async pump) to
the server via the `set_handoff_resume` protocol request and stored as a
transient, one-shot override on the agent.

`/handoffres` clears the current conversation in place (like `/clear`) so the
next message is the first visible one, then sets the override; always clearing
guarantees the override fires even right after a reconnect when the client's
display cache has not yet loaded server history. `/handoff-clear`
(alias `/handoffcancel`) sends `set_handoff_resume(None)` to restore automatic
injection. Selecting from the interactive `/handoff` overlay performs the same
`/handoffres` sequence (clear + set override) through the async pump. While
disconnected, `/handoff` still opens the overlay from the shared local handoff
store; resume then needs a connected server, and the commands degrade
gracefully.

Note on SSH-backed sessions: the `/handoff` overlay is unavailable over SSH
because it lists the client host's local handoff store, which is the wrong host
for an SSH-backed session (resuming sends the id to the server-side store). It
shows an "unavailable in SSH mode" notice instead. `/handoffres <id>` still works
over SSH when the user knows an id that exists on the server's store, since it
routes through the remote connection. Reading the server-side handoff store from
the client is out of scope; run `jcode` on the host to list its handoffs.

At first-message injection ([`render_first_message_handoff`]), the override is
honored if present and consumed immediately; otherwise the default
latest-for-project handoff is used. Because the override applies only to the
first visible message and is cleared after injection, manual selection never
regresses the automatic behavior on a later turn or session.

Automatic latest-for-project injection is itself one-shot: the injected handoff
is retired from the index once it is rendered, so a *later* fresh session in the
same project no longer re-injects the same stale snapshot (previously every new
session kept inheriting the old handoff forever). The retired snapshot's file is
kept, so explicit manual resume via `/handoffres` and the `/handoff` picker
still work; if the resumed session itself produces open work, its own close
captures a fresh handoff.

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

### Pruning

`handoff::prune_archived_snapshots()` enforces a retention policy over the
archived snapshot files, which the index's `MAX_INDEX_ENTRIES` cap alone does not
cover. It runs after every successful `handoff::capture` write, and
`handoff::sweep_stale_handoffs()` runs the same sweep once at host (server)
startup so stale files do not linger between captures.

- Per project, at most `MAX_ARCHIVED_SNAPSHOTS_PER_PROJECT` (16) archived
  snapshots are kept; older ones are deleted oldest-first.
- Archived snapshots older than `MAX_ARCHIVED_SNAPSHOT_AGE_DAYS` (30) are
  deleted regardless of count.

Live handoffs — the latest per project that the index still references — are
never pruned, even if old. Only superseded (archived) snapshots are eligible.
Pruning is best-effort: unreadable or missing files are ignored and a missing
store is a no-op, so it never breaks capture. Each removal (and a per-run
summary) is logged.

### Portability (export / import)

`handoff::export_handoff(id)` serializes a saved snapshot into a portable JSON
payload, and `handoff::import_handoff(payload, working_dir, disposition)`
adopts one on another host. This is the foundation for the remote-fallback
future work, letting a captured snapshot be shipped to a target host and become
the live handoff for that host's project.

Import is intentionally explicit and retirement-safe: it rekeys the snapshot to
the caller's working-directory project and *explicitly* registers it with the
project in the index. A blanket on-disk scan was rejected precisely because it
cannot distinguish a genuinely-remote handoff file from a *retired* local
snapshot (whose file must never reinject). Because import mints a new id and
deliberately indexes the snapshot, it never resurrects a retired source session.

The adopted session id is human-meaningful — `import-<source-session>`, with a
short disambiguating suffix only on collision — rather than an opaque UUID, so
`/handoffres <id>` stays recognizable. Pathological source ids (uppercase, dots,
slashes) are sanitized into a valid, bounded filename stem.

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
| Capture, identity, index, rendering, promotion, listing, specific render | `crates/jcode-base/src/handoff.rs` |
| Module registration | `crates/jcode-base/src/lib.rs` |
| Disconnect hook | `crates/jcode-app-core/src/server/client_disconnect_cleanup.rs` |
| First-message injection and manual override | `crates/jcode-app-core/src/agent/turn_execution.rs` |
| `set_handoff_resume` protocol + server handler | `crates/jcode-protocol/src/wire.rs`, `crates/jcode-app-core/src/server/client_actions.rs`, `client_lifecycle.rs` |
| TUI `/handoff`, `/handoffres`, `/handoff-clear` commands + interactive handoff picker overlay | `crates/jcode-tui/src/tui/session_picker.rs`, `session_picker_tests.rs` (handoff data source), `crates/jcode-tui/src/tui/app/inline_interactive.rs` (overlay open/routing/pending resume), `crates/jcode-tui/src/tui/app/remote/key_handling.rs`, `commands.rs`, `remote.rs` (async drain) |
| Storage and public-API tests | `crates/jcode-base/src/handoff_tests.rs` |
| Injection + override tests | `crates/jcode-app-core/src/agent_tests.rs` |
| Disconnect integration tests | `crates/jcode-app-core/src/server/client_disconnect_grace_tests.rs` |

## Testing

```bash
cargo test -p jcode-base --lib handoff::tests
cargo test -p jcode-app-core --lib manual_handoff_override_injects_selected_snapshot_once
cargo test -p jcode-app-core --lib stale_manual_handoff_override_falls_back_to_auto_inject
cargo test -p jcode-app-core --lib handle_set_handoff_resume_overrides_auto_inject_and_errors_on_unknown
cargo test -p jcode-app-core --lib handle_set_handoff_resume_none_clears_override
cargo test -p jcode-app-core --lib first_user_message_injects_handoff_once
cargo test -p jcode-app-core --lib cleanup_persists_handoff_for_session_with_open_todos
cargo test -p jcode-tui --lib local_handoff_listing
```

Coverage includes capture and index persistence, concurrent writers, timestamp
ordering, terminal-todo retirement, cross-checkout portability, origin changes,
project isolation, invalid filenames, corrupt-index recovery, bounded rendering,
initiative promotion, text/image-first injection, cleanup lock release, picker
listing (latest per project, newest first), archive listing (`list_all_handoffs`
surfaces superseded snapshots), specific-snapshot rendering, manual override
beating auto-inject, a stale-override fallback (a retired snapshot falls back to
auto-inject instead of booting context-less, and the stale id is consumed), the
`set_handoff_resume` server handler (valid set replies `Done` and wins over
auto-inject; unknown id replies `Error`; `None` restores auto-injection), a
no-regression guard that a manual selection does not disturb the default, the
interactive `/handoff` overlay (opens in handoff mode over the saved store,
newest first with archived snapshots visible, recency sorted) and that
`/handoffres` explains a server is needed locally, snapshot pruning
(per-project archived count
cap, archived age cap, live handoffs never pruned, and per-project scoping),
and portability (export/import round trip adopts a remote snapshot and makes it
injectable, malformed payloads are rejected, and import mints a fresh id so a
retired source is never resurrected). Tests use temporary storage and restore
the prior environment.

## Future work

- **Remote wire surface (landed 2026-09).** `import_handoff` is now reachable
  over the wire: `Request::HandoffImport` adopts a portable payload into the
  server's store as the live handoff for the session's project, and it is the
  entry point a remote-fallback flow uses after a failed live-session migration
  (consume the snapshot already on the target host, or ship one from the client).
  `Request::HandoffList` plus `ServerEvent::HandoffListed` expose the server-side
  store so a client can discover what the *target host* has saved. The remote
  `/handoff` overlay is fed from that listing over SSH. A combined atomic
  "clear + adopt + resume" apply request would be the natural next step and is
  listed below.

- **Picker row model (landed 2026-09).** The interactive overlay's handoff
  surface is decoupled from the session-shaped one. `session_picker.rs` now
  backs the flat list with a thin, picker-agnostic [`Row`] enum:
  `Row::Session(SessionInfo)` for live/persisted sessions and
  `Row::Handoff(HandoffModel)` for saved handoffs. `HandoffModel` keeps the
  real [`HandoffSnapshot`] (`disposition`, `project_key`, `initiative_id`, open
  todos) alongside the `SessionInfo`-shaped display projection, so handoff rows
  carry real fields instead of dummy session ones. The previous `handoff_mode`
  bool flag is gone: the picker derives `is_handoff()` from the backing row
  type, Enter/selection route off the snapshot's own `session_id`, and the
  session-only keys (`s`/`S`/`d`/`T`/`Space`) are no-ops for `Row::Handoff`
  rows. Behavior is byte-identical (the picker, review, and handoff suites
  still pass unchanged).

  Two deliberate bounds keep this refactor contained, recorded here so they are
  explicit decisions rather than unstated gaps:
  - **`ServerGroup.sessions` stays `Vec<SessionInfo>`.** Handoff rows never
    route through server groups — only the flat (`all_sessions` /
    `all_orphan_sessions`) store can hold a `Row::Handoff`. Re-typing the
    group's session list to `Vec<Row>` would be a public API change in the
    `jcode-tui-session-picker` crate that forces every group access through a
    `row.session()` projection for no correctness gain. `row_by_ref` therefore
    returns `None` for `SessionRef::Group` (a group can never be a handoff row).
  - **Public constructors (`new`/`new_grouped`/`reseed_grouped`) still take
    sessions, not rows.** They wrap `Row::Session` internally; only
    `for_handoffs` produces `Row::Handoff`. This keeps the session-facing API
    and wire/CLI behavior byte-identical. A caller cannot hand a prebuilt
    handoff row through the public constructors today, which is fine because
    the handoff overlay is the only producer of handoff rows.

  `is_handoff()` derives from a representative flat/orphan backing row (the
  data source is homogeneous by construction — constructors/reseed never mix
  or partially append), so it is O(1) on the per-frame render path yet robust
  to an emptied visible list.

- **Row-model unification (follow-up, deferred).** A fully-uniform `Vec<Row>`
  pipeline (groups included) is feasible but requires decoupling the
  serializable `HandoffSnapshot` / `HandoffTodo` shapes out of `jcode-base`
  into `jcode-session-types`, since `ServerGroup` lives in the leaf
  `jcode-tui-session-picker` crate that cannot depend on `jcode-base`
  (that would create a cycle with `jcode-app-core`). Doing so would let
  `ServerGroup.sessions` become `Vec<Row>` and would let `Row`/`HandoffModel`
  move into the picker crate. Payoff is architectural only — one accessor
  path, no `Group`-arm special case, a self-contained picker crate, and the
  ability to group handoffs by project — with **zero user-visible behavior
  change**. It touches the serialization/wire surface and re-types a public
  struct across `loading.rs`, `memory.rs`, `filter.rs`, and test literals, so
  it is deliberately out of scope here. Optional; pursue only if grouped
  handoffs or a non-app-core picker consumer becomes a real need.

- **Atomic handoff apply (landed 2026-09).** `Request::HandoffApply { id, payload,
  disposition }` adopts a portable payload and boots the session from it in a
  single server-side hop: `handle_handoff_apply` imports the payload first
  (rejecting malformed input with `Error` and leaving the live conversation
  untouched), then clears the current conversation and sets the handoff-resume
  override to the adopted id so the next first user message boots from the
  snapshot. This removes the previous non-atomic `clear()` + `set_handoff_resume`
  two-request split at the client, which could leave a cleared conversation with
  no override (auto-inject wins). Covered by a unit test
  (`handle_handoff_apply_imports_clears_and_arms_resume`), a wire roundtrip, and
  the existing real-socket handoff surface. Independent of the row model.

- **Wire-model duplication (follow-up, deferred).** `HandoffWireModel` and the
  session-shaped `Row`/display projection carry overlapping session fields.
  `end`, `archived`, `created_at`, and `name` are kept in sync by `wire.rs`'s
  `ui_projection` equivalents. Unifying them (render off the wire model directly,
  or type the wire surface out of the same shapes) is feasible but touches the
  session-picker's public API and its serialization surface for zero
  user-visible behavior change. Optional; pursue only if the duplication starts
  to drift or a non-app-core consumer needs the wire shape.

- **Payload-per-row (follow-up, deferred).** Each `HandoffListed` row carries its
  full export `payload`. This is convenient for one-round-trip re-adoption but
  means every listing ships the complete snapshot body even when the client only
  needs discovery. An on-demand `HandoffExport { id }` request (returning the
  payload only for a chosen row) would keep listings lean, at the cost of an
  extra round trip per adoption. Independent of the row model.

- **SSH handoff discovery is wired (landed 2026-09).** Over SSH the `/handoff`
  overlay is now fed from the connected *server's* handoff store via
  `handoff_list`/`HandoffListed` instead of the client host's local store (the
  wrong host for an SSH-backed session). `HandoffImport`/`HandoffImported`
  exposes adoption of a portable payload server-side, and `/handoffres <id>`
  works for any server-side id. The `payload` field on each `HandoffListed`
  row lets the client re-adopt a snapshot with a single round trip.

- **Standalone `--resume` has no handoff picker.** The interactive overlay is
  TUI-only; `jcode --resume` treats a handoff selection as an inert no-op. Adding
  handoff selection to the standalone resume CLI is a separate feature.

These are *feature/protocol* follow-ups (wire surface + commands), orthogonal to
the row-model abstraction above. Each is shippable independently on the current
architecture.

## Relationship to remote handoff

`REMOTE_HANDOFF.md` describes moving a live session's full state across hosts.
This feature records a lightweight snapshot at session close. The mechanisms
are independent. A saved snapshot may eventually provide a fallback when live
migration fails, but it is not a substitute for transferring live session state.
