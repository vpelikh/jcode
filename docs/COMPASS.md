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

## Query modes

The `mode` field is a real dispatch key: it selects which Compass query
operation runs. It is deliberately **not** named `intent`, because `intent` is
the harness-wide, auto-injected, display-only "why" field every tool already
carries (see `ensure_intent_in_schema`); overloading it would collide with that
contract and with the UI activity line. `search` (the default) is
keyword/semantic symbol search; the structural modes resolve a symbol and return
its relationships, so a session answers call-graph/neighborhood questions from
the index instead of reading many raw files.

| mode | Compass operation | what it returns |
| --- | --- | --- |
| `search` | `CodeQueryEngine::search` | ranked symbol hits + source snippets |
| `callers` | `::callers` | one-hop inbound calls of a symbol |
| `callees` | `::callees` | one-hop outbound calls of a symbol |
| `impact` | `::impact` | bounded transitive impact radius |
| `explore` | `::explore` | neighborhood symbols, connecting paths, **digest-verified source** |
| `discover` | `::discover` | natural-language question routed to seeds + neighborhood |
| `traverse` | `::node_trail` | the evidence path between two symbols |
| `context` | `compass_core::build_task_context` | declaration + callers + callees + tests + impact packet |
| `affected` | `compass_query::affected_nodes` | everyone who *depends on* a node (the reverse of `impact`): inbound dependents through the requested relations, to a bounded depth |
| `orientation` | graph summary (jcode-composed) | repo map: file/symbol counts, top-level directories, hottest symbols |

