# Compass semantic search

`compass_query` is a first-class, always-available tool (like `read` or
`agentgrep`) that provides semantic code search and structural analysis backed
by Compass's knowledge graph. It integrates Compass as a **pure library** —
there is no MCP server and no CLI subprocess.

This document describes how the Compass index is stored, built, kept fresh,
and pre-warmed so that agents get fast, correct semantic search without
blocking a turn on a multi-minute cold build.

## Behavior

- A query goes through `CompassQueryTool::execute` → `ensure_fresh_engine` →
  `build_compass_index` → `build_graph_with_layers`. If an index already
  exists and is fresh, the query is served directly from it (no build).
- When a session binds to a project, a background **pre-warm** builds the
  index off the query path (see below). A query that arrives before that
  completes fails fast with a retryable "still building" message instead of
  joining the build and blocking the turn.
- If no pre-warm ran, the first query builds the index in-process and caches
  it. On a cold index that build can take minutes on a large repo, which is
  exactly the stall pre-warming removes.

## Cache locations

All Compass cache data lives under the **jcode home** (`~/.jcode`, or
`$JCODE_HOME` if set), never inside a project folder:

```text
<jcode_home>/compass/<project_id>/
  .ast-cache/            branch-agnostic AST-fact cache (shared across SHAs)
  <sha>/compass-out/     per-commit graph (git-backed)
  workspace/compass-out/ single graph (non-git)
```

- `project_id` is derived from the repo's git *common dir*, which is identical
  across all worktrees, so every worktree shares one cache.
- Because everything lives under jcode home, no cache is ever written into a
  project folder, so worktrees/checkouts never need their cache copied around.
- A warm index is reused on subsequent queries; the build runs only when the
  project has never been indexed or when source changed since the last build.
- On a rebuild, Compass reuses its shared AST cache (incremental extract), so a
  branch switch only re-extracts files that actually changed.
- A git branch/commit switch (HEAD change) is detected via a cached SHA sidecar
  and forces a rebuild even when no file mtime changed, so a freshly checked
  out tree is never served against a stale index.
- The current commit's index can be force-refreshed by deleting just its
  per-SHA dir (`<project_id>/<sha>/`); deleting the whole `<project_id>/` root
  also discards the shared `.ast-cache` and forces a full re-extract.

### Shared-cache staleness semantics
A shared index represents a single *committed* tree, keyed by commit SHA. Its
freshness is decided purely by whether the current commit SHA matches the
sidecar — never by walking the working tree. This guarantees an individual
worktree's uncommitted edits never force a shared rebuild from that dirty tree
(which would leak that worktree's uncommitted code into the index that every
clean worktree on the same SHA also reads).

### Garbage collection
Per-SHA output dirs whose commit is no longer reachable from the repo are
pruned after `SHA_RETENTION_TTL` (14 days), so `~/.jcode/compass/<project>/`
does not grow unbounded as a user visits many commits. (TTL pruning needs git to
determine reachability; when git is unavailable it is skipped rather than risk
deleting a live index.) The shared `.ast-cache`, the non-git `workspace` output,
and the current HEAD's per-SHA dir are always kept even when unreachable
(protecting the index a live worktree reads).

To bound the cache even when many commits remain *reachable* from refs (e.g. a
long-lived backup branch keeps `git rev-list --all` growing), a hard cap
`SHA_INDEX_MAX_KEPT` (default 3) keeps only the newest per-SHA dirs plus the
current HEAD, and prunes the oldest regardless of reachability — and regardless
of whether git is available, so a git-less environment stays bounded too. When
the current HEAD's SHA cannot be resolved (git unavailable), no dir is
name-protected as "current"; the just-built dir still survives because it is the
newest. Since the shared `.ast-cache` keeps branch-to-branch re-extract
incremental, pruning a stale per-SHA graph is cheap to recreate on the next
visit.

## Pre-warm on session bind

The main performance feature: when a session subscribes to a working directory
whose index is missing, a background build is kicked off so the agent's *first*
`compass_query` finds a warm index instead of blocking the turn on a multi-
minute cold build.

- New helper `compass_query::prewarm_compass_index(working_dir)` (pub(crate)),
  called from the session `handle_subscribe` path right after the working dir
  is bound to an agent.
- **Cheap on the hot path**: it only resolves the cache layout and checks
  `graph.json` existence. It never runs a full build and never walks the source
  tree. It does shell out to `git` once on a cold cache (typically a few ms,
  then cached ~60s in-process) — an inlined ~ms cost, not the multi-minute
  build.
- If the index is genuinely missing, it spawns a dedicated `compass-prewarm`
  background thread running the same `build_compass_index` under the same
  per-project flock that `ensure_fresh_engine` uses, so it serializes against
  any on-query rebuild sharing the `.ast-cache`.
- **Deduplication**: a process-global `PREWARM_IN_FLIGHT` set (keyed by the
  per-SHA output dir) ensures a swarm of sessions subscribing to the same repo
  and SHA triggers at most one cold build. The in-flight marker is cleared by
  an RAII guard on both normal completion and panic unwind, so a leaked marker
  can never wedge later queries.
- **Failure backoff**: a failed pre-warm build records its time, and
  `prewarm_compass_index` skips re-spawning within a 300s cooldown so an
  unindexable project does not trigger a full build attempt on every subscribe.
  The cooldown is keyed by the *project* (`ast_cache_root`, stable across all
  SHAs), not the per-SHA output dir, so an unindexable project stays backed off
  across a branch/commit switch instead of re-triggering a full cold build on
  each new SHA. The on-query build still runs and surfaces failures to the
  agent.
