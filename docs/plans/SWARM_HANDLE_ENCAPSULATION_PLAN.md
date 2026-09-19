# Swarm Handle Encapsulation Plan (Tier 3)

Status: Plan for the Tier 3 "true encapsulation" follow-up flagged by
`SERVER_SERVICE_SPLIT_PLAN.md`. Tier 1 (mechanical convergence onto
`&SwarmServiceHandle`) is landed; this documents how to *close* the split by
privatizing the handle's fields so all mutations must go through handle
methods. **All seven slices are delivered (2026-09); the field boundary is
closed, but the "all mutations route through handle methods" done-criterion is
only partially met — the entangled orchestration mutations remain (see the
Done criteria status block).** The optional stricter `SwarmState` sub-field
privatization is also out of scope (see the `swarm_state` slice note).

Scope: `crates/jcode-app-core/src/server/services/swarm.rs` +
`crates/jcode-app-core/src/server/**`.

## Why

Tier 1 delivered typed handles and narrowed signatures, but the handle's
fields are still `pub(crate)`, so callers can reach into
`swarm.swarm_state.members` / `swarm.event_history` / etc. directly. That
keeps the "service boundary" aspirational: there is no compile-time guarantee
that a mutation goes through a handle method. This plan makes the boundary
real by privatizing the fields and exposing narrow read accessors + mutation
methods.

## Current field exposure (measured 2026-09)

`SwarmServiceHandle` fields are `pub(crate)`:

| Field | Direct field accesses | Notes |
|---|---|---|
| `swarm_state` (`SwarmState`) | 169 | biggest; members/plans/coordinators/swarms_by_id |
| `shared_context` | 12 | leaf map |
| `file_touch` | 5 | already an encapsulated `FileTouchService` |
| `channel_subscriptions{,_by_session}` | 10 | reverse indexes |
| `event_history` | 16 | all read-only bindings fed into `record_swarm_event` |
| `event_counter` | 14 | atomic counter, read-accessor friendly |
| `swarm_event_tx` | 16 | broadcast sender, read-accessor friendly |
| `await_members_runtime` | 1 | runtime handle |
| `swarm_mutation_runtime` | 7 | runtime handle |

Most direct accesses are **read-only local bindings** (e.g.
`let members = &swarm.swarm_state.members;`) that feed domain free functions.
Real mutation funnel: the domain functions in `state.rs` / `swarm.rs`
(`record_swarm_event`, `update_member_status`, `remove_session_from_swarm`,
etc.), which the handle methods already wrap.

## Strategy

Do **not** convert `state.rs`/`swarm.rs` free functions (they are the domain
API the handle wraps). Instead:

1. **Privatize fields one at a time** (smallest fanout first), adding a narrow
   read accessor method and routing every remaining mutation through a handle
   method. Each field = one reviewable slice, green after each.
2. **Read accessors are narrow & returning `Arc` clones** (or `&self` refs)
   for the handful of legitimate read sites (debug snapshots, broadcast
   rebuild). Mutations return `()` / small results via methods.
3. **The orchestration-heavy subscribe/resume paths** (`handle_subscribe`,
   `handle_resume_session`, `cleanup_detached_source_session_if_unused`,
   `handle_set_feature`) are the risk concentration: their mutations are
   interleaved with session logic and span several maps. Sequence them last,
   slice by slice, keeping borrow order identical.

## Slice sequencing (each green + committed)

1. **`event_counter` / `event_history` / `swarm_event_tx` trio.** Writes already
   funnel through `swarm::record_swarm_event`; direct accesses are read-only
   bindings. Privatize with a `swarm.read_event_sources()` accessor (returns
   the three refs) or per-field accessors, and reroute the ~46 local bindings.
   Lowest risk, high fanout-handled. *(landed 2026-09)*
   The three fields are now private. `read_event_sources()` returns a borrowed
   `EventSources` tuple (history / counter / sender). The `monitor_bus`
   file-touch path was rerouted onto `swarm.record_swarm_event(...)` (dropping
   the three owned source clones). `TestSwarmBuilder` seeds the sinks via a
   `#[cfg(test)] test_with_state` constructor + `with_event_sources` builder
   method (private fields are otherwise unconstructable from outside the
   services module). All 14 contiguous trio bindings + 8 special sites were
   converted; zero direct cross-module field access remains.