Notes:
- Operands: `query` is the scalar operand (the search text for `search`, the
  symbol for `callers`/`callees`/`impact`/`context`/`affected`, and `traverse`'s
  source);
  `symbols` (an array) is used by `explore` only, to resolve a whole set in one
  call; `source`/`target` name `traverse`'s endpoints (`source` falls back to
  `query`). At least one of `query`/`symbols`/`source` is required; a call with
  no operand is a clear error, and `traverse` requires both a source and a
  target. `orientation` takes no operand (it maps the whole repo), so it is
  exempt from the operand check. `relations` (an array, defaulting to Compass's
  `DEFAULT_AFFECTED_RELATIONS`) and `depth` (default 2, matching Compass's CLI)
  shape `affected` only; an all-blank `relations` override falls back to the
  default set rather than silently following no relations. `path` scopes
  structural nodes/source and `discover` (matched on whole path segments, so
  `src` does not match `src2`), but `context` does not support it (Compass's
  task-context API has no path scope, so a `path` there is a clear error rather
  than a silently ignored filter); `include_heuristic` opts into lower-confidence
  structural evidence. When a `path` filter excludes every result Compass
  returned, the (empty) report says the filter is the cause rather than "no
  matches", so it does not misdirect the caller to try another symbol name.
  Likewise, when an operand resolves to several nodes (Compass `AmbiguousMatch`),
  the empty report tells the caller to qualify the name or add a `path` instead
  of retrying, and a reversed `traverse` (Compass `DirectionMismatch`) says to
  swap the endpoints. Every report header (including `context` and `discover`,
  which build their own) and the tool title are bounded in length, so a large
  `symbols` set or a very long operand is summarized rather than printed in full;
  a `traverse` labels both endpoints (`source -> target`).
  Diagnostics from every mode (including `discover`, whose seed-ambiguity message
  can name a seed by raw node id) are relabeled to symbol names where the
  response carries the node, so a report never leaks an opaque `sha256:` id.
  A `callers`/`callees`/`impact`/`explore` query that resolves its symbol(s) but
  finds no relationships says so explicitly ("No callers found", etc.) rather
  than listing only the resolved seed as if it were a result. If the `path`
  filter merely excluded the related symbols, it says that instead of claiming
  none exist.
- The structural report renders the resolved node set (name, kind, roles, file),
  the edges between them, any traversed paths, and fenced declaration snippets
  for the first few nodes; `explore`/`context` additionally render the source
  Compass already verified from disk. This is the "read fewer files" win: one
  call returns real code for a neighborhood instead of the model issuing
  separate `read` calls. A `context` report renders each source file at most
  once even when several sections reference it.
- Structural reports are bounded (`MAX_EDGE_ROWS`, `MAX_NODE_ROWS`,
  `MAX_NODE_SNIPPETS`, `MAX_PATH_ROWS`, `MAX_SOURCE_FILES`) so one call cannot
  balloon context.
- An unknown `mode` is a clear error rather than a silent fallback to search.
- Errors are split by cause: a caller-input problem (a missing/invalid operand,
  an unsupported filter, or a Compass `InvalidParameter` such as an unknown
  `discover` scope) is reported plainly, while a genuine engine/index failure
  (corrupt artifact, graph invariant, internal) keeps the "clear the cache to
  force a rebuild" guidance.

## Result format

Each hit is rendered with its qualified name, source file, node kind, score,
and matched fields, **plus a compact source snippet** of the declaration read
from disk. Showing the actual code is what makes `compass_query` a genuine
substitute for `agentgrep` on symbol/declaration lookups: an earlier version
returned only a bare ranked list of node names + paths, so a precise lookup
(e.g. "definition of `SessionId`") surfaced fuzzy unrelated matches and the
model abandoned compass for a raw grep — the dominant reason `agentgrep`
grep calls outnumbered `compass_query` in real sessions.

Snippet details:
- Extracted from the current file on disk (not the index snapshot) relative to
  the session working directory, **falling back to the git worktree toplevel**
  so a session bound to a repo subdirectory still resolves the repo-relative
  source path Compass stores.
- Span is `[start_line, end_line)` from the node's source anchor, capped at 8
  lines with a `...` fold marker for longer nodes (bounding context-window cost).
- Only the **top 8 results** get a fenced snippet; the rest are listed as
  name/file/kind rows (still fully ranked) so one query cannot tile many fences
  into context (`tool::compass_query::MAX_SNIPPET_ROWS`).
- Source files are read **once per query** and shared across results that land
  in the same file (`tool::compass_query::SourceCache`), so a wide query does
  not re-open the same file per hit.
- Best-effort: a missing/unreadable file, an `..`-escaping or absolute path, or
  an out-of-range anchor renders no snippet without failing the query (see
  `tool::compass_query::resolve_source_text`).
- Reads are bounded: only the top 8 results trigger any file read, each unique
  file is read at most once, and only that file's line windows are materialized
  into the report (never the whole file body).

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
background" message. The guidance is mode-aware:

- **Keyword/search** queries suggest using `agentgrep` in the meantime or
  retrying `compass_query` shortly.
- **Structural** queries (`callers`, `callees`, `impact`, `explore`,
  `traverse`, `context`) note that `agentgrep` cannot fully substitute and point
  the agent at retrying `compass_query` after the warm-up.

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
  does not use (it sends `search`/`callers`/`callees`/`impact`/`explore`/
  `discover`/`traverse`/`context`/`affected`/`orientation`; none selects a
  community scope). `orientation` in particular composes its map from the graph
  rather than a community artifact (see below).
- In `compass-query`, `ranking.rs`, `recall.rs`, and `index.rs` only *store*
  `community` as an empty column / `None` on a no-cluster build; they never
  weight it in scoring or recall. So the tuning cuts build work without changing
  result quality on any path jcode uses.

### The `affected`/`orientation` graph load

`affected` and `orientation` do not use `CodeQueryEngine` (which the other modes
open); they use Compass's raw-graph surfaces (`compass_query::resolve_seed` +
`affected_nodes`, and raw node/edge summaries; jcode renders the report itself so
it can bound rows, apply `path`, and report in-scope counts), so they load
`graph.json` through
`compass_model::Graph::load_for_affected`. That is a bounded **compact
projection** (only the attributes affected traversal, seed resolution, and
rendering read) which Compass caches next to `graph.json` keyed by the graph
signature. It runs on a blocking thread inside the same `with_build_lock`-
serialized path the engine open uses, so concurrent calls stay safe and other
modes are not slowed (the load happens only for these two modes).

This load is **not** cheap, so two measures keep it off the query path:

- **The engine is never opened for these modes.** Opening `CodeQueryEngine` on a
  large repo is far slower than the graph load itself (measured ~58s on the
  240MB graph, versus ~13s cold / microseconds warm for the graph). The
  graph-only modes therefore use `ensure_fresh_graph`, which checks freshness
  the same way the engine path does (`index_is_fresh`: git-SHA sidecar plus a
  throttled mtime walk) and rebuilds a cold/stale index exactly like the engine
  path, then loads the graph only when the index is actually served — never
  opening the engine. On the real repo this cut a scoped `affected` call from
  ~72s (engine open + graph load) to ~14s, and a warm `orientation` call to
  ~0.16s.
- **The loaded graph is cached process-globally**, keyed by `(graph_path, mtime)`
  (`GRAPH_CACHE` in `compass_query.rs`), and shared as an `Arc<Graph>` so
  repeated calls re-pay nothing. Measured on the 240MB graph: ~22s for the first
  load and ~13s with Compass's compact cache; a warm cache hit is microseconds.
  A rebuild (new mtime) transparently invalidates the entry, the entry is
  inserted only after re-confirming the mtime is unchanged (so a concurrent
  rebuild cannot leave a stale entry), and inserting evicts other entries for
  the same path so at most one (large) graph per project is retained.

### `orientation` builds a community-free map (trade-off)

`orientation` summarizes the repository **without** communities. The alternative
was to build the index with `no_cluster = false` so `analysis.json`/community
columns land in the cache, but that would add a clustering pass to *every*
pre-warm and cold build for *every* user just to serve one mode. Instead the map
is derived from the graph jcode already has: distinct-file and symbol counts
(the file count dedupes `source_file` paths, so it does not under-report when a
graph has source-anchored nodes but no dedicated `file` node), top-level
directories by file count, and the most-connected ("hottest") symbols. If a
community-scoped map later proves worth the build cost, clustering can be
re-enabled behind a config flag and the map can consume it.

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
  future `mode` ever needs communities, they can be re-enabled. (jcode's
  `mode` never selects a community scope — it only maps to symbol- and
  call-graph-level Compass operations — so this is not active today.)
  `orientation` is the one mode that *would* benefit from communities (a
  clustered repo map), but it deliberately summarizes the graph without them to
  avoid paying a clustering pass on every build; see "`orientation` builds a
  community-free map" above. **Follow-up: community-backed `orientation`**
  (deferred, not implemented) — design below.
- `affected` follows a fixed relation set (`DEFAULT_AFFECTED_RELATIONS`) unless
  the caller passes `relations`; it does not infer relations from the language.
  A relation Compass did not extract (e.g. a dynamic dispatch it could not
  resolve) will not appear, so an empty `affected` report means "no *indexed*
  dependents", not "provably nothing depends on this".
- A per-SHA pre-warm happens only for the SHA a session subscribes to; if a
  session quickly switches branches, the new SHA cold-builds unless another
  subscribe pre-warms it.
- The `allow_raw_fallback` enforcement re-arms only once per session (after any
  single `compass_query` attempt). Re-arming it per redirect is considered but
  deferred (see the weighted trade-off in the enforcement section): it would
  force `compass_query` usage but risks false-blocking legitimate out-of-index
  searches. Revisit only if post-ship measurement shows fallback reliance is
  unchanged despite the source-snippet results.

### Follow-up (deferred): community-backed `orientation`

Not implemented. A richer `orientation` would list Compass's *communities*
(labelled module clusters), "god nodes" (hubs), and cross-community surprises,
instead of only top-level directories and hottest symbols. Compass already
produces this: `compass_core::cluster_existing_graph` rewrites `graph.json`'s
nodes with `community` metadata and emits `graph-overview.json` (labels, hubs,
cohesion, surprises, suggested questions) plus `labels.json`. The lower-level
`compass_graph::cluster` / `build_communities` compute the same communities
in memory (no files written).

**Measured cost of enabling it** (real jcode repo, 240MB graph, 91k nodes;
temp-copy, non-destructive probe):