- **Best-effort**: spawn failures are logged and swallowed; session bind never
  depends on it.
- **Gated** by `tools.prewarm_compass_index` (default on), and skipped when the
  session tool policy disables `compass_query` (no point building an index the
  session cannot query).
- **Effective bound directory**: pre-warm uses the same `bound_dir` that swarm
  grouping uses, not the raw subscribe report, so a home-dir subscribe while
  the agent is already bound to a project doesn't pre-warm the wrong path
  (issue #481).

### Fail-fast while pre-warming

If a query arrives *before* the pre-warm finishes, joining it would hold the
shared per-project build lock and turn a normally-instant warm query into a
multi-minute blocking build — the exact stall pre-warming targets.
`CompassQueryTool::execute` therefore checks `prewarm_in_flight` up front and,
when a background build is active, returns a retryable "index building in
background" message. The guidance is intent-aware:

- **Keyword/search** queries suggest using `agentgrep` in the meantime or
  retrying `compass_query` shortly.
- **Structural** queries (`callers`, `callees`, `impact`, `discovery`,
  `traverse`) note that `agentgrep` cannot fully substitute and point the agent
  at retrying `compass_query` after the warm-up.

Covered by `execute_fails_fast_while_prewarm_in_flight` and
`query_racing_prewarm_is_safe`.

## Rebuild tuning

When an index is built, two options are set to cut unnecessary work on large
repos:

- `no_cluster = true` and `no_viz = true`: the query engine opens `graph.json`
  and reads nodes/edges/files; it does not use community clustering or the HTML
  viz artifacts. Skipping them removes work unrelated to query results.
- Worker sizing is left to Compass's own bounded default (it self-limits to at
  most 12 and only spins up the full pool once enough files are missing to
  amortize it). A stricter ceiling is deliberately NOT pinned: `build_compass_index`
  serves both the background pre-warm **and** the on-query cold build, and
  capping the latter below the machine default would slow the blocking
  fallback.

### Why `no_cluster`/`no_viz` do not change query quality

- With `GraphStorage::Json`, no store is published, so `open` reads `graph.json`
  via the JSON graph engine. Validation only checks schema + node/edge counts;
  lookup indices build from node/file data, not communities.
- Communities are only surfaced by the `Community` discovery scope, which jcode
  does not use (it only sends `search`/`impact`/`discovery`/`callers`/`callees`/
  `traverse`).
- In `compass-query`, `ranking.rs`, `recall.rs`, and `index.rs` only *store*
  `community` as an empty column / `None` on a no-cluster build; they never
  weight it in scoring or recall. So the tuning cuts build work without changing
  result quality on any path jcode uses.

## Concurrency safety

`compass_query` is concurrency-safe. The common warm path is a pure function of
its input plus the index files. A cold cache either (a) fails fast with a
"still building" message when a pre-warm is already building this project, or
(b) triggers an in-process build, serialized via an exclusive `flock` (the
same per-project build lock the pre-warm uses) so concurrent calls cannot
clobber each other's `graph.json`. The `PREWARM_IN_FLIGHT` and `PREWARM_LAST_FAILED`
maps are poison-tolerant (`lock_cached` recovers the guard via `into_inner`), so
a panic in a pre-warm thread cannot brick later dedup or cooldown.

## Known limitations / future work

- `no_cluster`/`no_viz` cut query surface for community-scoped discovery; if a
  future `intent` ever needs communities, they can be re-enabled. (jcode's
  `intent` value is currently display-only — it never selects a community
  scope — so this is not active today.)
- A per-SHA pre-warm happens only for the SHA a session subscribes to; if a
  session quickly switches branches, the new SHA cold-builds unless another
  subscribe pre-warms it.

## Integration with compass-first enforcement

The compass-first enforcement tier (redirect `agentgrep` → `compass_query`
when a warm index exists, gated by `tools.prefer_compass_query`) coexists with
this feature. Both knob types live in `tools`: `prefer_compass_query` (the
enforcement redirect) and `prewarm_compass_index` (background pre-warm). They
are independent and both default on.

The one interaction to be aware of: an `agentgrep` call during an in-flight
pre-warm can be redirected to a `compass_query` that fails fast with
`building-in-background`, so the agent may see a redirect then a fail-fast.
That is benign — the message tells the model to retry `compass_query` once the
background build finishes, or use the `allow_raw_fallback` escape hatch — and
the redirect is safely gated on `prefer_compass_query` + `compass_query` being
invokable under the session tool policy (same `session_tool_is_disabled` check
the pre-warm uses).

The `allow_raw_fallback` escape hatch is not free for full-text grep: once a
redirect fires for a session, that session must make a real `compass_query`
attempt before an `agentgrep` grep call with `allow_raw_fallback` is accepted
again (see `tool::compass_enforcement`). This closes a prod-observed bypass
where a model retried `agentgrep` with `allow_raw_fallback: true` on the turn
immediately after a redirect and never attempted `compass_query` at all. The
restriction is cleared by any actual `compass_query` execution — including a
`building-in-background` fail-fast — so a project Compass genuinely cannot index
still reaches raw grep after one real attempt. `find`/`outline`/`trace` modes
are unaffected (they are never redirected and never blocked). The restriction is
also not applied when `compass_query` has since become unavailable to the session
(removed or disabled by policy), and the pending flag is reset on a fresh session
bind or restore, so a re-attached or restored session is never stale-blocked.
