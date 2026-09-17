# Swarm Handle Encapsulation Plan (Tier 3)

Status: Plan for the Tier 3 "true encapsulation" follow-up flagged by
`SERVER_SERVICE_SPLIT_PLAN.md`. Tier 1 (mechanical convergence onto
`&SwarmServiceHandle`) is landed; this documents how to *close* the split by
privatizing the handle's fields so all mutations must go through handle
methods.

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
   mutation-method moves.
3. **`channel_subscriptions{,_by_session}`** (10). Reverse indexes; add
   `subscribe_channel` / `unsubscribe_channel` / `resolve_subscribers` methods
   (some already exist on the handle as `remove_session_channel_subscriptions`).
4. **`swarm_state` (169)** — the big one, itself a struct. Privatize
   incrementally *within* `SwarmState`: members first (existing handle methods
   `ensure_member`/`rename_member_session`/`set_member_status`/etc. already
   cover most mutations), then plans/coordinators/swarms_by_id.
5. **`await_members_runtime` / `swarm_mutation_runtime`** (8). Runtime handles;
   expose narrow pass-through accessors or move the orchestration onto methods.
6. **`file_touch`** (5). `FileTouchService` is already encapsulated; just
   expose accessors (or keep as the service handle's own field).
7. **Subscribe/resume orchestration final pass** — with the per-map methods in
   place, collapse the last inline multi-map mutations in `client_session.rs`.

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