2. **`shared_context`** (12). One leaf map; add `read_shared_context()` +
   mutation-method moves. *(landed 2026-09)*
   Field is now private. Writes route through `set_shared_context(...)`
   (carrying the upsert-with-append logic and created_at preservation) and
   `remove_shared_context(swarm_id, key)`. Single-key + all-keys reads route
   through `get_shared_context` / `shared_context_entries`. Debug snapshot
   consumers (`debug_swarm_read`, `debug_server_state`) use the read-only
   `shared_context_map()` accessor. Call sites converted in
   `client_comm_context`, `comm_plan` (×3), and `debug_swarm_write` (×5).
   Zero direct cross-module field access remains.
3. **`channel_subscriptions{,_by_session}`** (10). Reverse indexes; add
   `subscribe_channel` / `unsubscribe_channel` / `resolve_subscribers` methods
   (some already exist on the handle as `remove_session_channel_subscriptions`).
   *(landed 2026-09)* Both indexes are now private. Writes route through
   `subscribe_session_to_channel` / `unsubscribe_session_from_channel` /
   `remove_session_channel_subscriptions`; reads go through the forward
   `channel_subscriptions_map()` and reverse
   `channel_subscriptions_by_session_map()` accessors. Call sites converted in
   `client_comm_channels` (list / members / subscribe / unsubscribe),
   `client_comm_message`, `debug_server_state`, and `debug_swarm_read`; the
   dead `server.rs` re-exports for subscribe/unsubscribe were removed. Zero
   direct cross-module field access remains.
4. **`swarm_state` (169)** — the big one, itself a struct. Privatize
   incrementally *within* `SwarmState`: members first (existing handle methods
   `ensure_member`/`rename_member_session`/`set_member_status`/etc. already
   cover most mutations), then plans/coordinators/swarms_by_id.
   *(landed 2026-09)* The handle's `swarm_state` field is private behind a
   `swarm_state()` read-only accessor; ~167 direct `swarm.swarm_state.<map>`
   accesses across 23 files route through it. Note: the single-purpose
   teardown/rename/registration mutations now route through the handle, while
   the entangled orchestration mutations still touch the maps in place through
   the accessor (see the Done criteria status block). The `SwarmState` struct's
   own four maps stay `pub` (state.rs remains the domain API the handle wraps,
   per non-goals); a stricter future boundary could snapshot-ify or further
   private the sub-fields, out of scope here.
5. **`await_members_runtime` / `swarm_mutation_runtime`** (8). Runtime handles;
   expose narrow pass-through accessors or move the orchestration onto methods.
   *(landed 2026-09)* The two fields are private; `await_members_runtime()` and
   `swarm_mutation_runtime()` pass-through accessors route all 8 cross-module
   reads (`client_lifecycle`, `comm_session` ×2, `comm_control` ×2, `comm_plan`
   ×3, `client_lightweight_control`). `handle_client` clones the await runtime
   from the surviving handle clone. Direct unit tests added for the accessors.