| approach | measured |
| --- | --- |
| default build-time (`no_cluster = false`) every build | rejected: adds clustering to every pre-warm/cold build for all users (the exact cost `no_cluster` was set to avoid) |
| `cluster_existing_graph` on the existing graph | **~81s** total: load 21.6s, cluster 7.7s, analyze 1.3s, report 49.9s, export 3.3s; 881 communities |

The dominant terms are the redundant graph load (21.6s) and the one-shot
`report` phase (49.9s), not the clustering itself (7.7s).

**Recommended design (if pursued):** gate behind a config flag (e.g.
`tools.compass_communities`, default **off**); on first use run clustering
lazily once and cache the result next to the graph keyed by mtime (like
`GRAPH_CACHE`) so it is paid once. To avoid the 49.9s one-shot report, call the
lower-level `build_communities` (which takes a `code_graph::GraphDocument`, not
the `model::Graph` the graph modes hold, so it needs that typed document rather
than truly reusing the loaded graph) instead of `cluster_existing_graph`.
`orientation` then renders community labels + hubs when present and falls back to
the current directory/hottest-symbol map when not. Cost stays zero for users who
never ask. This changes product cost/behavior and adds a config surface, so it
needs sign-off on the flag name and default before implementing.

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

Because the pending flag clears after *one* genuine `compass_query` attempt,
session logs historically show models satisfying that single required call and
then running the bulk of their grep searches with `allow_raw_fallback: true`
(often 75–91% of grep calls in a session). The source-snippet rendering above
attacks the underlying cause — making `compass_query` actually answer the
declaration/structure queries that previously pushed the model to grep.

### Considered alternative: re-arm the raw-fallback gate per redirect

A stricter alternative is to *not* clear the pending flag on the first compass
attempt, so every redirected grep (or each new search intent) requires a fresh
`compass_query` before `allow_raw_fallback` is honored. Weighing:

- **Effectiveness:** would force `compass_query` usage to approximate the grep
  rate more closely, since a raw-fallback grep must be re-earned each time.
  Stronger guarantee than relying on the model finding snippets useful.
- **Cost — false-blocking risk (the decisive drawback):** the whole point of
  `allow_raw_fallback` is legitimate out-of-index searches (build output, logs,
  vendored/generated code, files outside the indexed tree). Re-arming per grep
  means a session doing real work there pays a `compass_query` round-trip before
  every such grep — wasted turns, budget, and a "call compass that won't help"
  workflow. The existing one-attempt-per-session rule is already a compromise;
  our data shows it is *after* that one attempt that models over-use the hatch,
  which the snippet fix targets directly.
- **Cost — complexity:** the flag becomes a per-session *counter*/intent-map
  with state transitions across restore/bind (the current set already has
  documented edge cases). More state to keep correct under session switching.
- **Compatibility:** changes behavior for existing sessions mid-flight; harder
  to reason about for `find`/`outline`/`trace` which legitimately bypass.
- **Maintenance:** two mechanisms (enforcement + result quality) both poking at
  the same behavior makes a future regression harder to attribute.

**Decision:** keep the one-attempt gate and rely on the source-snippet fix as the
primary lever — it removes the underlying reason (bare results drove models to
grep) without risking legitimate out-of-index searches. A measurement of 114
real `compass_query` results reinforces the deferral:
- 92% (105/114) returned non-empty hits, so a "arm the gate on hits" rule would
  have kept the escape hatch closed for almost every real compass call;
- but 22% of compass-with-hits calls were followed by a raw-fallback grep in the
  same session — the model went back to grep even though compass returned
  results, i.e. hit-count is a poor proxy for "compass answered." Arming the gate
  on hits would therefore false-block real searches in that ~22% of cases, which
  is exactly what the escape hatch exists to avoid.

Because hit-count cannot separate "answered" from "noise," the enforcement is
**deferred pending real-world measurement**: `tool::compass_enforcement` now
records per-session `compass_query` vs raw-`agentgrep`-grep counts (logged as
`COMPASS_SEARCH_USAGE`) so a post-ship check can confirm whether the snippet fix
lifts the ratio. Re-arm the gate only if that measurement shows reliance is
unchanged *and* the extra compass round-trips on out-of-index searches are
acceptable.