6. **`file_touch`** (5). `FileTouchService` is already encapsulated; just
   expose accessors (or keep as the service handle's own field).
   *(landed 2026-09)* The field is private; a `file_touch()` accessor routes all
   8 cross-module sites (`monitor_bus` clones it, the debug/comm reads borrow
   it). Direct unit test added.
7. **Subscribe/resume orchestration final pass** — with the per-map methods in
   place, collapse the last inline multi-map mutations in `client_session.rs`.
   *(landed 2026-09)* The resume path's coordinator rewrite (old→new session id)
   was folded into `rename_member_session`, which now atomically updates
   members, `swarms_by_id`, and coordinators. Direct unit test locks the
   coordinator-rename. The remaining subscribe/cleanup inline mutations are
   deeply interleaved working-dir-rebind + coordinator re-election logic (the
   plan's "risk concentration"); they stay as documented orchestration because
   extracting them would risk the exact borrow-order behavior the plan warned
   to preserve.

## Boundaries / non-goals

- `state.rs` / `swarm.rs` free functions stay as-is (domain API; the handle
  wraps them). Private handle fields may call them internally.
- `spawn_or_resume_await_members` keeps owned args (task-spawn needs owned
  `Arc`/`Sender` clones); it builds its owned clones from accessors.
- No behavior change; every slice keeps the full `jcode-app-core` lib suite
  green (1480+) and clippy clean.
- Debug reads snapshots (Seam E) and maintenance loops use the accessors, not
  the raw maps.

## Done criteria

`swarm.swarm_state`, `swarm.event_history`, `swarm.event_counter`,
`swarm.swarm_event_tx`, `swarm.shared_context`,
`swarm.channel_subscriptions{,_by_session}` and the two runtime handles are
private; zero non-`services/swarm.rs` code reaches them; all mutations go
through handle methods; suite green + clippy clean.
**Status: field boundary MET, mutation funnel PARTIALLY met.**
- **Field boundary (met):** every named handle field is private; zero
  non-`services/swarm.rs` code references the raw fields — cross-module access
  goes through documented read accessors.
- **Single-purpose mutations (met):** teardown/rename/registration route
  through handle methods — `remove_session_member` / `take_session_membership`,
  `rename_member_session` (also rewrites coordinators), resume/detached cleanup,
  and `register_headless_member` (headless registration).
- **Remaining (not met):** "all mutations go through handle methods" does not
  hold — the entangled orchestration still mutates `members` / `swarms_by_id` /
  `plans.participants` / `coordinators` in place *after* borrowing them through
  the accessor. These are the plan's "risk concentration": the writes are
  interleaved with plan/task mutations, persistence, coordinator re-election,
  and subscriber fan-out (e.g. `plan.participants.insert` is always paired with
  a `version += 1` / task-progress update and a `participants.clone()` fan-out
  inside the same `plans.write()` scope), so leaf extraction would hold the very
  lock it itself takes (deadlock risk) or split logical mutations.

### Follow-up: route the coordination write sites (deferred to a new session)

**Measured 2026-09:** ~48 functional write sites, split into two kinds:

- **Kind A — genuine coordination mutations (~35, the WIN):** these should move
  onto **whole-transaction `SwarmServiceHandle` methods** (the caller stops
  touching the raw map entirely). Per file: `comm_control` (13), `comm_plan`
  (7), `comm_graph` (7), `comm_session` (8), `client_session` (4),
  `client_actions` (2), `comm_sync` (2). Requires one method per transaction
  (e.g. `assign_plan_task`, `requeue_existing_assignment`, `attach_plan`,
  `elect_coordinator`) taking the inputs and returning the fan-out tuple, NOT a
  leaf method (deadlock). Do slice-by-slice over these files, suite green +
  clippy `--all-targets` + deadlock review after each.
- **Kind B — ephemeral per-member UI-field writes (~5-8):** `background_tasks`
  (`output_tail`, `todo_items`, `todo_progress`), plus some per-member
  `last_seen`/role writes in `comm_control`/`client_actions`. These are
  cosmetic single-record updates, not coordination — recommend leaving them
  behind the read accessor (routing them is ceremony with no boundary value),
  unless literal completeness is required (then thin methods like
  `set_member_output_tail`).

Recommended scope for the follow-up: **route Kind A (the ~35 coordination
transactions) whole-transaction onto handle methods; leave Kind B as-is.**
Verify each slice independently.

- `debug_swarm_write` / persistence-test code is a documented privileged
  observer. `Server.swarm_state` (the handle's constructor source) is pub, out
  of scope. Full app-core lib suite green (1543), clippy clean on changed
  files.

## Review notes (2026-09)

- **`set_shared_context` unifies `created_at` semantics.** The three migrated
  write sites previously differed: `client_comm_context` and
  `debug_swarm_write` preserved the original `created_at` on re-insert, while
  `comm_plan::handle_comm_propose_plan` reset it to `now`. The shared method
  preserves `created_at` (matching the 2-of-3 majority and the more useful
  behavior). `created_at` is only consumed by debug snapshot display
  (`created_secs_ago` / `age_secs`), so this is cosmetic, not functional.
- **Read accessors return mutable-capable `&Arc<RwLock<...>>`.** The debug
  snapshot accessors (`shared_context_map`, `channel_subscriptions_map`,
  `channel_subscriptions_by_session_map`) expose the inner map by reference.
  This is intentional: debug is the privileged observer and uses these strictly
  for reads. They are documented read-only; all writes route through handle
  methods. A future stricter boundary could snapshot-ify these, but that is out
  of scope for the encapsulation slices.

- **Direct unit tests for the Tier-3 handle methods (added 2026-09).** The
  earlier review pass noted the new handle methods were only covered
  transitively via callers. `services/swarm.rs` now carries a dedicated
  `#[cfg(test)] mod tests` locking the behavior of `set_shared_context`
  (plain upsert, created_at preservation, append semantics),
  `get_shared_context` / `remove_shared_context` / `shared_context_entries`,
  `subscribe_session_to_channel` / `unsubscribe_session_from_channel` (both
  forward and reverse indexes), `read_event_sources` (seeded sinks
  round-trip), the runtime accessors, the file-touch accessor,
  `remove_session_member`, `rename_member_session` (coordinator rewrite), and
  `register_headless_member`. Suite green (1542), clippy clean.
