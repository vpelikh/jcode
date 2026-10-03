//! Semantic code search backed by Compass's knowledge graph.
//!
//! `compass_query` is a first-class, always-available tool (like `read` or
//! `agentgrep`). It integrates Compass as a pure library: there is no MCP
//! server and no CLI subprocess. When a session binds to a project, a
//! background pre-warm builds the Compass index off the query path (see
//! `prewarm_compass_index`); a query that arrives before that completes fails
//! fast with a retryable "still building" message instead of joining the build.
//! If no pre-warm ran, the first query builds the index in-process and caches
//! it.
//!
//! ## Cache locations
//!
//! All Compass cache data lives under the **jcode home** (`~/.jcode`, or
//! `$JCODE_HOME` if set), never inside a project folder:
//!
//! ```text
//! <jcode_home>/compass/<project_id>/
//!   .ast-cache/            branch-agnostic AST-fact cache (shared across SHAs)
//!   <sha>/compass-out/     per-commit graph (git-backed)
//!   workspace/compass-out/ single graph (non-git)
//! ```
//!
//! * `project_id` is derived from the repo's git *common dir*, which is
//!   identical across all worktrees, so every worktree shares one cache.
//! * Because everything lives under jcode home, no cache is ever written into a
//!   project folder, so worktrees/checkouts never need their cache copied around.
//!
//! A warm index is reused for subsequent queries, so the build runs only when
//! the project has never been indexed or when source has changed since the last
//! build. On a rebuild Compass reuses its shared AST cache (incremental
//! extract), so a branch switch only re-extracts files that actually changed
//! instead of rebuilding the whole project.
//! A git branch or commit switch (HEAD) is detected via a cached SHA sidecar
//! and forces a rebuild even when no file mtime changed, so a freshly checked
//! out tree is never served against a stale index.
//! The current commit's index can be force-refreshed by deleting just its
//! per-SHA dir (`<project_id>/<sha>/`); deleting the whole `<project_id>/` root
//! also discards the shared `.ast-cache` and forces a full re-extract.
//!
//! ### Shared-cache staleness semantics
//!
//! A shared index represents a single *committed* tree, keyed by commit SHA.
//! Its freshness is therefore decided purely by whether the current commit SHA
//! matches the sidecar — never by walking the working tree. This guarantees an
//! individual worktree's uncommitted edits never force a shared rebuild from
//! that dirty tree (which would leak that worktree's uncommitted code into the
//! index that every clean worktree on the same SHA also reads).
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use compass_core::{build_graph_with_layers, BuildOptions, BuildPurpose};
use compass_model::provenance::SourceAnchor;
use compass_model::query_contract::{
    CallRequest, CodeQueryLimits, CodeQueryResponse, ExploreRequest, ImpactRequest,
    NodeTrailRequest, SearchRequest,
};
use compass_model::{Graph, NodeIndex};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use super::{Tool, ToolContext, ToolOutput};

/// Top-level directory under the jcode home where all Compass indexes live.
/// Keeping everything under jcode home (`~/.jcode` or `$JCODE_HOME`) means no
/// cache data is ever written into a project folder, so a worktree or checkout
/// never needs its cache copied around.
const COMPASS_CACHE_HOME: &str = "compass";

/// Name of the branch-agnostic AST-fact digest cache shared across all SHAs of
/// one repository/project. Compass keys it by file content (repo-relative), so
/// a branch switch only re-extracts files that actually changed.
const AST_CACHE_DIR: &str = ".ast-cache";

/// Name of the non-git (workspace) output dir inside a project root.
const WORKSPACE_DIR: &str = "workspace";

/// Subdirectory of an output dir where Compass stores its SQLite code-query
/// index, one keyed child per `(graph identity, program digest, schema)`.
const CODE_QUERY_DIR: &str = "code-query";

/// Compass's cross-process index-build lock file name. Compass creates it with
/// `create_new` and removes it only after a *successful* build, so a build that
/// is killed or panics leaves it behind and permanently wedges every later
/// `open()` for that same index key with `query_index_lock_timeout`.
const CODE_QUERY_LOCK_FILE: &str = "index.lock";

/// Prefix of the temporary SQLite file Compass writes before atomically
/// renaming it to the final index; a crashed build leaves these orphans too.
const CODE_QUERY_TMP_PREFIX: &str = "index.tmp-";

/// How long a per-SHA output dir is retained before it is eligible for GC, if
/// its SHA is no longer reachable from the repo. Older, unreachable per-commit
/// graphs are pruned so `~/.jcode/compass/<project>/` does not grow unbounded
/// as a user visits many commits over time.
const SHA_RETENTION_TTL: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Maximum number of per-SHA index dirs kept under one project, regardless of
/// reachability. Beyond this, only the newest `SHA_INDEX_MAX_KEPT` dirs plus the
/// current HEAD's are retained. Without a cap, per-commit graphs that remain
/// *reachable* from any ref (e.g. a backup branch) accumulate forever — each can
/// be very large on a big repo — so a repo with many long-lived branches can grow
/// `~/.jcode/compass/<project>/` unbounded. Keeping only a few per-SHA dirs (plus
/// the current HEAD) bounds the per-project Compass cache irrespective of size.
const SHA_INDEX_MAX_KEPT: usize = 3;

/// Resolved Compass cache paths for a working directory.
///
/// * `output_dir` — Compass's *output root*. Compass writes its output under
///   `<output_dir>/compass-out/` (graph.json, manifest.json, and the `.git-sha`
///   sidecar). For git work it is per-commit (`.../<project_id>/<sha>/`), so
///   each distinct commit gets an isolated, immutable graph.
/// * `graph_path` — the `graph.json` inside `<output_dir>/compass-out/`.
/// * `ast_cache_root` — the branch-agnostic AST-fact digest cache shared across
///   all SHAs of the same repo/project, so branch switches rebuild incrementally.
/// * `build_lock_dir` — the directory used for the flock that serializes builds
///   sharing the same `output_dir`.
/// * `is_shared` — true when this is a git-backed per-SHA cache that must decide
///   staleness purely by commit SHA (see `index_is_stale`).
#[derive(Clone)]
struct CompassCachePaths {
    output_dir: PathBuf,
    graph_path: PathBuf,
    ast_cache_root: PathBuf,
    build_lock_dir: PathBuf,
    is_shared: bool,
}

#[derive(Debug, Deserialize)]
struct CompassQueryInput {
    /// The operand: the search text for `mode=search`, and the symbol for the
    /// single-symbol structural modes (`callers`/`callees`/`impact`/`context`)
    /// and `traverse`'s source. `explore` may pass a set via `symbols` instead.
    #[serde(default)]
    query: Option<String>,
    /// Optional path filter: a file or directory prefix/segment (e.g. `src`,
    /// `crates/foo/src/lib.rs`). Matched on whole path segments.
    #[serde(default)]
    path: Option<String>,
    /// Limit result count
    #[serde(default)]
    limit: Option<usize>,
    /// Query mode. `search` (default) is keyword/semantic symbol search;
    /// `callers`/`callees`/`impact` resolve a symbol and return its call graph;
    /// `explore` gathers a symbol's neighborhood and verified source; `traverse`
    /// finds the evidence path between two symbols; `context` composes a
    /// task-oriented packet (declaration + callers + callees + tests + impact +
    /// source) for one target. Named `mode` (not `intent`) because `intent` is
    /// the harness-wide, display-only "why" field every tool already carries.
    #[serde(default)]
    mode: Option<String>,
    /// A set of symbols for `mode=explore` only (overrides `query`). One call
    /// gathers the neighborhood and connecting paths for the whole set, so a
    /// session orients around several symbols without a call each. Blank entries
    /// are ignored; other modes ignore `symbols` entirely (they use `query`).
    #[serde(default)]
    symbols: Option<Vec<String>>,
    /// Explicit source symbol for `traverse` (overrides `query`).
    #[serde(default)]
    source: Option<String>,
    /// Explicit target symbol for `traverse`.
    #[serde(default)]
    target: Option<String>,
    /// Edge relations to follow for `mode=affected` (defaults to Compass's
    /// [`DEFAULT_AFFECTED_RELATIONS`]). Other modes ignore it.
    #[serde(default)]
    relations: Option<Vec<String>>,
    /// Traversal depth bound for `mode=affected` (default 2). Other modes ignore
    /// it; `limit` remains the separate result-count bound.
    #[serde(default)]
    depth: Option<usize>,
    /// Include heuristic (lower-confidence) structural evidence.
    #[serde(default)]
    include_heuristic: Option<bool>,
}

impl CompassQueryInput {
    /// The scalar operand: `query` (the search text for `search`, the symbol for
    /// the single-symbol structural modes). Empty when not supplied.
    fn query_operand(&self) -> String {
        self.query.clone().unwrap_or_default()
    }

    /// The first non-blank entry of `symbols`, if any. Only `explore` uses the
    /// set; this feeds its operand-presence check.
    fn first_symbol(&self) -> Option<String> {
        self.symbols
            .as_ref()
            .and_then(|s| s.iter().find(|x| !x.trim().is_empty()))
            .cloned()
    }

    /// Human-readable label naming what this call targeted, for the result title
    /// and error message. Prefers the concrete operand(s) over a free-form query.
    /// Bounded so neither a large `symbols` set nor a very long single operand can
    /// balloon the title/header line.
    fn display_target(&self, mode: QueryIntent) -> String {
        // `explore` is the one mode that takes a set.
        if mode == QueryIntent::Explore
            && let Some(symbols) = self.symbols.as_ref()
        {
            let non_blank: Vec<String> = symbols
                .iter()
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .collect();
            if !non_blank.is_empty() {
                return summarize_symbols(&non_blank);
            }
        }
        let mut label = String::new();
        for operand in [&self.source, &self.query] {
            if let Some(value) = operand.as_ref().filter(|v| !v.trim().is_empty()) {
                label = header_label(value);
                break;
            }
        }
        // `traverse` targets two symbols; name both so the header/title reads
        // "from -> to" rather than naming only the source.
        if let Some(target) = self.target.as_ref().filter(|t| !t.trim().is_empty()) {
            let target = header_label(target);
            if label.is_empty() {
                label = target;
            } else {
                label = format!("{label} -> {target}");
            }
        }
        header_label(&label)
    }
}

/// Maximum characters of the operand label rendered in a report header or tool
/// title. A wide `symbols` set is summarized rather than printed in full, so one
/// call cannot flood the context with a thousands-of-characters title.
const MAX_DISPLAY_TARGET_CHARS: usize = 120;

/// Bound the report header label. Even a single operand can be long (a
/// free-form `search` query may run to Compass's multi-kilobyte query cap), and
/// the header is context the model pays for, so collapse and truncate it.
fn header_label(query: &str) -> String {
    let collapsed: String = query.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= MAX_DISPLAY_TARGET_CHARS {
        return collapsed;
    }
    format!(
        "{}…",
        crate::util::truncate_str(&collapsed, MAX_DISPLAY_TARGET_CHARS)
    )
}

/// Join a symbol set into a bounded label: as many entries as fit within
/// [`MAX_DISPLAY_TARGET_CHARS`], then `… (+N more)` when some were omitted. A
/// single oversized first entry is hard-truncated so the label stays bounded.
fn summarize_symbols(symbols: &[String]) -> String {
    summarize_list(symbols, MAX_DISPLAY_TARGET_CHARS)
}

/// Join `items` with `, `, bounded to `max_chars`: always shows at least the
/// first entry (truncating it if it alone overflows) and appends `… (+N more)`
/// when the rest does not fit, so a caller-supplied list can never balloon a
/// header line.
fn summarize_list(items: &[String], max_chars: usize) -> String {
    if items.is_empty() {
        return String::new();
    }
    // Always show at least the first entry, truncating it if it alone overflows.
    let first = &items[0];
    let first = if first.len() > max_chars {
        format!("{}…", crate::util::truncate_str(first, max_chars))
    } else {
        first.clone()
    };
    let mut out = first;
    let mut shown = 1usize;
    for item in &items[1..] {
        if out.len() + 2 + item.len() > max_chars {
            break;
        }
        out.push_str(", ");
        out.push_str(item);
        shown += 1;
    }
    if shown < items.len() {
        out.push_str(&format!(" … (+{} more)", items.len() - shown));
    }
    out
}

pub struct CompassQueryTool;

impl CompassQueryTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CompassQueryTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for CompassQueryTool {
    fn name(&self) -> &str {
        "compass_query"
    }

    fn description(&self) -> &str {
        "Semantic code search via the code graph; prefer over agentgrep."
    }

    fn concurrency_safe_marker(&self) -> bool {
        // Read-only inspection tool for the common path (warm cache): pure
        // function of its input plus the index files, mutates no shared
        // agent/session state, spawns no subprocesses, and does not depend on
        // sibling tool results. A cold cache may (a) fail fast with a "still
        // building" message when a session-subscribe pre-warm is already
        // building this project on a background thread, or (b) trigger an
        // in-process index build that writes files, serialized via an exclusive
        // `flock` (see `with_build_lock`) so concurrent calls cannot clobber
        // each other's `graph.json`. Either way it is safe to run in parallel
        // with siblings.
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search text (mode=search) or the symbol for structural modes; traverse source."
                },
                "mode": {
                    "type": "string",
                    "enum": [
                        "search", "callers", "callees", "impact", "explore",
                        "discover", "traverse", "context", "affected", "orientation"
                    ],
                    "description": "search, callers, callees, impact, explore, discover, traverse, context, affected, orientation."
                },
                "symbols": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Symbol set for mode=explore only (overrides query)."
                },
                "source": {
                    "type": "string",
                    "description": "Source symbol for mode=traverse (overrides query)."
                },
                "target": {
                    "type": "string",
                    "description": "Target symbol for mode=traverse."
                },
                "path": {
                    "type": "string",
                    "description": "Path filter (file/dir segment, e.g. src). Not supported by mode=context."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results/nodes to return."
                },
                "relations": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Relations for mode=affected (default: calls, references, imports, inherits, ...)."
                },
                "depth": {
                    "type": "integer",
                    "description": "Traversal depth for mode=affected (default 2; 0 means no traversal). Ignored by other modes."
                },
                "include_heuristic": {
                    "type": "boolean",
                    "description": "Include lower-confidence heuristic evidence. Ignored by context/affected/orientation."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: CompassQueryInput = serde_json::from_value(input)?;

        let working_dir = ctx
            .working_dir
            .clone()
            .ok_or_else(|| anyhow!("compass_query requires a working directory"))?;

        // Resolve the mode up front so the pre-warm guidance and the query
        // dispatch agree on whether this is a structural call.
        let mode = QueryIntent::parse(params.mode.as_deref())?;

        // Resolve the Compass cache paths. All caches live under the jcode home
        // (`~/.jcode` or `$JCODE_HOME`), partitioned by repository/project id and
        // commit SHA. Nothing is written into the project folder, so a worktree
        // or fresh checkout never needs its cache copied around.
        let cache = resolve_compass_cache(&working_dir);
        if let Err(e) = std::fs::create_dir_all(&cache.output_dir) {
            return Ok(ToolOutput::new(format!(
                "Failed to create Compass cache directory: {}",
                e
            )));
        }

        // A session-subscribe pre-warm may still be building this project's index in
        // the background. If so, prefer to wait (bounded) for that already-running
        // build to finish so this query returns the answer in the same turn,
        // rather than starting its own blocking build or failing fast and relying
        // on the model to retry. Only if the build is not ready in time do we fall
        // back to the non-blocking "still building" hint.
        if prewarm_in_flight(&cache.graph_path, &cache.output_dir)
            && !wait_for_prewarm(&cache.graph_path, &cache.output_dir, PREWARM_JOIN_TIMEOUT)
                .await
        {
            let structural = mode.is_structural();
            let guidance = if structural {
                "This is a structural query, so `agentgrep` cannot fully \
                 substitute.\n\
                 Retry `compass_query` shortly; once the background build \
                 finishes, the next `compass_query` is served from the warm index."
            } else {
                "Use `agentgrep` for keyword searches in the meantime, or \
                 retry `compass_query` shortly; the background build finishes \
                 on its own and the next `compass_query` is served from the \
                 warm index."
            };
            return Ok(ToolOutput::new(format!(
                "A Compass index is being built for this workspace in the \
                 background and is not ready yet.\n\n{}",
                guidance
            ))
            .with_title("compass_query: index building in background")
            .with_metadata(json!({
                "engine": "compass",
                "status": "building-in-background",
            })));
        }

        let effective_limit = params.limit.unwrap_or(20).max(1);
        let include_heuristic = params.include_heuristic.unwrap_or(false);
        let graph_only = matches!(mode, QueryIntent::Affected | QueryIntent::Orientation);

        // The graph-only modes (`affected`/`orientation`) use Compass's raw
        // `Graph` (its affected traversal and repo-map surfaces live there) and
        // never touch the engine, so they take an engine-free path: ensure the
        // index is fresh and load the (process-globally cached) graph on a
        // blocking thread under the project flock. Skipping the engine open is
        // deliberate and load-bearing — on a large repo that open costs ~58s and
        // dominates the call, while the cached graph load is microseconds.
        let graph: Option<Arc<Graph>> = if graph_only {
            let cache_edge = cache.clone();
            let working_dir = working_dir.clone();
            let loaded = tokio::task::spawn_blocking(move || {
                ensure_fresh_graph(&cache_edge, &working_dir)
            })
            .await
            .expect("compass graph task panicked");
            match loaded {
                Ok(graph) => Some(graph),
                Err((open_err, build_err)) => {
                    return Ok(ToolOutput::new(format_index_unavailable(
                        &open_err, &build_err,
                    )));
                }
            }
        } else {
            None
        };

        // The engine-backed modes open (or build) the Compass query engine. A cold
        // or stale index is (re)built in-process via Compass's library API. The
        // build can take seconds for a large project, so it runs on a blocking
        // thread; the project flock serializes concurrent builds, keeping the
        // concurrency-safe contract intact.
        let engine = if graph_only {
            None
        } else {
            let cache_edge = cache.clone();
            let engine_res: std::result::Result<compass_query::CodeQueryEngine, (String, String)> =
                tokio::task::spawn_blocking({
                    let edge = cache_edge.clone();
                    let working_dir = working_dir.clone();
                    move || ensure_fresh_engine(&edge, &working_dir)
                })
                .await
                .expect("compass index task panicked");
            match engine_res {
                Ok(engine) => Some(engine),
                Err((open_err, build_err)) => {
                    return Ok(ToolOutput::new(format_index_unavailable(
                        &open_err, &build_err,
                    )));
                }
            }
        };

        let result = execute_query(
            engine.as_ref(),
            mode,
            &params,
            effective_limit,
            include_heuristic,
            &working_dir,
            graph.as_deref(),
        );

        let reported_limit = effective_limit;
        // `orientation` has no operand; label it by mode so the title/error is
        // never blank (the other modes name the symbol/query they targeted).
        let label = if mode == QueryIntent::Orientation {
            "orientation".to_string()
        } else {
            params.display_target(mode)
        };
        match result {
            Ok(output) => Ok(ToolOutput::new(output)
                .with_title(format!("compass_query: {label}"))
                .with_metadata(json!({
                    "engine": "compass",
                    "mode": mode.as_str(),
                    "limit": reported_limit,
                    "path_filter": params.path,
                }))),
            Err(e) => Ok(ToolOutput::new(match e.downcast_ref::<InputError>() {
                // A caller-input problem never reached the engine, so no cache
                // advice applies.
                Some(input) => input.to_string(),
                None => format_query_error(&e.to_string(), &label, &cache.output_dir),
            })),
        }
    }
}

/// Resolve the Compass cache paths for `working_dir`. All of them live under
/// the jcode home (see [`crate::storage::jcode_dir`]):
///
/// ```text
/// <jcode_home>/compass/<project_id>/<model-or-layout>/
///   .ast-cache/        branch-agnostic AST-fact digest cache (shared across SHAs)
///   <sha_or_layout>/... per-commit output dir (graph.json + .git-sha sidecar)
/// ```
///
/// `project_id` is derived from the git common dir for repos (identical across
/// every worktree of the same repo) or from the canonical absolute path for
/// non-git directories. No cache data is written into the project folder.
fn resolve_compass_cache(working_dir: &Path) -> CompassCachePaths {
    // Determine a stable per-repository id. It must be identical across all
    // worktrees of one repo so they share the AST cache and, per SHA, the index.
    let project_key = current_git_top_cached(working_dir).unwrap_or_else(|| {
        // Non-git: use the canonical absolute path so a stable project id still
        // lives entirely under jcode home (no data in the project folder).
        canonical_string(working_dir).unwrap_or_else(|| working_dir.display().to_string())
    });
    let project_id = short_id(&project_key);

    // `output_dir` below is Compass's *output root*: Compass writes its graph
    // under `<output_dir>/compass-out/graph.json` (see build_compass_index).
    let Ok(compass_home) = crate::storage::jcode_dir().map(|d| d.join(COMPASS_CACHE_HOME)) else {
        // No jcode home (unset and no dirs home): fall back to a local cache
        // inside the working dir so the tool still functions.
        let output_dir = working_dir.join(".jcode/cache/compass");
        let graph_path = output_dir.join("compass-out/graph.json");
        return CompassCachePaths {
            ast_cache_root: output_dir.join(AST_CACHE_DIR),
            build_lock_dir: output_dir.clone(),
            output_dir,
            graph_path,
            is_shared: false,
        };
    };
    let project_root = compass_home.join(&project_id);

    // All caches share one branch-agnostic AST-fact digest cache under the
    // project root, so switching branches re-extracts only changed files.
    let ast_cache_root = project_root.join(AST_CACHE_DIR);

    if let Some(sha) = current_git_sha_cached(working_dir) {
        // Git-backed: per-SHA output root, so each commit has an isolated,
        // immutable graph. Worktrees on the same SHA share it exactly.
        let output_dir = project_root.join(&sha);
        let graph_path = output_dir.join("compass-out/graph.json");
        // Serialize on the *project root* (not per-SHA): all worktrees of one
        // repo write the same shared `.ast-cache`, and Compass does not lock
        // its cache internally. A per-project flock prevents two worktrees on
        // different SHAs from corrupting the shared history index concurrently.
        CompassCachePaths {
            build_lock_dir: project_root.clone(),
            ast_cache_root,
            output_dir,
            graph_path,
            is_shared: true,
        }
    } else {
        // Non-git: stable per-project id under jcode home (never the project
        // folder), single graph, branch-agnostic AST cache.
        let output_dir = project_root.join("workspace");
        let graph_path = output_dir.join("compass-out/graph.json");
        CompassCachePaths {
            ast_cache_root,
            build_lock_dir: output_dir.clone(),
            output_dir,
            graph_path,
            is_shared: false,
        }
    }
}

/// Deterministic, stable identifier for a path (used for the shared-cache
/// partition). Uses SHA-256 rather than `DefaultHasher`, whose algorithm is
/// explicitly documented as unstable across Rust releases/builds — a stable
/// key is required so an on-disk cache id does not change (and orphan the
/// cache) when jcode is rebuilt or upgraded. The full 256-bit digest is used:
/// a collision here would silently merge two distinct projects' caches.
fn short_id(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(s.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Canonical absolute string form of a path, for a stable non-git project id.
fn canonical_string(p: &Path) -> Option<String> {
    std::fs::canonicalize(p)
        .ok()
        .map(|c| c.to_string_lossy().into_owned())
}

/// Returns the set of commit SHAs reachable in `working_dir`'s repo via
/// `git rev-list --all` (all refs: heads, tags, remotes), or `None` if git is
/// unavailable. Used to identify per-SHA output dirs that are no longer
/// reachable and can be garbage-collected.
fn git_reachable_shas(working_dir: &Path) -> Option<std::collections::HashSet<String>> {
    let output = std::process::Command::new("git")
        .args(["rev-list", "--all"])
        .current_dir(working_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut shas = std::collections::HashSet::new();
    for line in String::from_utf8(output.stdout).ok()?.lines() {
        let sha = line.trim();
        if !sha.is_empty() {
            shas.insert(sha.to_string());
        }
    }
    Some(shas)
}

/// True if `name` looks like a git commit hash (40 hex for SHA-1, or 64 hex for
/// SHA-256 object format), i.e. a per-SHA output dir that GC may consider.
fn looks_like_sha(name: &str) -> bool {
    let len = name.len();
    (len == 40 || len == 64) && name.chars().all(|c| c.is_ascii_hexdigit())
}

/// Garbage-collect per-SHA output dirs under `project_root` that are no longer
/// reachable from `working_dir`'s repo and have not been touched within the
/// retention window. This keeps `~/.jcode/compass/<project>/` bounded as a user
/// visits many commits. Best-effort: any failure just skips pruning.
///
/// `current_sha` (the HEAD this build is for) is always kept, even if it is a
/// detached checkout that no ref points to — pruning it would delete the index
/// the very worktree currently uses. It may be empty when the current SHA cannot
/// be resolved (git unavailable); in that case no dir is *name*-protected, but
/// the just-built dir has the newest mtime and survives the cap's newest-kept
/// rule, so nothing an active worktree reads is lost.
fn prune_stale_sha_outputs(project_root: &Path, working_dir: &Path, current_sha: &str) {
    // Never prune the shared AST cache, the non-git workspace, or any lock file.
    let now = std::time::SystemTime::now();
    let Ok(entries) = std::fs::read_dir(project_root) else {
        return;
    };
    // Reachable-set is optional: when git is unavailable `git_reachable_shas`
    // returns None, and we then cannot TTL-age-out unreachable dirs safely, but we
    // MUST still enforce the hard cap below — otherwise a git-less environment
    // never bounds `~/.jcode/compass/` at all.
    let reachable_opt = git_reachable_shas(working_dir);

    // Collect all per-SHA dirs, then apply two GC rules:
    //   1. When we know reachability: unreachable dirs older than SHA_RETENTION_TTL
    //      are removed outright.
    //   2. If the number of surviving per-SHA dirs still exceeds SHA_INDEX_MAX_KEPT,
    //      keep only the newest SHA_INDEX_MAX_KEPT (plus current_sha) and remove the
    //      rest — even if they are reachable from some (e.g. backup) ref. Otherwise
    //      reachable per-commit graphs accumulate forever (each can be very large)
    //      and `~/.jcode/compass/<project>/` grows unbounded. Rule 2 runs
    //      regardless of reachability, so the count cap always bounds growth.
    let mut candidates: Vec<(SystemTime, PathBuf, String)> = Vec::new();
    // Scan all per-SHA dirs (no truncation): the hard cap below prunes the
    // oldest survivors. Limiting the scan could hide old dirs from the cap in
    // pathological repos with very long visit histories.
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Only per-SHA dirs are candidates; never touch shared/workspace/others.
        if name == AST_CACHE_DIR || name == WORKSPACE_DIR || !looks_like_sha(name) {
            continue;
        }
        // The current HEAD is always kept, even a detached HEAD with no ref.
        if name == current_sha {
            continue;
        }
        // If git reachability is unknown (None), conservatively treat every dir as
        // reachable for TTL purposes (never TTL-prune), but still count it toward
        // the cap so the cache stays bounded.
        let reachable = match &reachable_opt {
            Some(set) => set.contains(name),
            None => true,
        };
        // Age in seconds. A dir whose mtime is in the future (clock skew, a
        // `touch`-backdated file) must still be *counted* toward the hard cap so
        // it cannot silently bypass `SHA_INDEX_MAX_KEPT`; clamp it to age 0
        // (newest) rather than skipping it entirely. If the mtime cannot be read
        // at all, conservatively treat it as just-created too so it stays subject
        // to the cap instead of accumulating unbounded.
        let age = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|mtime| now.duration_since(mtime).ok())
            .unwrap_or_default();
        // Reachable commits (any branch/tag) are kept regardless of age, but their dir
        // still counts toward the max-kept cap below.
        if reachable {
            candidates.push((now - age, entry.path(), name.to_string()));
            continue;
        }
        // Unreachable dirs must be older than the retention window before being
        // removed, so a recent checkout that happens to be unreachable from refs
        // is not deleted immediately.
        if age >= SHA_RETENTION_TTL {
            let _ = std::fs::remove_dir_all(entry.path());
        } else {
            // Freshly-produced unreachable dir may still be reused after a branch
            // switch; count it toward the cap so a burst of SHA visits cannot
            // exceed the bound.
            candidates.push((now - age, entry.path(), name.to_string()));
        }
    }

    // Enforce the hard cap: drop the oldest survivors beyond the newest `MAX_KEPT`.
    if candidates.len() > SHA_INDEX_MAX_KEPT {
        // Newest first; drop from the tail (oldest) keeping the newest MAX_KEPT.
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        for (_mtime, path, _name) in candidates.into_iter().skip(SHA_INDEX_MAX_KEPT) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Format the user-facing message shown when neither an existing Compass index
/// can be opened nor a fresh one can be built in-process. The message uses Rust
/// `\`-line-continuations so the rendered text has no stray leading indentation.
fn format_index_unavailable(open_err: &str, build_err: &str) -> String {
    format!(
        "Compass knowledge graph is not available for this project yet.\n\n\
         Compass could not open an existing index ({}), and an attempt to build \
         one in-process also failed ({}).\n\n\
         Common causes: the project has no source files Compass can parse, or the \
         Compass extractor hit an unsupported dependency. As a workaround, use \
         agentgrep for grep/find/trace-style searches in the meantime.",
        open_err, build_err
    )
}

/// Format the user-facing message shown when the search engine errors after the
/// index is successfully built/opened.
fn format_query_error(e: &str, query: &str, cache_dir: &std::path::Path) -> String {
    format!(
        "Compass query failed: {}\n\nQuery: {}\n\n\
         The index is built, but the search engine returned an error.\n\
         Clear the cache to force a rebuild:\n\
         rm -rf {}",
        e,
        query,
        cache_dir.display()
    )
}

/// Run `f` while holding an exclusive lock on the project's Compass build lock.
///
/// The index build writes `graph.json`, so concurrent calls (this tool is
/// concurrency-safe and may be dispatched in parallel with siblings) must not
/// run it at the same time. We serialize on an exclusive `flock` over a lock
/// file in `cache_dir`, mirroring the daemon/build-lock pattern used elsewhere
/// in this crate. The lock is released when the file is closed (dropped), even
/// if `f` errors. On non-Unix targets without `flock` we run `f` unguarded,
/// accepting the same (rare) single-build-per-project race as before.
fn with_build_lock<F, T>(cache_dir: &std::path::Path, f: F) -> T
where
    F: FnOnce() -> T,
{
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        let lock_path = cache_dir.join(".compass-build.lock");
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
        {
            // Blocking exclusive lock: concurrent callers queue here instead of
            // racing. Blocking is acceptable because a build runs at most once
            // per project, and the harness already blocks the executor during it.
            let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            // `file` is dropped (releasing the lock) when this scope ends.
            return f();
        }
    }
    f()
}

/// Maximum time we trust a previously verified-fresh index without re-walking the
/// source tree for staleness. Within this window, repeated queries on an unchanged
/// project skip the mtime scan entirely. The tradeoff (intentional): a source edit
/// is guaranteed to be detected on the first query *after* the window elapses, not
/// necessarily within it. The scan itself stays fully correct; this only throttles
/// how often it runs so a busy agent doesn't re-stat the tree on every single call.
const STALE_RESCAN_TTL: Duration = Duration::from_secs(5);

/// Lock a process-global `Mutex` for these per-process caches, tolerating
/// poisoning. If a thread panics while holding one of these locks (e.g. inside a
/// build path), `Mutex::lock()` would otherwise make every subsequent `.unwrap()`
/// panic and break the tool for the whole process. `into_inner()` recovers the
/// (consistent-enough) guard instead.
fn lock_cached<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Process-global cache of the raw `compass_model::Graph` used by the graph-only
/// modes (`affected`/`orientation`), keyed by `(graph_path, mtime)`. The
/// non-`CodeQueryEngine` modes load the graph through
/// [`Graph::load_for_affected`]; on a large repo that is a multi-second load
/// (measured ~22s cold, ~13s with Compass's compact cache on a 240MB graph), so
/// repeated calls in one process must not re-pay it. Keying on the file mtime
/// means a rebuild (new `graph.json`) transparently invalidates the entry.
type GraphCacheKey = (PathBuf, SystemTime);
static GRAPH_CACHE: OnceLock<Mutex<HashMap<GraphCacheKey, Arc<Graph>>>> = OnceLock::new();

/// Return the cached graph for this exact `(path, mtime)` without loading, if
/// present. A hit also proves the on-disk graph is unchanged (a rebuild changes
/// the mtime), so it is a direct cache hit for [`load_graph_cached`].
fn graph_cache_fresh(graph_path: &Path, mtime: SystemTime) -> Option<Arc<Graph>> {
    lock_cached(GRAPH_CACHE.get_or_init(|| Mutex::new(HashMap::new())))
        .get(&(graph_path.to_path_buf(), mtime))
        .cloned()
}

/// Load (or reuse) the raw graph for the graph-only modes. Returns the shared
/// `Arc<Graph>` so callers never clone the (large) structure. Must be called on a
/// blocking thread and under the project build lock, since the underlying loader
/// may write Compass's compact projection cache next to `graph.json`.
fn load_graph_cached(graph_path: &Path) -> Result<Arc<Graph>, String> {
    let mtime = std::fs::metadata(graph_path)
        .and_then(|m| m.modified())
        .map_err(|e| format!("could not stat {}: {e}", graph_path.display()))?;
    // A loaded entry for this exact `(path, mtime)` also proves the on-disk graph
    // is unchanged, so it is reused without reloading.
    if let Some(graph) = graph_cache_fresh(graph_path, mtime) {
        return Ok(graph);
    }
    let key = (graph_path.to_path_buf(), mtime);
    let graph = Arc::new(
        Graph::load_for_affected(graph_path)
            .map_err(|e| format!("could not load the Compass graph: {e}"))?,
    );
    // Re-stat and only insert when the file is unchanged, so a concurrent
    // rebuild cannot leave a stale entry under the new mtime's key. On insert,
    // evict any entry for the *same* path with a different mtime: a rebuild
    // supersedes the old graph, and keeping one graph per graph file bounds the
    // cache to the number of distinct projects a process touches (the graph is
    // large, so retaining every historical build would leak memory in a
    // long-lived daemon).
    if std::fs::metadata(graph_path).and_then(|m| m.modified()).ok() == Some(mtime) {
        let mut cache = lock_cached(GRAPH_CACHE.get_or_init(|| Mutex::new(HashMap::new())));
        cache.retain(|(path, _), _| path != graph_path);
        cache.insert(key, graph.clone());
    }
    Ok(graph)
}

/// Last time a correct staleness scan proved `cache_dir` fresh, keyed by cache dir
/// so each project is throttled independently. Bounded in size by the number of
/// distinct projects indexed in this process.
static LAST_STALE_SCAN: OnceLock<Mutex<HashMap<PathBuf, SystemTime>>> = OnceLock::new();

/// True when this cache was verified fresh within `STALE_RESCAN_TTL`. On any error
/// (missing entry, clock skew) we return false so correctness wins over the shortcut.
fn recently_scanned(cache_dir: &Path) -> bool {
    let map = lock_cached(LAST_STALE_SCAN.get_or_init(|| Mutex::new(HashMap::new())));
    match map.get(cache_dir) {
        Some(&t) => t.elapsed().map(|d| d < STALE_RESCAN_TTL).unwrap_or(false),
        None => false,
    }
}

fn record_scan(cache_dir: &Path) {
    lock_cached(LAST_STALE_SCAN.get_or_init(|| Mutex::new(HashMap::new())))
        .insert(cache_dir.to_path_buf(), SystemTime::now());
}

/// How long a resolved `git rev-parse HEAD` result is reused before we re-shell
/// out to git. A branch/commit switch is still detected on (almost) every query
/// because `index_is_stale` compares the *cached* SHA against the index sidecar
/// before the mtime walk; this TTL only bounds how often we pay the fork/exec
/// cost of `git` itself, not how quickly a switch is noticed. Two seconds keeps
/// switch detection effectively immediate while avoiding a subprocess per call.
const GIT_SHA_CACHE_TTL: Duration = Duration::from_secs(2);

/// Currently resolved git SHA per working dir, so the branch-change check doesn't
/// spawn `git` on every query. Keyed by working dir (not cache dir) since HEAD
/// is a property of the repo, not the cache.
static LAST_GIT_SHA: OnceLock<Mutex<HashMap<PathBuf, (SystemTime, String)>>> = OnceLock::new();

/// Return the current git SHA, reusing a recently resolved value so we don't
/// fork `git` on every query. Falls back to `None` (mtime walk) exactly when
/// `current_git_sha` would, and refreshes at most once per `GIT_SHA_CACHE_TTL`.
fn current_git_sha_cached(working_dir: &Path) -> Option<String> {
    let map = LAST_GIT_SHA.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = lock_cached(map);
        if let Some((t, sha)) = guard.get(working_dir)
            && t.elapsed().map(|d| d < GIT_SHA_CACHE_TTL).unwrap_or(false)
        {
            return Some(sha.clone());
        }
    } // Drop the read lock before shelling out to git.
    match current_git_sha(working_dir) {
        Some(sha) => {
            lock_cached(map)
                .insert(working_dir.to_path_buf(), (SystemTime::now(), sha.clone()));
            Some(sha)
        }
        None => None,
    }
}

/// Name of the sidecar file that records the git commit the index was built
/// against, stored alongside `graph.json`. A mismatch means the user switched
/// branches/commits, so the index must be rebuilt.
const GIT_SHA_FILE: &str = ".git-sha";

/// Read the git SHA the index at `cache_dir` was last built against, if any.
fn index_git_sha(cache_dir: &Path) -> Option<String> {
    let p = cache_dir.join(GIT_SHA_FILE);
    std::fs::read_to_string(&p).ok().and_then(|s| {
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    })
}

/// Persist the git SHA the index was built against, so a later checkout that
/// changes HEAD is detected and forces a rebuild. Best-effort: a write failure
/// just means branch switches won't be detected (we fall back to the mtime walk).
fn write_index_git_sha(cache_dir: &Path, sha: &str) {
    let _ = std::fs::write(cache_dir.join(GIT_SHA_FILE), sha);
}

/// Return the current working dir's git commit SHA, or None if the dir is
/// not a git repo, detached, or if git is missing/non-UTF8. This does NOT block:
/// a failed git call just returns None, and the index will rely on the mtime walk.
///
/// Non-git dirs, detached HEAD, or any failure (git absent, read error, non-UTF8)
/// all return `None`, which is treated as "no branch information to compare" —
/// the index then relies on the mtime walk. We deliberately do not block on git:
/// a slow or broken `git rev-parse` must not stall a query.
fn current_git_sha(working_dir: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(working_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// How long a resolved git common-dir result is reused. The common dir is a
/// stable property of a repo clone, so a per-process cache with a long TTL is
/// fine and avoids a `git` fork on every query.
const GIT_TOP_CACHE_TTL: Duration = Duration::from_secs(60);

/// Currently resolved git top/common-dir per working dir. The git *common dir*
/// is identical across all worktrees of a repo (unlike `--show-toplevel`, which
/// differs per worktree), so it is a correct shared-cache partition key.
static LAST_GIT_TOP: OnceLock<Mutex<HashMap<PathBuf, (SystemTime, String)>>> = OnceLock::new();

/// Return the git *common dir* path string for `working_dir`, cached per process
/// so we don't fork `git` on every query. This is stable across every worktree
/// of one repo, which is exactly the partition key we need for a shared cache.
///
/// Falls back to `None` like [`current_git_sha`] when git is unavailable.
fn current_git_top_cached(working_dir: &Path) -> Option<String> {
    let map = LAST_GIT_TOP.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = lock_cached(map);
        if let Some((t, top)) = guard.get(working_dir)
            && t.elapsed().map(|d| d < GIT_TOP_CACHE_TTL).unwrap_or(false)
        {
            return Some(top.clone());
        }
    } // Drop the read lock before shelling out to git.
    // Prefer the git *common dir* (identical across all worktrees). `--path-format=absolute`
    // is a rev-parse flag (requires git >= 2.31); on older git it fails and we fall back
    // to `--show-toplevel`, which is also an absolute, subdir-stable repo identity.
    let top = git_repo_identity(working_dir);
    let top = top?;
    if top.is_empty() {
        return None;
    }
    let result = top.clone();
    lock_cached(map).insert(working_dir.to_path_buf(), (SystemTime::now(), top));
    Some(result)
}

/// Resolve a stable repository identity string for `working_dir`, preferring
/// the git common dir (identical across all worktrees) and falling back to the
/// working-tree toplevel for older git. Returns `None` when git is unavailable
/// or `working_dir` is not inside a git repo.
///
/// Note on the fallback: `--show-toplevel` resolves to the *current worktree's*
/// own top directory, which differs for each linked worktree. That is safe
/// (it never causes cross-worktree contamination), but on git < 2.31 the
/// shared-cache benefit across linked worktrees is reduced because each worktree
/// maps to its own identity. The absolute common-dir primary path (git >= 2.31)
/// is what actually gives all worktrees one shared key.
fn git_repo_identity(working_dir: &Path) -> Option<String> {
    // Primary: absolute common dir.
    let common = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(working_dir)
        .output()
        .ok();
    if let Some(out) = common
        && out.status.success()
        && let Ok(s) = String::from_utf8(out.stdout)
    {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    // Fallback: toplevel (absolute, subdir-stable).
    let top = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(working_dir)
        .output()
        .ok()?;
    if !top.status.success() {
        return None;
    }
    let s = String::from_utf8(top.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Decide whether `graph_path`'s index is older than any source under `root`.
///
/// Best-effort: a missing index, or any IO error while walking the tree, is
/// treated as "not stale" (so we just build if it is missing, and never block a
/// query on a failed scan). We compare the index mtime against the newest mtime
/// among source files Compass can parse; new/moved dirs are also detected because
/// the walk descends through them.
///
/// Callers should gate this behind `recently_scanned`/`record_scan` so the walk
/// does not run on every query (see `ensure_fresh_engine`): within
/// `STALE_RESCAN_TTL` of a verified-fresh scan we reuse the index without re-walking.
///
/// `shared` selects shared-cache semantics. A shared index is keyed strictly by
/// the commit SHA and represents the *committed* tree: freshness is determined
/// purely by SHA match, and the mtime walk is intentionally skipped. This is
/// essential for correctness: local, uncommitted edits in one worktree must not
/// force a rebuild of the shared index from that worktree's dirty tree, which
/// would leak that worktree's uncommitted code into the index that clean
/// worktrees on the same commit also read.
fn index_is_stale(
    root: &Path,
    graph_path: &Path,
    current_sha: Option<&str>,
    cache_dir: &Path,
    shared: bool,
) -> bool {
    // Check for branch/commit change first. If the current git SHA differs from
    // the one the index was built against, it's definitely stale.
    if let Some(sha) = current_sha
        && let Some(cached_sha) = index_git_sha(cache_dir).as_deref()
        && sha != cached_sha
    {
        return true; // Branch/commit changed, index is stale
    }

    // A shared index is keyed by commit SHA and represents only committed code,
    // so SHA match is the complete freshness criterion. We never walk the tree:
    // uncommitted edits belong to one worktree and must not invalidate (or force
    // a rebuild of) the index clean worktrees on the same SHA read.
    if shared {
        return false;
    }

    // Short-circuit if we recently scanned and confirmed freshness.
    if recently_scanned(cache_dir) {
        return false;
    }

    let Ok(index_meta) = std::fs::metadata(graph_path) else {
        return false; // No index (or unreadable): handle the cold case elsewhere.
    };
    let Ok(index_mtime) = index_meta.modified() else {
        return false;
    };

    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if ft.is_dir() {
                // Skip caches/VCS so unrelated churn (e.g. .git, target) does not
                // force constant rebuilds.
                if let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && matches!(name, ".git" | "target" | "node_modules" | ".jcode")
                {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file() {
                let is_source = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e, "rs" | "py" | "js" | "ts" | "go" | "tsx" | "jsx"));
                if !is_source {
                    continue;
                }
                if let Ok(meta) = std::fs::metadata(&path)
                    && let Ok(m) = meta.modified()
                    && m > index_mtime
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Open the Compass engine for `graph_path`, building (or rebuilding) it under a
/// project flock when it is missing, corrupt, stale, or on a different branch.
/// Returns the engine, or `(open_err, build_err)` describing why neither an open
/// nor a build succeeded.
///
/// Staleness is checked *before* an opened engine is trusted: a valid-but-old
/// index must not be served. Both the open probe and any rebuild run inside
/// `with_build_lock`, which serializes concurrent/stale rebuilds so two parallel
/// calls can't write `graph.json` at once.
fn ensure_fresh_engine(
    cache: &CompassCachePaths,
    working_dir: &Path,
) -> std::result::Result<compass_query::CodeQueryEngine, (String, String)> {
    let graph_path = &cache.graph_path;
    let output_dir = &cache.output_dir;
    with_build_lock(&cache.build_lock_dir, || {
        // Clear any orphaned code-query build lock/temp from a previously killed
        // or panicked build before opening. Compass's `open()` cannot recover
        // from its own left-behind `index.lock` and would otherwise wedge with
        // `query_index_lock_timeout` forever; holding the project flock here
        // guarantees no live build owns it.
        recover_orphaned_code_query_builds(output_dir);
        // Decide freshness without opening the engine: `index_is_fresh` uses the
        // git SHA sidecar and a throttled mtime walk, so opening a stale index
        // first would pay the (large) open only to discard it and rebuild.
        // Gate on the graph existing because `index_is_fresh` reports "fresh"
        // for a missing index.
        if graph_path.is_file() && index_is_fresh(cache, working_dir) {
            // Fresh on disk: open and return. A fresh-but-unopenable index is a
            // corrupt artifact, so fall through to rebuild rather than erroring.
            if let Ok(engine) = compass_query::open(graph_path, None, output_dir) {
                return Ok(engine);
            }
        }
        // Missing, corrupt, or stale.
        discard_stale_output(cache);
        rebuild_stale_index(cache, working_dir)?;
        compass_query::open(graph_path, None, output_dir).map_err(|e| {
            (
                "existing index missing or stale".to_string(),
                format!("Index was built but could not be opened: {e}"),
            )
        })
    })
}

/// Ensure the project's Compass index exists and is fresh, returning the loaded
/// raw graph. This is the engine-free counterpart of [`ensure_fresh_engine`],
/// used by the graph-only modes (`affected`/`orientation`).
///
/// Those modes never touch `CodeQueryEngine`, and opening it on a large repo is
/// prohibitively slow (measured ~58s on a 240MB graph, dominating the whole
/// call), so this checks freshness by loading the compact affected projection
/// instead — which the mode needs anyway and which [`load_graph_cached`] then
/// reuses. The staleness logic is otherwise identical to [`ensure_fresh_engine`]:
/// the same `index_is_stale` test, the same throttled scan, and the same rebuild
/// under the project flock.
fn ensure_fresh_graph(
    cache: &CompassCachePaths,
    working_dir: &Path,
) -> std::result::Result<Arc<Graph>, (String, String)> {
    let graph_path = &cache.graph_path;
    with_build_lock(&cache.build_lock_dir, || {
        // Decide freshness *before* paying the (large) load: `load_graph_cached`
        // on a stale index would parse a graph we then throw away (~seconds on a
        // large repo). The freshness check is the same `index_is_fresh` the
        // engine path uses, so the two cannot drift, and it stays throttled (a
        // warm, cached graph is still cheap to serve). Gate on the graph
        // existing first, because `index_is_fresh` reports "fresh" for a missing
        // index; the cold case is the rebuild below, not here.
        if graph_path.is_file() && index_is_fresh(cache, working_dir) {
            // Fresh on disk: load and return. A fresh-but-unloadable graph is a
            // corrupt artifact, so fall through to rebuild rather than erroring
            // (matching `ensure_fresh_engine`).
            if let Ok(graph) = load_graph_cached(graph_path) {
                return Ok(graph);
            }
        }
        discard_stale_output(cache);
        rebuild_stale_index(cache, working_dir)?;
        load_graph_cached(graph_path)
            .map_err(|e| ("existing index missing or stale".to_string(), e))
    })
}

/// True when the index at `cache` is fresh for `working_dir`. Shared by
/// [`ensure_fresh_engine`] and [`ensure_fresh_graph`] so both paths agree on
/// staleness. Records the scan timestamp for non-shared caches.
fn index_is_fresh(cache: &CompassCachePaths, working_dir: &Path) -> bool {
    let current_sha = current_git_sha_cached(working_dir);
    let fresh = !index_is_stale(
        working_dir,
        &cache.graph_path,
        current_sha.as_deref(),
        &cache.output_dir,
        cache.is_shared,
    );
    if fresh && !cache.is_shared {
        record_scan(&cache.output_dir);
    }
    fresh
}

/// Recover from an interrupted Compass code-query index build.
///
/// Compass's own cross-process lock (`<key>/index.lock`, created with
/// `create_new`) is removed only on the *success* path of `build_with_lock`; if
/// the build is killed or panics, the lock file is left behind. Because that
/// lock lives inside the per-key `code-query/<key>/` dir and Compass retries it
/// for ~5s before failing, every later `open()` for the same project wedges
/// with `query_index_lock_timeout` — so `compass_query` reports "not available
/// for this project yet" forever, and the agent falls back to `agentgrep`.
///
/// This clears such orphans. It is only safe because callers hold the project's
/// `.compass-build.lock` flock (`with_build_lock`) for the whole ensure/open
/// sequence: no other jcode process/thread can be mid-build for this project
/// (each `with_build_lock` opens its own fd, so the flocks are mutually
/// exclusive even within one process), so any lock file observed here cannot
/// belong to a live jcode build — it is a leftover from a crashed one.
///
/// Clearing unconditionally (rather than only when the final index is absent)
/// is deliberate: a *present but corrupt* `index.sqlite3` also forces a rebuild
/// that needs the lock, so skipping it when an index file merely exists would
/// leave that case wedged. Removing the lock cannot disturb a healthy index —
/// `open()` never consults the lock once a valid index is present — so there is
/// no safe case the removal can break. The final `index.sqlite3` itself is left
/// untouched; only the lock and the partial `index.tmp-*` scratch files (often
/// hundreds of MB from the crashed build) are reclaimed.
fn recover_orphaned_code_query_builds(output_dir: &Path) {
    let code_query_root = output_dir.join(CODE_QUERY_DIR);
    let Ok(entries) = std::fs::read_dir(&code_query_root) else {
        return;
    };
    for entry in entries.flatten() {
        let key_dir = entry.path();
        if !key_dir.is_dir() {
            continue;
        }

        let lock_path = key_dir.join(CODE_QUERY_LOCK_FILE);
        let lock_existed = lock_path.exists();
        let _ = std::fs::remove_file(&lock_path);
        let mut removed_tmp = 0usize;
        if let Ok(files) = std::fs::read_dir(&key_dir) {
            for file in files.flatten() {
                let name = file.file_name();
                if name.to_string_lossy().starts_with(CODE_QUERY_TMP_PREFIX)
                    && std::fs::remove_file(file.path()).is_ok()
                {
                    removed_tmp += 1;
                }
            }
        }
        if lock_existed || removed_tmp > 0 {
            crate::logging::event_info(
                "COMPASS_INDEX_RECOVERY",
                vec![
                    ("phase", "cleared_orphaned_build".to_string()),
                    ("key_dir", key_dir.display().to_string()),
                    ("removed_lock", lock_existed.to_string()),
                    ("removed_tmp_files", removed_tmp.to_string()),
                ],
            );
        }
    }
}

/// Discard a stale/unusable output before a rebuild, preserving the shared-cache
/// guarantees. Shared caches are keyed by the committed SHA and republished
/// atomically by Compass, and the dir may still be in use by another worktree on
/// the same SHA, so it is never deleted. A non-shared cache evolves in place and
/// needs a clean discard on source edits. Callers must have dropped any open
/// engine/graph first.
fn discard_stale_output(cache: &CompassCachePaths) {
    if !cache.is_shared {
        let _ = std::fs::remove_dir_all(&cache.output_dir);
        // Don't remove .compass-build.lock here - it's safe to leave and removing
        // it while holding the lock could block other worktrees.
        let _ = std::fs::remove_file(cache.output_dir.join(GIT_SHA_FILE));
    }
}

/// Rebuild the index for `cache`, then apply the post-build bookkeeping both
/// freshness paths need: record the scan (non-shared) or prune aged-out per-SHA
/// graphs (shared). `build_compass_index` records the current git SHA sidecar,
/// so a later branch switch is detected without walking the tree. The scan is
/// deliberately skipped for shared caches so each caller validates freshness
/// against its own working directory.
fn rebuild_stale_index(
    cache: &CompassCachePaths,
    working_dir: &Path,
) -> std::result::Result<(), (String, String)> {
    build_compass_index(working_dir, &cache.output_dir, &cache.ast_cache_root)
        .map_err(|e| ("existing index missing or stale".to_string(), e.to_string()))?;
    if !cache.is_shared {
        record_scan(&cache.output_dir);
    } else {
        // Prune unreachable, aged-out per-SHA graphs so the shared cache does
        // not grow unbounded as the user visits many commits. Always keep the
        // current HEAD's dir, even a detached HEAD with no ref.
        //
        // Run the prune even when the current SHA cannot be resolved (git
        // unavailable, transient git failure): the hard cap must still bound the
        // cache, which is why `prune_stale_sha_outputs` treats an unknown
        // reachability as "everything reachable" and caps by count. An empty
        // `current_sha` means no dir is name-protected, but the just-built dir
        // has the newest mtime and survives the cap's newest-kept rule.
        if let Some(project_root) = cache.output_dir.parent() {
            let current_sha = current_git_sha_cached(working_dir).unwrap_or_default();
            prune_stale_sha_outputs(project_root, working_dir, &current_sha);
        }
    }
    Ok(())
}

/// Build a Compass knowledge-graph index for the project in-process, using the
/// Compass library API (`compass_core::build_graph_with_layers`). The resulting
/// store is written into `output_dir` so the project tree stays untouched.
///
/// `cache_root` (`ast_cache_root`) holds Compass's AST-fact digests. Unlike the
/// output, it is NOT keyed per SHA: it lives under the project's shared
/// `.ast-cache` dir, so on a branch switch Compass reuses the content-keyed
/// cache and re-extracts only changed files instead of the whole project.
///
/// On success the current git commit SHA is recorded in a sidecar next to the
/// index, so a later `index_is_stale` call can detect a branch/commit switch
/// (HEAD moving) even when no file mtime changes. The sidecar lives with the
/// index (`output_dir`), so any build path — including direct callers — captures
/// it rather than relying on the caller to remember.
///
/// The output is cached under `output_dir` and the caller re-opens it via
/// `compass_query::open`, so subsequent queries skip the build entirely unless
/// source has since changed.
fn build_compass_index(
    root: &std::path::Path,
    output_dir: &std::path::Path,
    ast_cache_root: &std::path::Path,
) -> Result<(), anyhow::Error> {
    let mut options = BuildOptions::new(root);
    options.output_root = Some(output_dir.to_path_buf());
    options.cache_root = Some(ast_cache_root.to_path_buf());
    options.purpose = BuildPurpose::Extract;
    options.scan_filesystem = true;
    options.graph_storage = compass_core::GraphStorage::Json;
    // The query engine opens the persisted `graph.json` and reads nodes, edges,
    // and files directly; it does not need community clustering or the HTML
    // viz artifacts. Skipping them cuts unrelated build work on large projects
    // (the 300MB+ case that makes a cold build stall a query for minutes).
    options.no_cluster = true;
    options.no_viz = true;
    // Worker sizing is left to Compass's own bounded default (it self-limits
    // to at most PIPELINE_RAYON_WORKER_CAP = 12 and only spins up the full pool
    // once enough files are missing to amortize it). We deliberately do NOT
    // pin a stricter ceiling here: this function serves both the background
    // pre-warm AND the on-query cold build, and capping the latter below the
    // machine default would make the blocking fallback slower, not faster.

    build_graph_with_layers(&options, None, &[])
        .map_err(|e| anyhow!("compass_core build_graph failed: {}", e))?;

    // Record the commit we built against so a later branch switch is detected.
    // Best-effort: if git/SHA is unavailable we simply skip the sidecar and the
    // staleness check falls back to the mtime walk.
    if let Some(sha) = current_git_sha(root) {
        write_index_git_sha(output_dir, &sha);
    }
    Ok(())
}

/// Process-global set of per-SHA output dirs currently being pre-warmed, so a
/// swarm of sessions that all subscribe to the same repo (and thus the same
/// SHA) don't each spawn a duplicate cold build. Tolerant of poisoning (a panic
/// in a pre-warm thread must not brick later dedup).
static PREWARM_IN_FLIGHT: OnceLock<Mutex<HashMap<PathBuf, std::sync::Arc<tokio::sync::Notify>>>> =
    OnceLock::new();

/// True when a background pre-warm is currently building the Compass index for
/// `output_dir`'s per-SHA index. Used by `CompassQueryTool::execute` to
/// short-circuit a query that would otherwise race ahead of the pre-warm and,
/// by holding the same per-project build lock, turn a normally-instant warm
/// query into a minutes-long blocking build of its own.
///
/// Takes the already-resolved `graph_path` (per-SHA) and `output_dir` so
/// `execute` does not resolve the cache a second time; a finished pre-warm
/// leaves a graph.json there and is served directly.
fn prewarm_in_flight(graph_path: &Path, output_dir: &Path) -> bool {
    // Only meaningful when there is still nothing ready to serve; a finished
    // pre-warm leaves a graph.json and is served directly.
    if graph_path.is_file() {
        return false;
    }
    lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())))
        .contains_key(output_dir)
}

/// Bound within which a `compass_query` will wait for an already-running
/// background pre-warm to finish, rather than failing fast and relying on the
/// model to retry. Short enough not to stall a turn indefinitely; long enough
/// to absorb a typical cold build so the answer is served in the same turn.
const PREWARM_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Wait (bounded) for an in-flight background pre-warm of `output_dir` to
/// finish and produce a ready index, so the calling query can serve from the
/// warm graph instead of failing fast. Returns `true` if the index became ready
/// (or was already ready), `false` if there was no pre-warm running or the wait
/// timed out and the caller should fall back to its non-blocking path.
///
/// This is an async helper: waiting must not block a Tokio worker, so it awaits
/// the completion signal rather than spinning.
async fn wait_for_prewarm(graph_path: &Path, output_dir: &Path, timeout: std::time::Duration) -> bool {
    // Already ready.
    if graph_path.is_file() {
        return true;
    }
    let done = {
        let map = lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())));
        map.get(output_dir).cloned()
    };
    let Some(done) = done else {
        return false; // no pre-warm in flight; nothing to wait for
    };
    // Wait (bounded) for the pre-warm's completion signal. `notify_waiters` wakes
    // all currently-registered waiters but leaves no permit, so a `notified()`
    // registered *after* the build finished would wait until timeout. Rather than
    // depend on the signal, always re-check the ready graph afterwards: either
    // the wait was interrupted by completion, or it timed out, and in both cases
    // the graph having appeared is the ground truth that the index is warm.
    let _ = tokio::time::timeout(timeout, done.notified()).await;
    graph_path.is_file()
}

/// Best-effort, off-the-query-path pre-warm of the Compass knowledge graph for
/// `working_dir`.
///
/// This is the helper that backs the session-subscribe hook: it resolves the
/// cache layout, cheaply decides freshness WITHOUT building (so the subscribe
/// path never runs a full build), and only if the index is missing does it
/// spawn a background thread to build it under the existing per-project build
/// lock. Duplicate builds for the same project are suppressed with a
/// process-global in-flight set, so a busy agent session triggers at most one
/// cold build per project per process.
///
/// Note on cost: resolving the cache and confirming git identity shells out to
/// `git` on a cold cache (typically a few ms per call, and cached for ~60s
/// within a process), but never runs the Compass build itself. That is the
/// expensive, multi-minute work this helper moves onto a background thread.
///
/// Returns `false` (and does nothing) when pre-warming should not run: the
/// index is already fresh, the working dir has no git identity, or the caller
/// opted out. `true` means a background build was scheduled (or was already
/// in flight). Errors are swallowed: pre-warm is best-effort and must never
/// disturb session bind.
pub(crate) fn prewarm_compass_index(working_dir: &Path) -> bool {
    // Cheap staleness gate: an existing index needs no pre-warm. Anything
    // already on disk (even mildly stale) is served cheaply by the query path,
    // which handles incremental rebuilds itself; this feature only targets the
    // cases where there is *nothing* to serve yet — the multi-minute cold build
    // that would otherwise block a query. (Resolving the cache shells out to
    // `git` once on a cold cache; that ~ms cost is inlined and cached by
    // `resolve_compass_cache`/`current_git_top_cached`.)
    let cache = resolve_compass_cache(working_dir);
    if cache.graph_path.is_file() {
        return false;
    }

    // Even in the "no index yet" case, skip a pre-warm when there is no git repo
    // to index at all (`current_git_top_cached` is None), so non-git scratch
    // folders don't spawn a build. Note: a `git init`-ed repo with no commits
    // still passes this gate (it has a common dir regardless of whether it has
    // source), which is acceptable: its cold build is near-instant and produces
    // a trivial empty index that the query path reuses. We deliberately do not
    // walk the tree here — that would add cost to every session subscribe.
    if current_git_top_cached(working_dir).is_none() {
        return false;
    }

    // Back off after a recent failed build so an unindexable project does not
    // trigger a full multi-minute build attempt on every subscribe. The query
    // path's own build-on-demand still runs and surfaces the failure to the
    // agent; pre-warm just stops amplifying it.
    //
    // Keyed by the *project* (ast_cache_root, stable across SHAs of one repo),
    // not the per-SHA output_dir: an unindexable project fails on every SHA it
    // is visited at, so a per-SHA key would both (a) reset the cooldown on every
    // branch/commit switch, letting each new SHA re-trigger a full cold build,
    // and (b) grow the failure map one entry per failed SHA over the daemon's
    // lifetime. The project key keeps the cooldown for the whole repo and bounds
    // the map to the number of distinct projects.
    {
        let failed_map =
            lock_cached(PREWARM_LAST_FAILED.get_or_init(|| Mutex::new(HashMap::new())));
        if let Some(&at) = failed_map.get(&cache.ast_cache_root)
            && at.elapsed().map(|d| d < PREWARM_FAIL_COOLDOWN).unwrap_or(false)
        {
            return false; // recent project failure; don't re-spawn yet
        }
    }

    // Deduplicate concurrent pre-warm builds per output dir (per-SHA for git
    // repos), so a swarm of sessions on the same SHA spawns only one build.
    // Different SHAs index into different output dirs and serialize via the
    // shared per-project build flock instead.
    let mut map = lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())));
    if map.contains_key(&cache.output_dir) {
        return true; // already being built by another subscriber
    }
    // A per-build completion signal so a `compass_query` that arrives while this
    // build is running can wait for it (bounded) and then serve from the warm
    // index, instead of failing fast and relying on the model to retry.
    let done = std::sync::Arc::new(tokio::sync::Notify::new());
    map.insert(cache.output_dir.clone(), done.clone());

    let out_dir = cache.output_dir.clone();
    let error_out_dir = out_dir.clone(); // for cleanup if spawn fails
    let ast_cache = cache.ast_cache_root.clone();
    let build_lock_dir = cache.build_lock_dir.clone();
    let root = working_dir.to_path_buf();
    // Release the in-flight-map guard before `.spawn(...).map_err(...)`: that
    // error path re-locks `PREWARM_IN_FLIGHT` to clean up the marker, and `map`
    // still aliases the same `std::sync::Mutex` until the end of this block. A
    // `std::sync::Mutex` is not reentrant, so keeping the guard alive would
    // deadlock the spawn-failure path against itself. (`drop` runs now; NLL only
    // ends the borrow at last use, it does not move the `Drop` to the end of the
    // block on its own here because the guard's drop is deferred.)
    drop(map);
    let done_wake = done.clone();
    std::thread::Builder::new()
        .name("compass-prewarm".to_string())
        .spawn(move || {
            // Remove the in-flight marker (and wake any waiting query) on BOTH
            // normal completion and panic unwind. A leaked marker would make
            // every subsequent query for this project fail-fast with "still
            // building" until process restart.
            let _guard = PrewarmMarkerGuard {
                output_dir: out_dir.clone(),
                done: done_wake,
            };
            // Serialize against any on-query build AND other pre-warm threads
            // sharing this project's `.ast-cache` via the same per-project flock
            // that `ensure_fresh_engine` uses. A bare `build_compass_index`
            // here (without the lock) could otherwise run `build_graph_with_layers`
            // concurrently with a query-triggered rebuild against the same
            // output dir and shared AST cache — the exact corruption the lock
            // (commit 24ccf8e2c) exists to prevent.
            with_build_lock(&build_lock_dir, || {
                let result = build_compass_index(&root, &out_dir, &ast_cache);
                // Record a failed attempt so we don't hot-loop re-spawning a full
                // cold build on every subscribe for a project Compass genuinely
                // cannot index (e.g. unsupported dependency). A recent success
                // clears the marker; a failure sets a cooldown.
                let mut failed_map =
                    lock_cached(PREWARM_LAST_FAILED.get_or_init(|| Mutex::new(HashMap::new())));
                match result {
                    Ok(()) => {
                        failed_map.remove(&ast_cache);
                    }
                    Err(_) => {
                        failed_map.insert(ast_cache.clone(), SystemTime::now());
                    }
                }
            });
        })
        .map_err(|e| {
            crate::logging::event_warn(
                "COMPASS_PREWARM",
                vec![("error", format!("failed to spawn pre-warm thread: {e}"))],
            );
            // Do not leave a stale in-flight marker behind if the spawn failed, and wake
            // any query waiting on this build so it doesn't hang on a notify
            // that will never fire.
            lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())))
                .remove(&error_out_dir);
            done.notify_waiters();
        })
        .is_ok()
}

/// Window within which a failed pre-warm build is not retried, so a project
/// that Compass cannot index does not trigger a multi-minute full build attempt
/// (and its repeated cost) on every session subscribe. Short enough that a
/// transient hiccup resolves quickly; long enough to stop a tight retry loop.
const PREWARM_FAIL_COOLDOWN: Duration = Duration::from_secs(300);

/// When the last pre-warm build for a project failed, keyed for cooldown by
/// the project root (`ast_cache_root`, stable across SHAs of one repo).
static PREWARM_LAST_FAILED: OnceLock<Mutex<HashMap<PathBuf, SystemTime>>> = OnceLock::new();

/// RAII guard that removes this project's pre-warm in-flight marker when it is
/// dropped, and wakes any `compass_query` that is waiting on this build to
/// finish. Created at the top of the pre-warm thread so the cleanup runs on
/// normal completion AND on panic unwind (a panic anywhere in
/// `build_compass_index` / `build_graph_with_layers` must not permanently wedge
/// every later query for the project with a stale "building" marker, nor strand
/// a waiting query forever).
struct PrewarmMarkerGuard {
    output_dir: PathBuf,
    done: std::sync::Arc<tokio::sync::Notify>,
}

impl Drop for PrewarmMarkerGuard {
    fn drop(&mut self) {
        lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())))
            .remove(&self.output_dir);
        self.done.notify_waiters();
    }
}

/// The query intent selected by the caller.
///
/// Historically only `Search` was implemented; every other intent was a
/// display-only hint passed to the renderer while the engine still ran a plain
/// keyword search. This enum is now the real dispatch key: each structural
/// intent maps to a distinct Compass query operation so one call returns the
/// call graph / neighborhood / path the model actually asked for — which is
/// exactly what lets a session answer structural questions from the index
/// instead of reading many raw files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueryIntent {
    /// Keyword/semantic symbol search.
    Search,
    /// One-hop inbound calls of a symbol.
    Callers,
    /// One-hop outbound calls of a symbol.
    Callees,
    /// Bounded transitive impact radius of a symbol.
    Impact,
    /// Neighborhood (related symbols, connecting paths, verified source).
    Explore,
    /// Evidence path between two symbols.
    Traverse,
    /// Task-oriented evidence packet for one target.
    Context,
    /// Natural-language discovery: route the query to seeds and a bounded
    /// structural neighborhood.
    Discover,
    /// "What depends on this?" — the *reverse* of `impact`: walk inbound edges
    /// (callers, importers, implementors, renderers, ...) from a seed out to a
    /// bounded depth, so a session can size a change's blast radius. Backed by
    /// Compass's `affected_nodes` traversal over the raw `compass_model::Graph`
    /// rather than `CodeQueryEngine`.
    Affected,
    /// Repository map: a bounded, community-free overview (file/symbol counts,
    /// top-level directories, hottest symbols) so a session can orient in an
    /// unfamiliar repo. jcode builds the index with `no_cluster`/`no_viz`, so no
    /// community/analysis artifact exists to summarize; this composes its own
    /// map from the graph instead.
    Orientation,
}

impl QueryIntent {
    fn parse(raw: Option<&str>) -> Result<Self> {
        let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(Self::Search);
        };
        match raw.to_ascii_lowercase().as_str() {
            "search" => Ok(Self::Search),
            "callers" => Ok(Self::Callers),
            "callees" => Ok(Self::Callees),
            "impact" => Ok(Self::Impact),
            "explore" => Ok(Self::Explore),
            "discover" | "discovery" => Ok(Self::Discover),
            "traverse" | "trail" | "path" | "node_trail" => Ok(Self::Traverse),
            "context" | "task_context" => Ok(Self::Context),
            "affected" | "affected_nodes" => Ok(Self::Affected),
            "orientation" | "repo_map" | "repo-map" | "map" | "overview" => {
                Ok(Self::Orientation)
            }
            other => Err(anyhow!(
                "unknown compass_query mode {other:?}; expected one of \
                 search, callers, callees, impact, explore, discover, \
                 traverse, context, affected, orientation"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Callers => "callers",
            Self::Callees => "callees",
            Self::Impact => "impact",
            Self::Explore => "explore",
            Self::Traverse => "traverse",
            Self::Context => "context",
            Self::Discover => "discover",
            Self::Affected => "affected",
            Self::Orientation => "orientation",
        }
    }

    /// Structural intents cannot be substituted by a raw grep, so their
    /// pre-warm "still building" guidance points the model at retrying
    /// `compass_query` rather than falling back to `agentgrep`.
    fn is_structural(self) -> bool {
        !matches!(self, Self::Search)
    }
}

/// A single resolved node, flattened for rendering. Keeps the fields the report
/// shows (identity, kind, roles, source anchor) out of Compass's richer
/// `QueryNode` so rendering does not depend on the full contract type.
struct NodeView {
    id: String,
    name: String,
    qualified_name: String,
    kind: String,
    roles: Vec<String>,
    file: Option<String>,
    source: Option<SourceAnchor>,
}

/// A resolved edge between two nodes, flattened for rendering.
struct EdgeView {
    source: String,
    target: String,
    kind: String,
}

/// A node-to-node path, flattened to the node ids in traversal order.
struct PathView {
    node_ids: Vec<String>,
    weakest_confidence: String,
}

/// Fully resolved, render-ready view of one Compass response. Both search and
/// structural operations are normalized into this shape so a single renderer
/// (and one source cache) serves every intent.
struct ResponseView {
    /// Search hits in ranked order (empty for structural operations).
    hits: Vec<ResultRow>,
    nodes: Vec<NodeView>,
    edges: Vec<EdgeView>,
    paths: Vec<PathView>,
    /// Verified source files Compass already resolved from disk (explore/context).
    files: Vec<FileView>,
    truncated: bool,
    diagnostics: Vec<String>,
    /// True when a `path` filter removed results Compass did return, so an empty
    /// report can say the filter excluded them rather than "no matches".
    filtered_out: bool,
    /// True when a `path` filter removed an edge Compass did return (both
    /// endpoints present in the response but one outside the filter), so a
    /// "no callers found" note is not shown when callers merely fell outside it.
    filter_dropped_edges: bool,
    /// The set of diagnostic codes Compass emitted. Used to phrase an empty
    /// report correctly: an ambiguous operand needs qualifying, a reversed trail
    /// needs the direction swapped, rather than "try a different symbol name".
    diagnostic_codes: std::collections::BTreeSet<compass_model::query_contract::QueryDiagnosticCode>,
}

/// A digest-verified source file Compass resolved from disk.
struct FileView {
    path: String,
    digest: String,
    source: Option<String>,
    truncated: bool,
}

/// Tracks source already emitted with a fenced body during one report render, so
/// sibling `context` sections do not repeat it. Bodies are keyed by
/// `(path, digest)` (a path could theoretically carry different content across
/// sections); `paths` is the set of paths dumped at all, used to suppress a
/// duplicate per-node declaration snippet for a file already shown in full.
#[derive(Default)]
struct RenderedFiles {
    bodies: std::collections::HashSet<(String, String)>,
    paths: std::collections::HashSet<String>,
}

impl RenderedFiles {
    /// Number of distinct file bodies emitted so far.
    fn len(&self) -> usize {
        self.bodies.len()
    }

    /// Whether this exact `(path, digest)` body was already emitted.
    fn contains_body(&self, path: &str, digest: &str) -> bool {
        self.bodies.contains(&(path.to_string(), digest.to_string()))
    }

    /// Whether a body for this path (any digest) was already emitted.
    fn contains_path(&self, path: &str) -> bool {
        self.paths.contains(path)
    }

    /// Record an emitted body; returns false if it was already present.
    fn insert(&mut self, path: &str, digest: &str) -> bool {
        self.paths.insert(path.to_string());
        self.bodies.insert((path.to_string(), digest.to_string()))
    }
}

impl ResponseView {
    fn node(&self, id: &str) -> Option<&NodeView> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Short display label for a node id: qualified name when known, else id.
    fn label(&self, id: &str) -> String {
        self.node(id)
            .map(|n| n.qualified_name.clone())
            .unwrap_or_else(|| id.to_string())
    }
}

/// Marker for a caller-input error (a bad or absent operand, an unknown mode), as
/// opposed to an engine/query failure. The tool distinguishes the two so it never
/// tells the model to "clear the cache to force a rebuild" for a missing operand
/// (the index is fine). Carried inside `anyhow::Error` and recovered by
/// `downcast_ref`, so the helper signatures stay `anyhow`.
#[derive(Debug)]
struct InputError(String);

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InputError {}

/// Build an input-error `anyhow::Error` (see [`InputError`]).
fn input_error(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(InputError(message.into()))
}

/// Classify a Compass `QueryError` for the caller. An `InvalidParameter` kind is
/// a caller-input problem (an unknown scope, an ambiguous symbol, a limit out of
/// range), not a broken index, so it must be reported plainly rather than with
/// "clear the cache to force a rebuild". Every other kind (corrupt artifact,
/// graph invariant, internal, memory limit) is a genuine engine/index failure and
/// keeps the rebuild guidance.
fn map_engine_error(e: compass_query::QueryError) -> anyhow::Error {
    use compass_query::QueryErrorKind;
    if e.kind() == QueryErrorKind::InvalidParameter {
        input_error(format!("compass_query rejected the request: {}", e.message()))
    } else {
        anyhow!("{}", e)
    }
}

/// Classify a `build_task_context` failure like [`map_engine_error`]: an invalid
/// request (bad target/limits) or an `InvalidParameter` from an inner query is a
/// caller-input problem; the rest (schema, result, encoding) stay engine errors.
fn map_task_context_error(e: compass_core::TaskContextError) -> anyhow::Error {
    use compass_core::TaskContextError;
    match e {
        TaskContextError::InvalidRequest(message) => {
            input_error(format!("compass_query rejected the request: {message}"))
        }
        TaskContextError::Query(err) => map_engine_error(err),
        other => anyhow!("{other}"),
    }
}

/// Run a query through the Compass `CodeQueryEngine`, dispatching on `mode`.
/// Returns a model-ready formatted report.
fn execute_query(
    engine: Option<&compass_query::CodeQueryEngine>,
    intent: QueryIntent,
    params: &CompassQueryInput,
    limit: usize,
    include_heuristic: bool,
    working_dir: &Path,
    graph: Option<&Graph>,
) -> Result<String, anyhow::Error> {
    let limits = CodeQueryLimits {
        // Clamp instead of casting: a pathological usize > u32::MAX must not
        // silently wrap to 0 and violate CodeQueryLimits::is_valid().
        max_nodes: limit.clamp(1, u32::MAX as usize) as u32,
        ..Default::default()
    };

    // The scalar operand for `search` and the single-symbol structural modes is
    // `query` (or `source` for `traverse`'s from-end); `symbols` is the set for
    // `explore` only. `target` names `traverse`'s to-end.
    let query = params.query_operand();
    let source = params
        .source
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| query.clone());
    let target = params.target.clone().unwrap_or_default();

    // Reject an operand-less call with a clear message rather than silently
    // searching for the empty string. `discover` uses its own non-empty check;
    // `traverse` names its source/target explicitly below.
    if intent != QueryIntent::Discover
        && intent != QueryIntent::Traverse
        && intent != QueryIntent::Orientation
    {
        let operand = match intent {
            // `explore` takes a set; any one entry satisfies the check.
            QueryIntent::Explore => params
                .first_symbol()
                .unwrap_or_else(|| query.clone()),
            _ => query.clone(),
        };
        if operand.trim().is_empty() {
            return Err(input_error(format!(
                "compass_query mode={} requires an operand (set `query`{})",
                intent.as_str(),
                if intent == QueryIntent::Explore {
                    " or `symbols`"
                } else {
                    ""
                }
            )));
        }
    }

    // `explore` accepts a set: `symbols` wins (non-blank entries), else the
    // scalar `query`. Computed here (not in the match arm) because the
    // truncation note after rendering needs the requested/queried counts.
    let mut explore_symbols: Vec<String> = match params.symbols.clone() {
        Some(symbols) if symbols.iter().any(|s| !s.trim().is_empty()) => symbols
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect(),
        _ => vec![query.clone()],
    };
    if explore_symbols.is_empty() {
        explore_symbols.push(query.clone());
    }
    let (explore_total, explore_queried) = clamp_explore_symbols(&mut explore_symbols);
    let explore_limits = CodeQueryLimits {
        max_candidates: limits
            .max_candidates
            .max(explore_queried as u32)
            .min(COMPASS_MAX_CANDIDATES as u32),
        max_source_bytes: MAX_VERIFIED_SOURCE_BYTES,
        ..limits
    };

    // `affected` and `orientation` operate on the raw `compass_model::Graph`
    // (Compass's affected traversal and repo-map surfaces live there), not on
    // `CodeQueryEngine`. Dispatch them here, before the engine-response match,
    // and return directly; the engine is `None` for these modes (the caller
    // never opens it). Callers that drive `execute_query` without a graph get a
    // clear "unavailable" message rather than a panic.
    if matches!(intent, QueryIntent::Affected | QueryIntent::Orientation) {
        let Some(graph) = graph else {
            return Err(anyhow!(
                "compass_query mode={} needs the Compass graph, which is not \
                 available in this context",
                intent.as_str()
            ));
        };
        return match intent {
            QueryIntent::Affected => execute_affected(graph, &query, params, limit),
            QueryIntent::Orientation => execute_orientation(graph, params, limit),
            _ => unreachable!("guarded above"),
        };
    }

    // Every remaining mode is engine-backed; the caller opens the engine for
    // them (and passes `None` only for the graph modes handled above).
    let Some(engine) = engine else {
        return Err(anyhow!(
            "compass_query mode={} needs the Compass query engine",
            intent.as_str()
        ));
    };

    // `traverse` diagnostic messages name their endpoints by raw node id; keep the
    // operands and limits so we can resolve them to names after the call (the
    // failed-trail path adds no nodes to the response to label from).
    let traverse_relabel = (intent == QueryIntent::Traverse)
        .then(|| (source.clone(), target.clone(), limits.clone()));

    let response = match intent {
        QueryIntent::Search => engine.search(SearchRequest {
            query: query.clone(),
            limits,
        }),
        QueryIntent::Callers => engine.callers(CallRequest {
            symbol: query.clone(),
            include_heuristic,
            limits,
        }),
        QueryIntent::Callees => engine.callees(CallRequest {
            symbol: query.clone(),
            include_heuristic,
            limits,
        }),
        QueryIntent::Impact => engine.impact(ImpactRequest {
            symbol: query.clone(),
            include_heuristic,
            limits,
        }),
        QueryIntent::Explore => engine.explore(ExploreRequest {
            symbols: explore_symbols,
            root: working_dir.to_string_lossy().into_owned(),
            include_heuristic,
            limits: explore_limits,
        }),
        QueryIntent::Traverse => {
            if source.trim().is_empty() {
                return Err(input_error(
                    "compass_query mode=traverse requires a `source` symbol \
                     (set `source` or `query`)",
                ));
            }
            if target.trim().is_empty() {
                return Err(input_error(
                    "compass_query mode=traverse requires a `target` symbol \
                     (set `target`)",
                ));
            }
            engine.node_trail(NodeTrailRequest {
                source,
                target,
                include_heuristic,
                limits,
            })
        }
        QueryIntent::Context => {
            // `build_task_context` composes its own report and offers no path
            // scope, so a `path` filter would be silently ignored. Reject it
            // rather than return an unscoped packet the caller thinks is scoped.
            if params
                .path
                .as_deref()
                .is_some_and(|p| !p.trim().is_empty())
            {
                return Err(input_error(
                    "compass_query mode=context does not support a `path` filter; \
                     scope the target by qualified name instead",
                ));
            }
            return execute_task_context(engine, &query, limits, working_dir);
        }
        QueryIntent::Discover => {
            return execute_discover(
                engine,
                &query,
                params.path.as_deref(),
                include_heuristic,
                limit,
                working_dir,
            );
        }
        // Handled by the early return above (they need the raw graph, not the
        // engine); unreachable here but required for exhaustiveness.
        QueryIntent::Affected | QueryIntent::Orientation => {
            unreachable!("graph modes return before the engine response match")
        }
    }
    .map_err(map_engine_error)?;

    let mut view = ResponseView::from(response, params.path.as_deref());
    // Compass diagnostics can interpolate raw `sha256:` node ids (a failed
    // `traverse` names its endpoints in the message). Prefer names wherever we
    // can resolve the id: the response's own nodes first, then a companion search
    // of the operands the caller supplied (the failed-trail case adds no nodes).
    // `discover` handles its own diagnostics and returns before this point.
    if !view.diagnostics.is_empty() {
        let mut labels: HashMap<String, String> = view
            .nodes
            .iter()
            .map(|n| (n.id.clone(), n.qualified_name.clone()))
            .collect();
        if let Some((source, target, limits)) = traverse_relabel {
            for operand in [&source, &target] {
                for (id, node) in resolve_candidate_labels(engine, operand, limits.clone()) {
                    labels.entry(id).or_insert(node.qualified_name);
                }
            }
        }
        for diag in &mut view.diagnostics {
            *diag = relabel_ids(diag, &labels);
        }
    }
    // The header names the operand(s) the caller asked about. For `explore` that
    // is the full `symbols` set (summarized); for other modes it is the scalar
    // operand, falling back to `query` when only a free-form query was given.
    let header_target = {
        let target = params.display_target(intent);
        if target.is_empty() {
            query.clone()
        } else {
            target
        }
    };
    let mut rendered = format_view(
        &header_target,
        intent,
        limit,
        params.path.as_deref(),
        &view,
        working_dir,
    );
    // Be explicit when an oversized explore set was trimmed to Compass's cap, so
    // the model knows not every requested symbol was queried.
    if intent == QueryIntent::Explore && explore_total > explore_queried {
        rendered.push_str(&format!(
            "\n**Note:** explore received {explore_total} symbols; only the first \
             {explore_queried} were queried (Compass cap).\n"
        ));
    }
    Ok(rendered)
}

/// Map candidate node ids (from a `context` ambiguous/not-found target) to human
/// labels. Compass's task-context target carries only `SearchHit`s (node id,
/// score), so we run a companion `search` for the same target and reuse the
/// resolved nodes' qualified name, kind, roles, and file. Returns an empty map
/// if the companion search fails, in which case the caller falls back to raw ids.
fn resolve_candidate_labels(
    engine: &compass_query::CodeQueryEngine,
    target: &str,
    limits: CodeQueryLimits,
) -> HashMap<String, NodeView> {
    let request = SearchRequest {
        query: target.to_string(),
        limits,
    };
    match engine.search(request) {
        Ok(response) => response
            .nodes
            .iter()
            .map(|node| {
                (
                    node.id.clone(),
                    NodeView {
                        id: node.id.clone(),
                        name: node.name.clone(),
                        qualified_name: node.qualified_name.clone(),
                        kind: node.kind.as_str().to_string(),
                        roles: node.roles.iter().map(|r| role_name(*r)).collect(),
                        file: node.source.as_ref().map(|s| s.file.clone()),
                        source: node.source.clone(),
                    },
                )
            })
            .collect(),
        Err(_) => HashMap::new(),
    }
}

/// Replace every known node id inside `text` with its qualified name. Compass
/// diagnostics interpolate raw ids (e.g. a failed `traverse` names its endpoints
/// as `sha256:...`); substituting readable names keeps the diagnostic useful.
/// Ids are matched whole and longest-first so a shorter id cannot corrupt a
/// longer one that contains it as a prefix (ids are fixed-length hashes, but the
/// ordering makes the intent explicit and safe for any scheme).
fn relabel_ids(text: &str, labels: &HashMap<String, String>) -> String {
    if labels.is_empty() {
        return text.to_string();
    }
    let mut ids: Vec<&String> = labels.keys().collect();
    ids.sort_by(|a, b| b.len().cmp(&a.len()));
    let mut out = text.to_string();
    for id in ids {
        if out.contains(id.as_str()) {
            out = out.replace(id.as_str(), &labels[id]);
        }
    }
    out
}

/// Like [`compass_model::NodeRecord::display_label`] but never falls back to the
/// raw node id: a node with no label/name/qualified name/signature/file/location
/// would otherwise render as its opaque `sha256:` identity. Falls back to the
/// node kind, so a degenerate node is still identifiable without leaking an id.
fn graph_node_display(node: &compass_model::NodeRecord) -> String {
    let display = node.display_label();
    // `display_label` falls back to the raw node id for a degenerate node. Rather
    // than pattern-match specific id schemes (which drift), reject the fallback
    // directly: the result is an id when it equals the node's id, or when it
    // looks like a scheme-qualified digest (`word:<hex>`).
    if display == node.id.as_str() || is_scheme_qualified_digest(&display) {
        return format!("({})", node.kind_name());
    }
    display
}

/// True when `value` looks like an opaque `<scheme>:<hex>` node id (e.g.
/// `sha256:...`), used to reject an id that slipped through as a display label.
fn is_scheme_qualified_digest(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once(':') else {
        return false;
    };
    // Real content digests are sha256/md5/sha1/blake3 (>= 32 hex chars); a
    // shorter run is more likely a label like `http:...` than an id, so require
    // 32 to avoid misfiring on a legitimate label.
    scheme.len() >= 3
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && rest.len() >= 32
        && rest.chars().all(|c| c.is_ascii_hexdigit())
}

/// Render a resolved node as a one-line label: `` `qualified_name` (kind, file)
/// [roles] `` (falling back to the plain `name` when the qualified name is
/// empty). The file and roles are omitted when absent. Used for `context`
/// headers and candidate lists so neither leaks a raw `sha256:` node id.
fn format_node_label(view: &NodeView) -> String {
    let roles = if view.roles.is_empty() {
        String::new()
    } else {
        format!(" [{}]", view.roles.join(", "))
    };
    // Prefer the qualified name; fall back to the plain name when it is empty
    // (matching the search-hit render path) so a label is never blank.
    let display = if view.qualified_name.is_empty() {
        &view.name
    } else {
        &view.qualified_name
    };
    match &view.file {
        Some(file) => format!("`{display}` ({}, {}){roles}", view.kind, file),
        None => format!("`{display}` ({}){roles}", view.kind),
    }
}

/// Follow relations *inbound* to a seed and render everyone who depends on it —
/// Compass's `affected` query, which is the reverse of `impact` (that walks
/// outbound callers). Uses `resolve_seed`/`affected_nodes` from `compass-query`
/// over the raw `compass_model::Graph`, then renders the report itself (so rows
/// can be bounded, `path` applied, and in-scope counts reported).
///
/// The graph is provided by the caller, which loads it with
/// [`Graph::load_for_affected`] (a bounded compact projection: it drops
/// attributes irrelevant to affected traversal and caches the projection next to
/// `graph.json`) under `spawn_blocking` + the project flock, so only this mode
/// pays for the second load and no other mode is slowed. jcode bounds the header
/// and the number of listed rows, and applies an optional `path` filter (Compass's
/// traversal has no path scope) rather than silently ignoring it.
fn execute_affected(
    graph: &Graph,
    query: &str,
    params: &CompassQueryInput,
    limit: usize,
) -> Result<String, anyhow::Error> {
    // Relations default to Compass's canonical affected set; blank entries are
    // dropped, and an all-blank override falls back to the default rather than
    // silently following no relations (which would report "nothing affected").
    let relations: Vec<String> = match params.relations.clone() {
        Some(list) if list.iter().any(|r| !r.trim().is_empty()) => list
            .into_iter()
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .collect(),
        _ => compass_query::DEFAULT_AFFECTED_RELATIONS
            .iter()
            .map(|r| (*r).to_string())
            .collect(),
    };
    // Honor an explicit depth exactly (including 0, which Compass treats as "no
    // traversal"); only default when the caller omitted it. Clamping would
    // silently override the caller's request.
    let depth = params.depth.unwrap_or(DEFAULT_AFFECTED_DEPTH);

    // Resolve the seed ourselves so an unmatched operand reads as a clear,
    // input-shaped error (and a "not found" note rather than a bare body).
    let Some(seed) = compass_query::resolve_seed(graph, query) else {
        return Ok(format!(
            "No unique node matched `{}` in the Compass graph. Qualify the name \
             (e.g. a path or `crate::module::symbol`) or run `mode=search` to \
             find the right node.\n",
            header_label(query)
        ));
    };

    // Apply an optional `path` filter to hits after traversal: Compass's
    // `affected_nodes` has no path scope, so filtering post-hoc is what keeps a
    // scoped request scoped rather than silently ignoring `path`.
    let path_filter = params
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());

    let hits = compass_query::affected_nodes(graph, seed, &relations, depth);
    let seed_label = graph_node_display(graph.node(seed));

    // Count the hits that survive the `path` filter separately from those the
    // limit cuts, so the header count is the *in-scope* total and the truncation
    // note attributes the shortfall to the right cause.
    let mut in_scope = 0usize;
    let mut dropped_by_path = 0usize;
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for hit in &hits {
        let node = graph.node(hit.node);
        let file = node.string("source_file");
        let file = file.trim();
        if let Some(filter) = path_filter
            && !file.is_empty()
            && !path_matches_filter(filter, file)
        {
            dropped_by_path += 1;
            continue;
        }
        in_scope += 1;
        if rows.len() >= limit {
            continue;
        }
        let location = node.string("source_location");
        let location = location.trim();
        let where_ = match (file.is_empty(), location.is_empty()) {
            (true, _) => "(no file)".to_string(),
            (false, true) => file.to_string(),
            (false, false) => format!("{file}:{location}"),
        };
        rows.push((
            header_label(&graph_node_display(node)),
            hit.relation.clone(),
            where_,
        ));
    }

    let mut out = String::new();
    out.push_str(&format!("# Compass affected: {}\n\n", header_label(query)));
    out.push_str(&format!("**Mode:** {}\n", QueryIntent::Affected.as_str()));
    out.push_str(&format!("**Seed:** {}\n", header_label(&seed_label)));
    // Bound like every other header line: a caller could pass many long relations.
    // The cap clears the full default set (149 chars) so the common case is not
    // truncated, while still bounding a pathological override.
    out.push_str(&format!(
        "**Relations:** {}\n",
        summarize_list(&relations, MAX_RELATIONS_CHARS)
    ));
    out.push_str(&format!("**Depth:** {depth}\n"));
    out.push_str(&format!("**Limit:** {limit}\n"));
    if let Some(p) = path_filter {
        out.push_str(&format!("**Path filter:** {p}\n"));
    }
    out.push('\n');

    if hits.is_empty() {
        out.push_str(&format!(
            "No affected nodes. Nothing depends on `{}` through the requested \
             relations within depth {depth}.\n",
            header_label(&seed_label)
        ));
        return Ok(out);
    }
    if in_scope == 0 {
        // Dependents exist but the filter excluded every one of them.
        out.push_str(&format!(
            "No affected nodes in the requested path. All {dropped_by_path} \
             dependent(s) fell outside the `path` filter; widen or drop it.\n"
        ));
        return Ok(out);
    }

    out.push_str(&format!("**{in_scope} affected node(s)**\n\n"));
    for (display, relation, where_) in &rows {
        out.push_str(&format!("- {display} [{relation}] {where_}\n"));
    }
    if in_scope > rows.len() {
        out.push_str(&format!(
            "\n**Note:** showing {} of {in_scope} affected node(s) (limit {limit}).\n",
            rows.len()
        ));
    }
    if dropped_by_path > 0 {
        out.push_str(&format!(
            "\n**Note:** {dropped_by_path} affected node(s) fell outside the `path` filter.\n"
        ));
    }
    Ok(out)
}

/// Compose a bounded, community-free repository map so a session can orient in
/// an unfamiliar repo before drilling in with `search`/`context`.
///
/// jcode builds the index with `no_cluster = true` / `no_viz = true` (see
/// "Build tuning" in docs/COMPASS.md), so `graph.json` carries no community
/// column and there is no `analysis.json` to summarize. Rather than pay a
/// clustering pass on every cold build for every user just to serve this one
/// mode, the map is derived from the graph jcode already has: file/symbol
/// counts, top-level directories by file count, and the most connected
/// ("hottest") symbols. Communities can be re-enabled later if a
/// community-scoped map proves worth the build cost.
fn execute_orientation(
    graph: &Graph,
    params: &CompassQueryInput,
    limit: usize,
) -> Result<String, anyhow::Error> {
    let path_filter = params
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());

    // First pass: gather distinct source files and symbol candidates. Attribute
    // reads are cheap; the heavy work is `degree`, deferred to a second pass so
    // only candidate symbols pay for it. Files are deduped by path (not counted
    // as file-kind nodes): a graph can have source-anchored nodes but no
    // dedicated file node, so counting file-kind nodes would under-report.
    let mut files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut symbol_count = 0usize;
    let mut candidates: Vec<(NodeIndex, String)> = Vec::new();
    for (index, node) in graph.nodes() {
        let file = node.string("source_file");
        let file = file.trim();
        if let Some(filter) = path_filter {
            // A node with no file is kept only when no filter is active.
            if file.is_empty() || !path_matches_filter(filter, file) {
                continue;
            }
        }
        if !file.is_empty() {
            files.insert(file.to_string());
        }
        // A file-level node (or a node with no source anchor, which cannot be
        // placed) is not a symbol; everything else is one.
        let kind = node.kind_name();
        if kind == ORIENTATION_FILE_KIND || file.is_empty() {
            continue;
        }
        symbol_count += 1;
        // Defer the degree walk to a second pass. Use the id-free display label:
        // `node.label()` falls back to the opaque node id for a degenerate node,
        // which must never be printed.
        let label = graph_node_display(node);
        if !label.trim().is_empty() {
            candidates.push((index, label));
        }
    }
    let file_count = files.len();

    // Top-level directories by file count, derived from the same distinct-file
    // set so the directory tally stays consistent with the headline count.
    let mut dir_files: HashMap<String, usize> = HashMap::new();
    for file in &files {
        *dir_files.entry(top_level_dir(file)).or_insert(0) += 1;
    }

    let mut scored: Vec<(usize, String)> = candidates
        .into_iter()
        .map(|(index, label)| (graph.degree(index), label))
        .collect();
    // Sort by degree desc, then label asc (deterministic ties).
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let mut dirs: Vec<(String, usize)> = dir_files.into_iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut out = String::new();
    out.push_str("# Compass orientation: repository map\n\n");
    out.push_str(&format!("**Mode:** {}\n", QueryIntent::Orientation.as_str()));
    out.push_str(&format!("**Limit:** {limit}\n"));
    if let Some(p) = path_filter {
        out.push_str(&format!("**Path filter:** {p}\n"));
    }
    out.push('\n');
    out.push_str(&format!(
        "**Graph:** {file_count} file(s), {symbol_count} symbol(s)\n\n"
    ));

    out.push_str("## Top-level directories\n\n");
    if dirs.is_empty() {
        out.push_str("(none)\n\n");
    } else {
        for (dir, count) in dirs.iter().take(ORIENTATION_DIR_ROWS) {
            out.push_str(&format!("- `{dir}` — {count} file(s)\n"));
        }
        if dirs.len() > ORIENTATION_DIR_ROWS {
            out.push_str(&format!(
                "- … {} more\n",
                dirs.len() - ORIENTATION_DIR_ROWS
            ));
        }
        out.push('\n');
    }

    out.push_str("## Hottest symbols (most connections)\n\n");
    if scored.iter().all(|(degree, _)| *degree == 0) {
        out.push_str("(none)\n\n");
    } else {
        let rows = limit.min(ORIENTATION_SYMBOL_ROWS);
        for (degree, label) in scored.iter().take(rows) {
            out.push_str(&format!("- {} — {degree} connection(s)\n", header_label(label)));
        }
        out.push('\n');
    }

    out.push_str(
        "> This map is derived from the graph (no community clustering): Co\
         mpass is built with `no_cluster`/`no_viz`, so no community or analysis \
         artifact exists to summarize. Drill in with `mode=search`, `mode=context`, \
         or `mode=affected`.\n",
    );
    Ok(out)
}

/// Compose a task-oriented context packet with Compass's `build_task_context`.
/// One call surfaces the declaration + verified source, exact callers/callees,
/// related tests, and bounded transitive impact for a target, replacing the
/// several file reads an agent would otherwise do to orient around a symbol.
fn execute_task_context(
    engine: &compass_query::CodeQueryEngine,
    target: &str,
    limits: CodeQueryLimits,
    working_dir: &Path,
) -> Result<String, anyhow::Error> {
    use compass_core::{
        build_task_context, TaskContextIntent, TaskContextLimits, TaskContextRequest,
        TaskContextSectionKind, TaskContextTarget,
    };

    if target.trim().is_empty() {
        return Err(input_error("compass_query mode=context requires a target symbol"));
    }

    let request = TaskContextRequest {
        intent: TaskContextIntent::Explain,
        target: target.to_string(),
        repository_root: working_dir.to_string_lossy().into_owned(),
        limits: TaskContextLimits {
            query: CodeQueryLimits {
                // `context` already fans out into several sub-queries; keep each
                // bounded but allow the full neighborhood rather than truncating.
                max_source_bytes: MAX_VERIFIED_SOURCE_BYTES,
                ..limits
            },
            ..Default::default()
        },
    };

    let context = build_task_context(engine, &request, &[]).map_err(map_task_context_error)?;

    let mut out = String::new();
    out.push_str(&format!("# Compass context: {}\n\n", header_label(target)));
    out.push_str(&format!("**Target:** {}\n", header_label(target)));
    match &context.target {
        TaskContextTarget::Exact { node_id } => {
            // Compass reports the raw node id; resolve it to a name/kind/file via
            // a companion search so the header is readable (the sections below
            // already name the declaration, but this line need not leak a hash).
            let labels = resolve_candidate_labels(engine, target, limits);
            match labels.get(node_id) {
                Some(view) => {
                    out.push_str(&format!("**Resolved node:** {}\n", format_node_label(view)));
                }
                None => out.push_str(&format!("**Resolved node:** {node_id}\n")),
            }
        }
        TaskContextTarget::Ambiguous { candidates } | TaskContextTarget::NotFound { candidates } => {
            // Compass returns candidate *ids* here, not names, so resolve them
            // through a companion search for the same target. Raw `sha256:` ids
            // are useless to the model; names/kinds/files let it pick the right
            // symbol (and are what "refine with a qualified name" needs).
            let labels = resolve_candidate_labels(engine, target, limits);
            if matches!(context.target, TaskContextTarget::Ambiguous { .. }) {
                out.push_str(&format!(
                    "**Ambiguous target:** {} candidate(s); refine with a qualified \
                     name or file:\n",
                    candidates.len()
                ));
            } else if candidates.is_empty() {
                out.push_str("**No exact match.** No similarly named symbols were found.\n");
            } else {
                out.push_str("**No exact match.** Closest candidates:\n");
            }
            for candidate in candidates.iter().take(MAX_CANDIDATE_ROWS) {
                match labels.get(&candidate.node_id) {
                    Some(candidate_view) => {
                        out.push_str(&format!("- {}\n", format_node_label(candidate_view)));
                    }
                    // Fall back to the raw id only when the companion search did
                    // not surface this candidate.
                    None => out.push_str(&format!("- {}\n", candidate.node_id)),
                }
            }
            out.push('\n');
            return Ok(out);
        }
    }

    // One shared set (and one shared file reader) across every section so a file
    // that appears in several sections is read from disk and dumped only once.
    let mut rendered_files = RenderedFiles::default();
    let mut cache = SourceCache::default();
    for section in &context.sections {
        let title = match section.kind {
            TaskContextSectionKind::DeclarationSource => "Declaration + source",
            TaskContextSectionKind::ExactCallers => "Exact callers",
            TaskContextSectionKind::ExactCallees => "Exact callees",
            TaskContextSectionKind::ImplementationType => "Implementation / type",
            TaskContextSectionKind::RelatedTests => "Related tests",
            TaskContextSectionKind::TransitiveImpact => "Transitive impact",
            TaskContextSectionKind::Framework => "Framework",
        };
        out.push_str(&format!("\n## {title}\n\n"));
        let view = ResponseView::from(section.evidence.clone(), None);
        render_view_body_inner(
            &mut out,
            &view,
            working_dir,
            MAX_SNIPPET_ROWS,
            false,
            &mut rendered_files,
            &mut cache,
        );
    }

    if !context.omissions.is_empty() {
        out.push_str("\n**Omissions:**\n");
        for omission in context.omissions.iter().take(MAX_CANDIDATE_ROWS) {
            out.push_str(&format!("- {}: {}\n", omission.category, omission.reason));
        }
    }
    Ok(out)
}

/// Route a natural-language question through Compass discovery (`CodeQueryEngine
/// ::discover`). Discovery selects the most likely seed symbols for the question
/// and returns a bounded structural neighborhood around them, so a session can
/// start from a prose question ("how does auth reach the DB") instead of first
/// guessing symbol names. Each seed is rendered with its source, then the
/// neighborhood edges are listed.
fn execute_discover(
    engine: &compass_query::CodeQueryEngine,
    question: &str,
    path_filter: Option<&str>,
    include_heuristic: bool,
    limit: usize,
    working_dir: &Path,
) -> Result<String, anyhow::Error> {
    use compass_model::query_contract::{
        DiscoveryLimits, DiscoveryQueryRequest, DiscoveryScope, DiscoveryScopeKind,
        MAX_DISCOVERY_SEEDS,
    };

    if question.trim().is_empty() {
        return Err(input_error("compass_query mode=discover requires a non-empty query"));
    }

    // A `path` filter becomes a `Source` discovery scope, so a question can be
    // constrained to a file or directory (e.g. only search within one crate).
    let scope = match path_filter {
        Some(path) if !path.trim().is_empty() => vec![DiscoveryScope {
            kind: DiscoveryScopeKind::Source,
            value: path.trim().to_string(),
        }],
        _ => Vec::new(),
    };

    let response = engine
        .discover(DiscoveryQueryRequest {
            question: question.to_string(),
            direction: Default::default(),
            relation_contexts: Vec::new(),
            scope,
            traversal: Default::default(),
            include_heuristic,
            limits: DiscoveryLimits {
                // Compass caps discovery seeds; clamp within the valid range.
                max_seeds: (limit.clamp(1, u32::MAX as usize) as u32).min(MAX_DISCOVERY_SEEDS),
                ..Default::default()
            },
        })
        .map_err(map_engine_error)?;

    let mut out = String::new();
    out.push_str(&format!("# Compass discover: {}\n\n", header_label(question)));
    out.push_str("**Mode:** discover\n");
    out.push_str(&format!("**Limit:** {limit}\n"));
    if let Some(path) = path_filter.filter(|p| !p.trim().is_empty()) {
        out.push_str(&format!("**Path filter:** {path}\n"));
    }
    out.push('\n');

    // Discovery returns node records (id -> qualified name) for the seeds and
    // their neighborhood; resolve ids to labels so the report names symbols
    // rather than opaque digests. Fall back to the raw id when a node is absent.
    // Shared with diagnostic relabeling below.
    let labels: HashMap<String, String> = response
        .nodes
        .iter()
        .map(|n| (n.id.clone(), n.qualified_name.clone()))
        .collect();
    let label = |id: &str| -> String {
        labels
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    };

    // Compass caps discovery at MAX_DISCOVERY_SEEDS seeds regardless of the
    // request, so note when the caller asked for more than it can return.
    if limit > MAX_DISCOVERY_SEEDS as usize {
        out.push_str(&format!(
            "**Note:** discovery returns at most {MAX_DISCOVERY_SEEDS} seeds \
             (Compass cap); requested limit was {limit}.\n\n"
        ));
    }

    if response.seeds.is_empty() {
        out.push_str("No seeds matched. Try more specific terms or `mode=search`.\n\n");
    } else {
        out.push_str(&format!("**Seeds ({}):**\n", response.seeds.len()));
        let mut cache = SourceCache::default();
        for seed in response.seeds.iter().take(MAX_CANDIDATE_ROWS) {
            let file = seed.source.as_ref().map(|s| s.file.clone());
            match &file {
                Some(file) => out.push_str(&format!(
                    "- {} (score {}, {file})\n",
                    label(&seed.node_id),
                    seed.score
                )),
                None => out.push_str(&format!(
                    "- {} (score {})\n",
                    label(&seed.node_id),
                    seed.score
                )),
            }
            for alt in seed.alternatives.iter().take(3) {
                out.push_str(&format!("  - alt: {}\n", alt.qualified_name));
            }
            if let Some(anchor) = &seed.source
                && let Some(text) = cache.snippet(working_dir, anchor)
            {
                out.push_str("\n```\n");
                out.push_str(&text);
                out.push_str("```\n");
            }
        }
        out.push('\n');
    }

    if !response.edges.is_empty() {
        out.push_str(&format!("**Neighborhood ({}):**\n", response.edges.len()));
        for edge in response.edges.iter().take(MAX_EDGE_ROWS) {
            out.push_str(&format!(
                "- {} --{}--> {}\n",
                label(&edge.source),
                edge.kind.as_str(),
                label(&edge.target)
            ));
        }
        if response.edges.len() > MAX_EDGE_ROWS {
            out.push_str(&format!(
                "- … {} more edge(s) omitted\n",
                response.edges.len() - MAX_EDGE_ROWS
            ));
        }
        out.push('\n');
    }

    if response.truncated {
        out.push_str("**Note:** discovery results were truncated by Compass bounds.\n\n");
    }
    if !response.diagnostics.is_empty() {
        out.push_str("**Diagnostics:**\n");
        // Compass discovery diagnostics can interpolate raw `sha256:` seed ids
        // (e.g. "Seed <id> is ambiguous"); relabel with the seed names resolved
        // above, matching the structural report.
        for diag in response.diagnostics.iter().take(MAX_CANDIDATE_ROWS) {
            out.push_str(&format!("- {}\n", relabel_ids(&diag.message, &labels)));
        }
    }
    Ok(out)
}

/// Maximum number of results that get a full fenced source snippet. Beyond
/// this, results are shown as ranked name/file/kind rows (still useful, but
/// without a code block) so a single query cannot balloon the model context
/// window with many fences. Callers can raise the tool `limit` to surface more
/// however; the cap bounds the *context*, not the result count.
const MAX_SNIPPET_ROWS: usize = 8;

/// Cap on ambiguous/omission lists so a single report section cannot grow
/// without bound.
const MAX_CANDIDATE_ROWS: usize = 12;

/// Bound on Compass's own digest-verified source resolution for explore/context,
/// so a large neighborhood cannot pull unbounded file bytes into one report.
const MAX_VERIFIED_SOURCE_BYTES: u64 = 512 * 1024;

/// One ranked search result, plus the source anchor and kind needed to render a
/// code snippet alongside it.
#[derive(Clone)]
struct ResultRow {
    name: String,
    file: Option<String>,
    score: f64,
    matched: Vec<String>,
    source: Option<SourceAnchor>,
    kind: String,
}

impl ResponseView {
    /// Normalize a Compass `CodeQueryResponse` into the render-ready view,
    /// applying the optional path filter to search hits and to structural nodes.
    fn from(response: CodeQueryResponse, path_filter: Option<&str>) -> Self {
        let node_view = |node: &compass_model::query_contract::QueryNode| NodeView {
            id: node.id.clone(),
            name: node.name.clone(),
            qualified_name: node.qualified_name.clone(),
            kind: node.kind.as_str().to_string(),
            roles: node.roles.iter().map(|r| role_name(*r)).collect(),
            file: node.source.as_ref().map(|s| s.file.clone()),
            source: node.source.clone(),
        };

        let kept_ids: std::collections::HashSet<&str> = match path_filter {
            Some(filter) => response
                .nodes
                .iter()
                .filter(|n| {
                    n.source
                        .as_ref()
                        .map(|s| path_matches_filter(filter, &s.file))
                        .unwrap_or(false)
                })
                .map(|n| n.id.as_str())
                .collect(),
            None => response.nodes.iter().map(|n| n.id.as_str()).collect(),
        };

        let mut hits: Vec<ResultRow> = Vec::new();
        for hit in &response.results {
            if !kept_ids.contains(hit.node_id.as_str()) {
                continue;
            }
            let Some(node) = response.nodes.iter().find(|n| n.id == hit.node_id) else {
                continue;
            };
            hits.push(ResultRow {
                name: node.qualified_name.clone(),
                file: node.source.as_ref().map(|s| s.file.clone()),
                score: hit.score,
                matched: hit.matched_fields.clone(),
                source: node.source.clone(),
                kind: node.kind.as_str().to_string(),
            });
        }

        let nodes: Vec<NodeView> = response
            .nodes
            .iter()
            .filter(|n| kept_ids.contains(n.id.as_str()))
            .map(node_view)
            .collect();

        let edges: Vec<EdgeView> = response
            .edges
            .iter()
            .filter(|e| kept_ids.contains(e.source.as_str()) && kept_ids.contains(e.target.as_str()))
            .map(|e| EdgeView {
                source: e.source.clone(),
                target: e.target.clone(),
                kind: e.kind.as_str().to_string(),
            })
            .collect();

        // An edge whose endpoint is a node Compass returned but the filter
        // excluded: the relationship exists, just outside the filtered view.
        let filter_dropped_edges = path_filter.is_some() && {
            let returned_ids: std::collections::HashSet<&str> =
                response.nodes.iter().map(|n| n.id.as_str()).collect();
            response.edges.iter().any(|e| {
                !(kept_ids.contains(e.source.as_str()) && kept_ids.contains(e.target.as_str()))
                    && (returned_ids.contains(e.source.as_str())
                        || returned_ids.contains(e.target.as_str()))
            })
        };

        let paths: Vec<PathView> = response
            .paths
            .iter()
            .filter(|p| {
                // Keep a path only when the filter kept all of its nodes, so a
                // filtered report never shows a chain with dropped/hidden nodes
                // (which would fall back to raw ids).
                p.node_ids
                    .iter()
                    .all(|id| kept_ids.contains(id.as_str()))
            })
            .map(|p| PathView {
                node_ids: p.node_ids.clone(),
                weakest_confidence: p.weakest_confidence.as_str().to_string(),
            })
            .collect();

        let files: Vec<FileView> = response
            .files
            .iter()
            .filter(|f| {
                path_filter
                    .map(|filter| path_matches_filter(filter, &f.path))
                    .unwrap_or(true)
            })
            .map(|f| FileView {
                path: f.path.clone(),
                digest: f.content_digest.clone(),
                source: f.source.clone(),
                truncated: f.truncated,
            })
            .collect();

        let diagnostics = response
            .diagnostics
            .iter()
            .map(|d| d.message.clone())
            .collect();

        let diagnostic_codes: std::collections::BTreeSet<_> =
            response.diagnostics.iter().map(|d| d.code).collect();

        // A path filter that removed nodes/results/source Compass did return makes
        // an empty report a different situation from "nothing was found at all".
        let filter_present = path_filter.is_some();
        let filter_dropped_nodes = !response.nodes.is_empty() && kept_ids.is_empty();
        let filter_dropped_hits = !response.results.is_empty() && hits.is_empty();
        let filter_dropped_files = !response.files.is_empty() && files.is_empty();
        let filtered_out = filter_present
            && (filter_dropped_nodes || filter_dropped_hits || filter_dropped_files);

        Self {
            hits,
            nodes,
            edges,
            paths,
            files,
            truncated: response.truncated,
            diagnostics,
            filtered_out,
            filter_dropped_edges,
            diagnostic_codes,
        }
    }
}

/// Render the model-ready report. Search renders ranked hits with source
/// snippets; structural intents render the resolved node set, the edges between
/// them, any traversed paths, and (for explore/context) the digest-verified
/// source Compass already read. All source reads share one [`SourceCache`].
fn format_view(
    query: &str,
    intent: QueryIntent,
    limit: usize,
    path_filter: Option<&str>,
    view: &ResponseView,
    working_dir: &Path,
) -> String {
    let mut output = String::new();
    output.push_str(&format!("# Compass query: {}\n\n", header_label(query)));
    output.push_str(&format!("**Mode:** {}\n", intent.as_str()));
    output.push_str(&format!("**Limit:** {}\n", limit));
    if let Some(p) = path_filter {
        output.push_str(&format!("**Path filter:** {}\n", p));
    }
    output.push('\n');

    let mut rendered_files = RenderedFiles::default();
    let mut cache = SourceCache::default();
    render_view_body_inner(
        &mut output,
        view,
        working_dir,
        MAX_SNIPPET_ROWS,
        matches!(intent, QueryIntent::Search),
        &mut rendered_files,
        &mut cache,
    );
    // `callers`/`callees`/`impact`/`explore` list the resolved symbols even when
    // the graph has no relationships for them (only the seed is resolved). Say so
    // explicitly, so a lone "Resolved symbols (1)" is not misread as results. Only
    // fire when nothing else rendered: an `explore`/`context` report may carry
    // verified source for the seed even with no relationships.
    let structural_neighborhood = matches!(
        intent,
        QueryIntent::Callers | QueryIntent::Callees | QueryIntent::Impact | QueryIntent::Explore
    );
    if structural_neighborhood
        && !view.nodes.is_empty()
        && view.edges.is_empty()
        && view.paths.is_empty()
        && view.files.is_empty()
        && view.diagnostics.is_empty()
    {
        if view.filter_dropped_edges {
            // Relationships exist but their other endpoint fell outside the
            // `path` filter, so do not claim the symbol has none.
            output.push_str(
                "No related symbols in the requested path. Relationships exist but \
                 fall outside the `path` filter; widen or drop it.\n\n",
            );
        } else {
            output.push_str(match intent {
                QueryIntent::Callers => "No callers found for this symbol.\n\n",
                QueryIntent::Callees => "No callees found for this symbol.\n\n",
                QueryIntent::Impact => "No impacted symbols found for this symbol.\n\n",
                _ => "No related symbols found in this symbol's neighborhood.\n\n",
            });
        }
    }
    output
}

/// Body renderer shared by `format_view` and the `context` section loop.
/// `snippet_rows` bounds how many ranked hits get a fenced source block. The
/// `is_search` flag is set for a keyword search: such a report always prints its
/// `Found N result(s)` header (even for zero hits, preserving the historical
/// machine-readable contract callers assert on), whereas a structural view only
/// prints this header when it actually has hits.
///
/// `rendered_files` accumulates the source-file paths already emitted with a
/// fenced body across sibling calls (the `context` intent renders one view per
/// section), so the same file is never dumped more than once in a report.
/// `cache` is shared across those sibling calls for the same reason, so a file
/// referenced by several `context` sections is read from disk only once.
fn render_view_body_inner(
    output: &mut String,
    view: &ResponseView,
    working_dir: &Path,
    snippet_rows: usize,
    is_search: bool,
    rendered_files: &mut RenderedFiles,
    cache: &mut SourceCache,
) {
    if is_search || !view.hits.is_empty() {
        output.push_str(&format!("**Found {} result(s)**\n\n", view.hits.len()));
        for (i, row) in view.hits.iter().enumerate() {
            output.push_str(&format!("## {}. {}\n\n", i + 1, row.name));
            if let Some(file) = &row.file {
                output.push_str(&format!("**File:** {}\n", file));
            }
            output.push_str(&format!("**Kind:** {}\n", row.kind));
            output.push_str(&format!("**Score:** {:.3}\n", row.score));
            if !row.matched.is_empty() {
                output.push_str(&format!("**Matched:** {}\n", row.matched.join(", ")));
            }
            if i < snippet_rows
                && let Some(anchor) = &row.source
                && let Some(text) = cache.snippet(working_dir, anchor)
            {
                output.push_str("\n```\n");
                output.push_str(&text);
                output.push_str("```\n");
            }
            output.push('\n');
        }
    }

    if !view.edges.is_empty() {
        output.push_str(&format!("**Relationships ({}):**\n", view.edges.len()));
        for edge in view.edges.iter().take(MAX_EDGE_ROWS) {
            output.push_str(&format!(
                "- {} --{}--> {}\n",
                view.label(&edge.source),
                edge.kind,
                view.label(&edge.target)
            ));
        }
        if view.edges.len() > MAX_EDGE_ROWS {
            output.push_str(&format!(
                "- … {} more edge(s) omitted\n",
                view.edges.len() - MAX_EDGE_ROWS
            ));
        }
        output.push('\n');
    }

    if !view.paths.is_empty() {
        output.push_str(&format!("**Paths ({}):**\n", view.paths.len()));
        for (i, path) in view.paths.iter().enumerate().take(MAX_PATH_ROWS) {
            let chain = path
                .node_ids
                .iter()
                .map(|id| view.label(id))
                .collect::<Vec<_>>()
                .join(" -> ");
            output.push_str(&format!(
                "{}. {} _(weakest confidence: {})_\n",
                i + 1,
                chain,
                path.weakest_confidence
            ));
        }
        if view.paths.len() > MAX_PATH_ROWS {
            output.push_str(&format!(
                "- … {} more path(s) omitted\n",
                view.paths.len() - MAX_PATH_ROWS
            ));
        }
        output.push('\n');
    }

    // Structural results that are not search hits still deserve their resolved
    // declarations shown, so a `callers`/`callees`/`impact` report names and
    // locates each node. Source is only rendered for nodes whose file did not
    // already come back digest-verified below.
    if view.hits.is_empty() && !view.nodes.is_empty() {
        output.push_str(&format!("**Resolved symbols ({}):**\n", view.nodes.len()));
        for node in view.nodes.iter().take(MAX_NODE_ROWS) {
            output.push_str(&format!("- {}\n", format_node_label(node)));
        }
        if view.nodes.len() > MAX_NODE_ROWS {
            output.push_str(&format!(
                "- … {} more node(s) omitted\n",
                view.nodes.len() - MAX_NODE_ROWS
            ));
        }
        output.push('\n');

        // Show the declaration source for the first few resolved symbols so a
        // callers/callees/impact answer includes real code, not just names.
        // Later nodes stay as lean rows so a wide structural query cannot tile
        // many fences (or read many files) into one report. A node whose file is
        // a *fresh* digest-verified file in this view, or already rendered in a
        // sibling `context` section, is skipped so the file is not emitted twice.
        // A stale verified file (source `None`) is not deduped: the node snippet
        // is then the only fresh source the report can offer for that file.
        let verified_fresh: std::collections::HashSet<&str> = view
            .files
            .iter()
            .filter(|f| f.source.is_some())
            .map(|f| f.path.as_str())
            .collect();
        for node in view.nodes.iter().take(MAX_NODE_SNIPPETS) {
            let Some(anchor) = &node.source else { continue };
            if verified_fresh.contains(anchor.file.as_str())
                || rendered_files.contains_path(&anchor.file)
            {
                continue;
            }
            let Some(text) = cache.snippet(working_dir, anchor) else {
                continue;
            };
            let label = if node.qualified_name.is_empty() {
                &node.name
            } else {
                &node.qualified_name
            };
            output.push_str(&format!("`{label}`:\n\n```\n{text}```\n\n"));
        }
    }

    // Digest-verified source that Compass resolved from disk (explore/context).
    // This is the primary "read fewer files" win: the model gets real source for
    // the neighborhood without issuing separate `read` calls. `rendered_files` is
    // shared across the sibling section views of a `context` report, so a file
    // that appears in several sections (e.g. the declaration file also carries an
    // implementation relation) is dumped only once.
    if !view.files.is_empty() {
        for file in &view.files {
            if rendered_files.len() >= MAX_SOURCE_FILES
                && !rendered_files.contains_body(&file.path, &file.digest)
            {
                break;
            }
            if !rendered_files.insert(&file.path, &file.digest) {
                continue;
            }
            match &file.source {
                Some(text) => {
                    output.push_str(&format!("### {}\n\n", file.path));
                    output.push_str("```\n");
                    output.push_str(text);
                    if !text.ends_with('\n') {
                        output.push('\n');
                    }
                    if file.truncated {
                        output.push_str("... (truncated)\n");
                    }
                    output.push_str("```\n\n");
                }
                None => {
                    output.push_str(&format!(
                        "### {}\n\n_(source unavailable: index digest differs from disk; \
                         re-run after rebuilding the index)_\n\n",
                        file.path
                    ));
                }
            }
        }
    }

    if view.hits.is_empty() && view.nodes.is_empty() && view.files.is_empty() {
        use compass_model::query_contract::QueryDiagnosticCode;
        if view.filtered_out {
            // Compass resolved the operand, but the `path` filter excluded every
            // result; "try a different symbol name" would be wrong advice here.
            output.push_str(
                "No results in the requested path. Compass matched symbols, but the \
                 `path` filter excluded them; widen or drop the filter.\n\n",
            );
        } else if view.diagnostic_codes.contains(&QueryDiagnosticCode::AmbiguousMatch) {
            // The symbol matched several nodes, so nothing resolved to one target.
            // Advise qualifying the name, not trying a different symbol.
            output.push_str(
                "No exact match: the operand is ambiguous (it matched several \
                 symbols). Re-run with a qualified name or add a `path` to \
                 disambiguate; see the diagnostics below.\n\n",
            );
        } else if view.diagnostic_codes.contains(&QueryDiagnosticCode::DirectionMismatch) {
            // The two endpoints are connected, just not in the requested order.
            output.push_str(
                "No trail in the requested direction. A trail connects the two \
                 symbols, but only from `target` to `source`; swap them.\n\n",
            );
        } else {
            output.push_str("No matches. Try a different symbol name, a broader query, \
                             or `mode=context` for a task-oriented packet.\n\n");
        }
    }

    if view.truncated {
        output.push_str("**Note:** results were truncated by Compass query bounds.\n\n");
    }
    if !view.diagnostics.is_empty() {
        output.push_str("**Diagnostics:**\n");
        for diag in view.diagnostics.iter().take(MAX_CANDIDATE_ROWS) {
            output.push_str(&format!("- {diag}\n"));
        }
    }
}

/// Cap how many edges/nodes/paths/source files a single structural report
/// renders, bounding one call's context cost.
const MAX_EDGE_ROWS: usize = 40;
const MAX_NODE_ROWS: usize = 40;
/// How many resolved structural nodes get a fenced declaration snippet (each a
/// source-file read), bounding the context cost of a wide call-graph answer.
const MAX_NODE_SNIPPETS: usize = 6;
const MAX_PATH_ROWS: usize = 10;
const MAX_SOURCE_FILES: usize = 8;

/// Default `affected` traversal depth when the caller does not pass `depth`.
/// Mirrors Compass's own CLI default (`--depth` defaults to 2).
const DEFAULT_AFFECTED_DEPTH: usize = 2;

/// Cap for the `affected` report's `Relations:` header line. Clears the full
/// default relation set (149 chars) so the common case is shown in full, while
/// still bounding a pathological caller-supplied list.
const MAX_RELATIONS_CHARS: usize = 200;

/// Compass node kind for a file-level node; these are counted as files in the
/// orientation map rather than as symbols.
const ORIENTATION_FILE_KIND: &str = "file";
/// Row caps for the orientation map's directory and hottest-symbol sections.
const ORIENTATION_DIR_ROWS: usize = 12;
const ORIENTATION_SYMBOL_ROWS: usize = 20;

/// The leading path segment of a repo-relative file path (`a/b/c.rs` -> `a`), or
/// the whole path when it has no directory. Never blank for a non-empty path.
fn top_level_dir(file: &str) -> String {
    let mut segments = file
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".");
    match (segments.next(), segments.next()) {
        // A path with at least one separator: the first segment is the directory.
        (Some(first), Some(_)) => first.to_string(),
        // Separator-less (a root-level file) and empty both bucket under the
        // repo root, so the section is always a directory listing.
        _ => ".".to_string(),
    }
}

/// Compass's internal hard ceiling on `max_candidates` / explore symbols
/// (`MAX_CODE_QUERY_CANDIDATES`). The constant is private to `compass-query`, so
/// it is mirrored here; keep it in sync if the pinned Compass version changes.
const COMPASS_MAX_CANDIDATES: usize = 256;

/// Does a repo-relative `path` match a user-supplied path filter? Both sides are
/// normalized into path segments first (split on `/` or `\`, dropping empty and
/// `.` components), so a filter like `./src`, `.`, or `/` behaves like `src` or
/// an empty filter. A filter then matches when its segment sequence appears as a
/// contiguous run of the path's segments, so `src` matches `src/lib.rs` (and
/// `crates/x/src/lib.rs`) but not `src2/lib.rs`, and `src/lib.rs` matches a
/// trailing path segment but not `src/lib.rs.bak`.
fn path_matches_filter(filter: &str, path: &str) -> bool {
    fn segments(value: &str) -> Vec<&str> {
        value
            .split(['/', '\\'])
            .filter(|part| !part.is_empty() && *part != ".")
            .collect()
    }
    let filter_segments = segments(filter);
    if filter_segments.is_empty() {
        return true;
    }
    segments(path)
        .windows(filter_segments.len())
        .any(|window| window == filter_segments.as_slice())
}

/// snake_case name for a semantic node role, taken from Compass's own serde
/// representation (`NodeRole` uses `#[serde(rename_all = "snake_case")]`) so the
/// label never drifts from the vocabulary Compass defines.
fn role_name(role: compass_model::code_graph::NodeRole) -> String {
    serde_json::to_value(role)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{role:?}").to_lowercase())
}

/// Trim an explore symbol set to Compass's hard candidate ceiling in place.
/// Compass rejects the whole request (not just the excess) above the ceiling, so
/// this prevents an oversized `symbols` call from failing outright. Returns
/// `(requested, queried)` so the caller can surface a truncation note.
fn clamp_explore_symbols(symbols: &mut Vec<String>) -> (usize, usize) {
    let total = symbols.len();
    if total > COMPASS_MAX_CANDIDATES {
        symbols.truncate(COMPASS_MAX_CANDIDATES);
    }
    (total, symbols.len())
}

/// Deduplicated source-file reader for a single query. Reads each unique file
/// at most once (resolving against the working dir then git toplevel), so a wide
/// query with many hits in one file does not re-open/re-read it per row.
#[derive(Default)]
struct SourceCache {
    /// repo-relative file path -> full resolved source text (or absence on miss)
    text_by_file: HashMap<String, Option<String>>,
}

impl SourceCache {
    /// Return the line-window snippet for `anchor`, reading the file at most
    /// once across all rows. `None` if the file is missing/unreadable or the
    /// anchor is degenerate (snippet simply omitted, never an error).
    fn snippet(&mut self, working_dir: &Path, anchor: &SourceAnchor) -> Option<String> {
        if anchor.start_line == 0 || anchor.end_line < anchor.start_line {
            return None;
        }
        let text = self.text(working_dir, &anchor.file)?;
        // Render the [start_line, end_line) window from the in-memory file text,
        // mirroring the streaming reader's window semantics (capped at 8 lines).
        let mut out = String::new();
        let start = anchor.start_line;
        let requested_end = anchor.end_line.max(start + 1);
        let render_to = requested_end.min(start + 8);
        let fold = requested_end > render_to;
        let digits = render_to.saturating_sub(1).to_string().len().max(1);
        let mut line_num: u32 = 0;
        for line in text.lines() {
            line_num += 1;
            if line_num >= start && line_num < render_to {
                out.push_str(&format!("{:width$}| {}\n", line_num, line, width = digits));
            }
            if line_num >= render_to {
                break;
            }
        }
        // Line numbering is 1-based per compass anchors; if the start was past
        // the last line we render nothing (no fold for an empty/EOF-clamped span).
        if fold && line_num >= render_to {
            out.push_str("...\n");
        }
        Some(out)
    }

    fn text(&mut self, working_dir: &Path, file: &str) -> Option<String> {
        if let Some(cached) = self.text_by_file.get(file) {
            return cached.clone();
        }
        let resolved = resolve_source_text(working_dir, file);
        self.text_by_file.insert(file.to_string(), resolved.clone());
        resolved
    }
}

/// Read the full text of a repo-relative source file, resolving the anchor's
/// `file` against the session working directory and, as a fallback, the
/// enclosing git worktree root. Compass stores `source.file` relative to the
/// repository root it was indexed from, which is the git top for a repo (the
/// same identity `resolve_compass_cache` uses). When a session is bound to a
/// subdirectory of the repo, `working_dir` alone is not enough, so we also try
/// the git toplevel. Returns `None` when the file is missing, outside the
/// working tree, or unreadable, so a snippet is a best-effort enrichment that
/// never fails the query. Guards against path traversal: an absolute or
/// `..`-escaping `file` is refused.
fn resolve_source_text(working_dir: &Path, file: &str) -> Option<String> {
    let rel = Path::new(file);
    if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return None;
    }
    let bases = std::iter::once(working_dir.to_path_buf())
        .chain(git_toplevel_cached(working_dir).map(PathBuf::from));
    for base in bases {
        if let Ok(text) = std::fs::read_to_string(base.join(rel)) {
            return Some(text);
        }
    }
    None
}

/// How long a resolved git worktree toplevel is reused. The toplevel is stable
/// for a given working dir, so a short per-process cache avoids re-forking git
/// for every snippet row in one query.
const GIT_TOPLEVEL_CACHE_TTL: Duration = Duration::from_secs(60);

static LAST_GIT_TOPLEVEL: OnceLock<Mutex<HashMap<PathBuf, (SystemTime, String)>>> =
    OnceLock::new();

/// Resolve the git worktree toplevel for `working_dir` (cached), or `None` when
/// it is not inside a git repo / git is unavailable. This is the repo-relative
/// base for Compass source paths. Distinct from `current_git_top_cached`, which
/// returns the git *common dir* (a stable shared-cache key but not the checkout
/// root: for a linked worktree the common dir is the main repo's `.git`, while
/// the toplevel is the worktree's own root).
fn git_toplevel_cached(working_dir: &Path) -> Option<String> {
    let map = LAST_GIT_TOPLEVEL.get_or_init(|| Mutex::new(HashMap::new()));
    {
        let guard = lock_cached(map);
        if let Some((t, top)) = guard.get(working_dir)
            && t.elapsed().map(|d| d < GIT_TOPLEVEL_CACHE_TTL).unwrap_or(false)
        {
            return Some(top.clone());
        }
    }
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(working_dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    let result = if s.is_empty() { None } else { Some(s) };
    if let Some(top) = &result {
        lock_cached(map).insert(
            working_dir.to_path_buf(),
            (SystemTime::now(), top.clone()),
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_tool_core::ToolExecutionMode;
    use std::io::Write;
    use std::path::PathBuf;

    /// Build a hits-only [`ResponseView`] from raw rows and render it, so the
    /// search-rendering tests exercise the same `render_view_body` path the
    /// production `format_view` uses without hand-building a Compass response.
    fn format_test_query(
        query: &str,
        limit: usize,
        path_filter: Option<&str>,
        rows: &[ResultRow],
        working_dir: &Path,
    ) -> String {
        let view = ResponseView {
            hits: rows.to_vec(),
            nodes: Vec::new(),
            edges: Vec::new(),
            paths: Vec::new(),
            files: Vec::new(),
            truncated: false,
            diagnostics: Vec::new(),
            filtered_out: false,
            filter_dropped_edges: false,
            diagnostic_codes: Default::default(),
        };
        format_view(query, QueryIntent::Search, limit, path_filter, &view, working_dir)
    }

    /// Build a minimal `CompassQueryInput` from a free-form query string, for
    /// driving `execute_query` in integration tests.
    fn input(query: &str) -> CompassQueryInput {
        CompassQueryInput {
            query: Some(query.to_string()),
            path: None,
            limit: None,
            mode: None,
            symbols: None,
            source: None,
            target: None,
            relations: None,
            depth: None,
            include_heuristic: None,
        }
    }

    /// Test helper that sets `JCODE_HOME` for the duration of a test, so
    /// `resolve_compass_cache`/`execute` writes under a temp dir instead of the
    /// real `~/.jcode`. Holds the `TempDir` so it isn't removed early, and
    /// restores/removes the previous `JCODE_HOME` on drop.
    struct HomeGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    impl HomeGuard {
        fn set() -> (Self, PathBuf) {
            let _lock = crate::storage::lock_test_env();
            let dir = tempfile::tempdir().expect("temp home");
            let path = dir.path().to_path_buf();
            crate::env::set_var("JCODE_HOME", &path);
            (HomeGuard { _lock, _dir: dir }, path)
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            crate::env::remove_var("JCODE_HOME");
        }
    }

    /// Create an isolated temp project with a single source file, returning the
    /// project dir, its `compass-out` output dir, and a separate branch-agnostic
    /// AST cache root (mirroring the production split). The `TempDir` is dropped
    /// (and the directory removed) automatically when the test ends, so each test
    /// gets a unique, isolated workspace with no cross-test leakage.
    fn make_isolated_project() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let root = tmp.path().to_path_buf();
        let mut f = std::fs::File::create(root.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);

        // Match production semantics: `output_dir` is Compass's *output root*
        // (Compass writes its graph under `<output_dir>/compass-out/graph.json`),
        // and `ast_cache_root` is the branch-agnostic AST-fact cache.
        let output_dir = root.join("cache/compass");
        let ast_cache_root = root.join("cache/.ast-cache");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::create_dir_all(&ast_cache_root).unwrap();
        (tmp, root, output_dir, ast_cache_root)
    }

    #[test]
    fn index_unavailable_message_has_no_stray_indentation() {
        let msg = format_index_unavailable("open-booms", "build-booms");
        assert!(msg.contains("open-booms"));
        assert!(msg.contains("build-booms"));
        // No embedded indentation from source formatting.
        assert!(
            !msg.contains("                             "),
            "message contained embedded indentation: {:?}",
            msg
        );
        // Every physical line begins at column 0.
        for line in msg.lines() {
            assert!(
                !line.starts_with(' '),
                "unexpected leading space: {:?}",
                line
            );
        }
    }

    #[test]
    fn query_error_message_names_cache_dir() {
        let msg = format_query_error(
            "boom",
            "auth",
            std::path::Path::new("/tmp/x/.jcode/cache/compass"),
        );
        assert!(msg.contains("boom"));
        assert!(msg.contains("auth"));
        assert!(msg.contains("/tmp/x/.jcode/cache/compass"));
    }
    #[test]
    fn builds_and_queries_index() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();

        // Build the index in-process.
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build should succeed");

        // Open and run a search.
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");
        let response = engine
            .search(SearchRequest {
                query: "authentication".to_string(),
                limits: CodeQueryLimits {
                    max_nodes: 10,
                    ..Default::default()
                },
            })
            .expect("search should succeed");

        // The full build→open→search pipeline completed without error, which is
        // the real invariant being tested. Exact hit counts depend on the
        // semantic model, and a 1-line fixture is not guaranteed to match.
        let _ = response.results.len();
    }

    // Validates the concurrency contract: `concurrency_safe_marker()` is true,
    // so the harness may dispatch this tool in parallel with siblings. On a cold
    // cache the tool writes graph.json, so without the flock added in deba52e74
    // concurrent calls would race and could corrupt the index. We run several
    // real OS threads (each with its own tiny runtime) so the builds genuinely
    // overlap, then assert every call succeeds and a single valid index remains.
    #[test]
    fn concurrent_cold_builds_do_not_race() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let mut f = std::fs::File::create(root.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);
        let graph_path = resolve_compass_cache(&root).graph_path;
        assert!(!graph_path.exists(), "fixture should start with no index");

        let tool = std::sync::Arc::new(CompassQueryTool::new());
        let failures = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let success = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        std::thread::scope(|s| {
            for i in 0..4 {
                let tool = tool.clone();
                let failures = failures.clone();
                let success = success.clone();
                let root = root.clone();
                s.spawn(move || {
                    // Each thread runs its own current-thread runtime so the
                    // blocking builds overlap on real cores, like parallel dispatch.
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .build()
                        .expect("runtime");
                    let ctx = ToolContext {
                        session_id: "s".into(),
                        message_id: "m".into(),
                        tool_call_id: format!("t-{i}"),
                        working_dir: Some(root),
                        stdin_request_tx: None,
                        graceful_shutdown_signal: None,
                        execution_mode: ToolExecutionMode::Direct,
                    };
                    let out = rt.block_on(
                        tool.execute(serde_json::json!({ "query": "authentication" }), ctx),
                    );
                    match out {
                        Ok(out)
                            if !out.output.contains("is not available for this project yet") =>
                        {
                            success.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        _ => {
                            failures.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                });
            }
        });

        assert_eq!(
            failures.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "concurrent cold builds must all succeed"
        );
        assert_eq!(
            success.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "all four concurrent builds must succeed"
        );

        // Exactly one valid index must exist and be reopenable after the race.
        assert!(
            graph_path.exists(),
            "index should exist after concurrent builds"
        );
        let edge = resolve_compass_cache(&root);
        assert!(
            compass_query::open(&graph_path, None, &edge.output_dir).is_ok(),
            "index left by concurrent builds must be openable"
        );
    }

    // Two worktrees of the same repo on DIFFERENT commits must be able to build
    // concurrently without corrupting the shared .ast-cache. This is the race the
    // per-project build lock (vs a per-SHA lock) exists to prevent: Compass does
    // not internally lock its shared-history cache.
    #[test]
    fn concurrent_cross_worktree_builds_do_not_corrupt_shared_cache() {
        let (_home, _home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();

        let git = |args: &[&str], cwd: &std::path::Path| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"], &main) {
            return;
        }
        git(&["config", "user.email", "test@example.com"], &main);
        git(&["config", "user.name", "Test"], &main);
        std::fs::write(main.join("main.rs"), "fn a() {}\n").unwrap();
        git(&["add", "."], &main);
        if !git(&["commit", "-qm", "init"], &main) {
            return;
        }
        // Create a second commit on a different branch so the worktrees are on
        // DIFFERENT SHAs.
        git(&["checkout", "-qb", "other"], &main);
        std::fs::write(main.join("main.rs"), "fn b() {}\n").unwrap();
        git(&["commit", "-aqm", "other"], &main);
        // Two linked worktrees, one per branch/commit.
        git(&["checkout", "-q", "master"], &main);
        if !std::process::Command::new("git")
            .args(["worktree", "add", "-qb", "wother", wt.to_str().unwrap(), "other"])
            .current_dir(&main)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return;
        }

        let tool = std::sync::Arc::new(CompassQueryTool::new());
        let failures = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        std::thread::scope(|s| {
            for wd in [main.clone(), wt.clone()] {
                let tool = tool.clone();
                let failures = failures.clone();
                s.spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .build()
                        .expect("runtime");
                    let ctx = ToolContext {
                        session_id: "s".into(),
                        message_id: "m".into(),
                        tool_call_id: "t".into(),
                        working_dir: Some(wd),
                        stdin_request_tx: None,
                        graceful_shutdown_signal: None,
                        execution_mode: ToolExecutionMode::Direct,
                    };
                    let out = rt.block_on(
                        tool.execute(serde_json::json!({ "query": "authentication" }), ctx),
                    );
                    match out {
                        Ok(out) if out.output.contains("**Found ") => {}
                        _ => {
                            failures.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                });
            }
        });
        assert_eq!(
            failures.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "both cross-worktree concurrent builds must succeed against a shared .ast-cache"
        );
        // Both per-SHA outputs must be independently valid.
        let main_shas = git_reachable_shas(&main).unwrap();
        let (a_sha, b_sha) = {
            let mut v: Vec<&String> = main_shas.iter().collect();
            v.sort();
            (v[0].clone(), v[1].clone())
        };
        for sha in [a_sha, b_sha] {
            let out_dir = crate::storage::jcode_dir()
                .unwrap()
                .join(COMPASS_CACHE_HOME)
                .join(short_id(
                    &git_repo_identity(&main).unwrap(),
                ))
                .join(&sha);
            assert!(
                compass_query::open(&out_dir.join("compass-out/graph.json"), None, &out_dir).is_ok(),
                "per-SHA index for {sha} must be openable after concurrent builds"
            );
        }
    }

    #[test]
    fn index_is_stale_detects_new_source() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        let graph_path = output_dir.join("compass-out/graph.json");
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");

        // A fresh index is not considered stale against its own source.
        assert!(
            !index_is_stale(
                &root,
                &graph_path,
                current_git_sha(&root).as_deref(),
                &output_dir,
                false
            ),
            "just-built index should not be stale"
        );

        // Adding a new source file (mtime strictly after the build) makes it stale.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut f = std::fs::File::create(root.join("added.rs")).unwrap();
        writeln!(f, "fn newly_added() {{ }}").unwrap();
        drop(f);
        assert!(
            index_is_stale(
                &root,
                &graph_path,
                current_git_sha(&root).as_deref(),
                &output_dir,
                false
            ),
            "index must be stale after a newer source file is added"
        );

        // Rebuilding refreshes the index mtime, so it is no longer stale.
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("rebuild");
        assert!(
            !index_is_stale(
                &root,
                &graph_path,
                current_git_sha(&root).as_deref(),
                &output_dir,
                false
            ),
            "index should be fresh again after rebuild"
        );
    }

    // A stale index must be transparently rebuilt when the tool is invoked, with
    // no manual cache deletion required by the caller. This verifies a rebuild
    // actually happened (the index mtime advances) rather than just that the
    // query succeeds — a valid-but-stale index would also satisfy the latter.
    #[tokio::test]
    async fn ensure_fresh_graph_rebuilds_a_stale_index() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn authenticate() {}\n").unwrap();
        let edge = resolve_compass_cache(&root);
        let out_dir = edge.output_dir.join("compass-out");
        build_compass_index(&root, &edge.output_dir, &edge.ast_cache_root).expect("build");

        std::thread::sleep(std::time::Duration::from_millis(10));
        let before = std::fs::metadata(&out_dir).unwrap().modified().unwrap();

        // Add a source file so the built index is now stale. (No prior
        // `ensure_fresh_graph` call, so no scan timestamp is recorded and the
        // staleness walk is not throttled.)
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(root.join("added.rs"), "fn newly_added() {}\n").unwrap();

        let graph = ensure_fresh_graph(&edge, &root).expect("rebuilt graph");
        assert!(
            graph
                .nodes()
                .any(|(_, n)| n.label().contains("newly_added")),
            "a stale graph must be rebuilt to include the new file"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        let after = std::fs::metadata(&out_dir).unwrap().modified().unwrap();
        assert!(after > before, "the rebuild must refresh the index on disk");
    }

    #[tokio::test]
    async fn stale_index_is_rebuilt_on_query() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let mut f = std::fs::File::create(root.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);
        let edge = resolve_compass_cache(&root);
        let graph_path = edge.graph_path.clone();
        let out_dir = edge.output_dir.join("compass-out");
        let c = edge.clone();
        build_compass_index(&root, &c.output_dir, &c.ast_cache_root).expect("build");

        std::thread::sleep(std::time::Duration::from_millis(10));
        let before = std::fs::metadata(&out_dir)
            .expect("index dir exists")
            .modified()
            .expect("index dir mtime");

        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut f = std::fs::File::create(root.join("added.rs")).unwrap();
        writeln!(f, "fn newly_added() {{ }}").unwrap();
        drop(f);

        let ctx = ToolContext {
            session_id: "s".into(),
            message_id: "m".into(),
            tool_call_id: "t".into(),
            working_dir: Some(root),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        };
        let out = CompassQueryTool::new()
            .execute(serde_json::json!({ "query": "authentication" }), ctx)
            .await
            .expect("execute");
        assert!(
            !out.output.contains("is not available for this project yet"),
            "stale index should rebuild and not report unavailability: {}",
            out.output
        );

        // The rebuild must have refreshed the index on disk (the compass-out dir
        // is recreated on a rebuild, so its mtime advances).
        std::thread::sleep(std::time::Duration::from_millis(10));
        let after = std::fs::metadata(&out_dir)
            .expect("index dir exists after query")
            .modified()
            .expect("index dir mtime after");
        assert!(
            after > before,
            "stale index must be rebuilt (dir mtime {after:?} should be after {before:?})"
        );
        assert!(
            compass_query::open(&graph_path, None, &edge.output_dir).is_ok(),
            "rebuilt index must be openable"
        );
    }

    // A fresh (non-stale) index must be served as-is: a follow-up query with no
    // source change must NOT rebuild it (the index dir mtime stays stable). This
    // guards against regressions where the cache is needlessly discarded.
    #[tokio::test]
    async fn fresh_index_is_not_rebuilt_on_query() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let mut f = std::fs::File::create(root.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);
        let edge = resolve_compass_cache(&root);
        let out_dir = edge.output_dir.join("compass-out");
        let c = edge.clone();
        build_compass_index(&root, &c.output_dir, &c.ast_cache_root).expect("build");

        std::thread::sleep(std::time::Duration::from_millis(10));
        let before = std::fs::metadata(&out_dir)
            .expect("index dir exists")
            .modified()
            .expect("index dir mtime");

        let ctx = ToolContext {
            session_id: "s".into(),
            message_id: "m".into(),
            tool_call_id: "t".into(),
            working_dir: Some(root),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        };
        let out = CompassQueryTool::new()
            .execute(serde_json::json!({ "query": "authentication" }), ctx)
            .await
            .expect("execute");
        assert!(
            !out.output.contains("is not available for this project yet"),
            "fresh index query should succeed: {}",
            out.output
        );
        // The query must actually return real search results through the public
        // execute interface, not merely not-error: the built index must contain
        // at least one node for "authentication".
        assert!(
            out.output.contains("**Found ") && out.output.contains(" result(s)**"),
            "execute must return a result report, got: {}",
            out.output
        );

        // No rebuild => the index dir mtime is unchanged.
        let after = std::fs::metadata(&out_dir)
            .expect("index dir exists after query")
            .modified()
            .expect("index dir mtime after");
        assert_eq!(
            after, before,
            "fresh index must not be rebuilt when source is unchanged"
        );
    }

    // An operand-less call must return a plain input-error message, not the
    // engine-failure text that (misleadingly) advises clearing the index cache:
    // the query never reached the engine.
    #[tokio::test]
    async fn operand_less_call_reports_input_error_not_cache_advice() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        let edge = resolve_compass_cache(&root);
        let c = edge.clone();
        build_compass_index(&root, &c.output_dir, &c.ast_cache_root).expect("build");

        let ctx = ToolContext {
            session_id: "s".into(),
            message_id: "m".into(),
            tool_call_id: "t".into(),
            working_dir: Some(root),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        };
        let out = CompassQueryTool::new()
            .execute(serde_json::json!({ "mode": "callers" }), ctx)
            .await
            .expect("execute");
        assert!(
            out.output.contains("requires an operand"),
            "must name the missing operand, got: {}",
            out.output
        );
        assert!(
            !out.output.contains("clear the cache"),
            "an input error must not advise clearing the cache, got: {}",
            out.output
        );
    }

    // A `context` call with a `path` filter is an input error, so it must read as
    // a plain rejection rather than the engine-failure text with cache advice.
    #[tokio::test]
    async fn context_path_filter_reports_input_error_not_cache_advice() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        let edge = resolve_compass_cache(&root);
        let c = edge.clone();
        build_compass_index(&root, &c.output_dir, &c.ast_cache_root).expect("build");

        let ctx = ToolContext {
            session_id: "s".into(),
            message_id: "m".into(),
            tool_call_id: "t".into(),
            working_dir: Some(root),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        };
        let out = CompassQueryTool::new()
            .execute(
                serde_json::json!({ "mode": "context", "query": "a", "path": "main.rs" }),
                ctx,
            )
            .await
            .expect("execute");
        assert!(
            out.output.contains("does not support a `path` filter"),
            "must name the unsupported filter, got: {}",
            out.output
        );
        assert!(
            !out.output.contains("clear the cache"),
            "an input error must not advise clearing the cache, got: {}",
            out.output
        );
    }

    // An engine-side `InvalidParameter` (here a `discover` path scope matching no
    // source) is the caller's mistake, not a broken index, so it must be reported
    // as a plain rejection rather than with the "clear the cache" rebuild advice.
    #[tokio::test]
    async fn engine_invalid_parameter_reports_input_error_not_cache_advice() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        let edge = resolve_compass_cache(&root);
        let c = edge.clone();
        build_compass_index(&root, &c.output_dir, &c.ast_cache_root).expect("build");

        let ctx = ToolContext {
            session_id: "s".into(),
            message_id: "m".into(),
            tool_call_id: "t".into(),
            working_dir: Some(root),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        };
        let out = CompassQueryTool::new()
            .execute(
                serde_json::json!({
                    "mode": "discover",
                    "query": "how does auth reach the database",
                    "path": "definitely/not/a/real/directory"
                }),
                ctx,
            )
            .await
            .expect("execute");
        assert!(
            !out.output.contains("clear the cache"),
            "an InvalidParameter must not advise clearing the cache, got: {}",
            out.output
        );
        assert!(
            out.output.contains("rejected the request"),
            "an InvalidParameter must read as a plain rejection, got: {}",
            out.output
        );
    }

    // The classifier splits engine errors by kind: only `InvalidParameter` is a
    // caller-input problem; a real index failure keeps the rebuild guidance.
    #[test]
    fn engine_error_classifier_splits_invalid_parameter_from_failures() {
        use compass_query::{QueryError, QueryErrorKind};

        let invalid = QueryError::new(QueryErrorKind::InvalidParameter, "bad", "boom");
        assert!(
            map_engine_error(invalid).downcast_ref::<InputError>().is_some(),
            "InvalidParameter must classify as an input error"
        );

        let corrupt = QueryError::new(QueryErrorKind::CorruptArtifact, "bad", "boom");
        assert!(
            map_engine_error(corrupt).downcast_ref::<InputError>().is_none(),
            "a real engine failure must not classify as an input error"
        );

        let invalid_request =
            compass_core::TaskContextError::InvalidRequest("nope".to_string());
        assert!(
            map_task_context_error(invalid_request)
                .downcast_ref::<InputError>()
                .is_some(),
            "an invalid task-context request must classify as an input error"
        );
    }

    /// Write a temp file (inside `dir`/rel) and return the dir + the repo-relative path.
    fn temp_src(content: &str, rel: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        if let Some(parent) = Path::new(rel).parent() {
            std::fs::create_dir_all(dir.path().join(parent)).unwrap();
        }
        std::fs::write(dir.path().join(rel), content).unwrap();
        (dir, rel.to_string())
    }

    fn anchor(file: &str, start: u32, end: u32) -> SourceAnchor {
        SourceAnchor {
            file: file.to_string(),
            start_byte: 0,
            end_byte: 0,
            start_line: start,
            start_column: 0,
            end_line: end,
            end_column: 0,
        }
    }

    #[test]
    fn source_cache_renders_line_range_with_gutter() {
        let (dir, rel) = temp_src("line1\nline2\nline3\nline4\nline5\n", "sub/snippet.txt");
        let mut cache = SourceCache::default();
        let out = cache.snippet(dir.path(), &anchor(&rel, 2, 5)).expect("snippet");
        assert_eq!(out, "2| line2\n3| line3\n4| line4\n");
    }

    #[test]
    fn source_cache_clamps_end_past_eof() {
        let (dir, rel) = temp_src("a\nb\nc\n", "sub/snippet.txt");
        let mut cache = SourceCache::default();
        let out = cache.snippet(dir.path(), &anchor(&rel, 1, 100)).expect("snippet");
        assert_eq!(out, "1| a\n2| b\n3| c\n");
    }

    #[test]
    fn source_cache_folds_very_long_anchor() {
        let mut content = String::new();
        for i in 1..=40 {
            content.push_str(&format!("line{i}\n"));
        }
        let (dir, rel) = temp_src(&content, "sub/snippet.txt");
        let mut cache = SourceCache::default();
        let out = cache.snippet(dir.path(), &anchor(&rel, 1, 40)).expect("snippet");
        // Span is capped at start+8 lines (lines 1..=8), then a fold marker.
        assert!(out.starts_with("1| line1\n2| line2\n"), "got: {out}");
        assert!(
            out.contains("8| line8\n"),
            "the capped span should end at line 8: {out}"
        );
        assert!(
            !out.contains("9| line9\n"),
            "long node must be folded before line 9: {out}"
        );
        assert!(out.contains("...\n"), "long node should be folded: {out}");
    }

    #[test]
    fn source_cache_empty_or_out_of_range_start() {
        // Start past EOF renders nothing.
        let (dir, rel) = temp_src("a\nb\n", "sub/snippet.txt");
        let mut cache = SourceCache::default();
        assert_eq!(
            cache.snippet(dir.path(), &anchor(&rel, 5, 9)).unwrap_or_default(),
            "",
            "start past EOF renders nothing"
        );
        // Degenerate zero-width anchor (start == end) still renders the line.
        let (dir3, rel3) = temp_src("only\n", "sub/snippet.txt");
        let mut c3 = SourceCache::default();
        assert_eq!(
            c3.snippet(dir3.path(), &anchor(&rel3, 1, 1)).unwrap_or_default(),
            "1| only\n"
        );
        // Missing file -> None (snippet omitted).
        let mut cm = SourceCache::default();
        assert!(
            cm.snippet(dir.path(), &anchor("sub/missing.txt", 1, 2)).is_none(),
            "missing file must yield no snippet"
        );
    }

    #[test]
    fn source_cache_reads_each_file_once_across_rows() {
        // A wide query can have many rows in one file; the cache must resolve
        // that file exactly once and serve every row's window from the single
        // read (this is the #2 dedupe guarantee).
        let (dir, _rel) = temp_src(
            "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n",
            "sub/many.rs",
        );
        let file = "sub/many.rs";
        let mut cache = SourceCache::default();
        // Request four different line windows in the same file.
        for ln in 1..=4 {
            let s = cache.snippet(dir.path(), &anchor(file, ln, ln + 1)).expect("snippet");
            assert!(s.contains(&format!("{ln}| fn")), "row {ln} window: {s}");
        }
        // Exactly one resolved text for the file (deduped), no re-read per row.
        assert_eq!(cache.text_by_file.len(), 1, "file must be resolved exactly once");
        assert!(
            cache.text_by_file.get(file).unwrap().is_some(),
            "resolved text must be cached"
        );
        // A missing file is cached as a miss too, so a later row in that file
        // does not re-attempt the read.
        cache.snippet(dir.path(), &anchor("sub/absent.rs", 1, 2));
        assert_eq!(cache.text_by_file.len(), 2, "missed file must also be cached");
        assert!(
            cache.text_by_file.get("sub/absent.rs").unwrap().is_none(),
            "missed file cached as None"
        );
    }

    #[test]
    fn format_query_renders_source_snippets_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            "fn main() {\n    let x = 1;\n}\n",
        )
        .unwrap();
        let rows = vec![ResultRow {
            name: "jcode::main".to_string(),
            file: Some("src/main.rs".to_string()),
            score: 123.0,
            matched: vec!["struct".to_string()],
            source: Some(SourceAnchor {
                file: "src/main.rs".to_string(),
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                start_column: 0,
                end_line: 3,
                end_column: 0,
            }),
            kind: "struct".to_string(),
        }];
        let rendered = format_test_query("main", 20, None, &rows, dir.path());
        assert!(rendered.contains("## 1. jcode::main"), "got: {rendered}");
        assert!(rendered.contains("**Kind:** struct"), "got: {rendered}");
        assert!(
            rendered.contains("1| fn main() {"),
            "snippet must render source, got: {rendered}"
        );
        assert!(rendered.contains("```"), "snippet must be fenced, got: {rendered}");
    }

    #[test]
    fn format_query_caps_snippets_at_max_rows_but_lists_rest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        // A file with 20 distinct functions so all rows have resolvable sources.
        let mut src = String::new();
        for i in 1..=20 {
            src.push_str(&format!("fn f{i}() {{}}\n"));
        }
        std::fs::write(dir.path().join("src/many.rs"), src).unwrap();
        let rows: Vec<ResultRow> = (1..=20)
            .map(|i| ResultRow {
                name: format!("f{i}"),
                file: Some("src/many.rs".to_string()),
                score: i as f64,
                matched: vec![],
                source: Some(SourceAnchor {
                    file: "src/many.rs".to_string(),
                    start_byte: 0,
                    end_byte: 0,
                    start_line: i,
                    start_column: 0,
                    end_line: i + 1,
                    end_column: 0,
                }),
                kind: "function".to_string(),
            })
            .collect();
        let rendered = format_test_query("many", 20, None, &rows, dir.path());
        // All 20 rows are listed, but only MAX_SNIPPET_ROWS get fences.
        assert!(rendered.contains("## 20. f20"), "last row must be listed: {rendered}");
        let fences = rendered.matches("```").count();
        assert!(
            fences <= MAX_SNIPPET_ROWS * 2,
            "snippet fences must be capped (got {fences}): {rendered}"
        );
        assert!(
            rendered.contains("1| fn f1()"),
            "top row keeps its snippet: {rendered}"
        );
        // A row past the cap is listed but not fenced with code.
        assert!(
            !rendered.contains("fn f15() {}"),
            "rows past the cap must not render their snippet code: {rendered}"
        );
    }

    #[test]
    fn format_query_omits_snippet_when_file_missing_or_traversal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/real.rs"), "fn real() {}\n").unwrap();
        // Missing file: no snippet, still renders the row.
        let rows = vec![ResultRow {
            name: "missing".to_string(),
            file: Some("src/nope.rs".to_string()),
            score: 1.0,
            matched: vec![],
            source: Some(SourceAnchor {
                file: "src/nope.rs".to_string(),
                start_byte: 0,
                end_byte: 1,
                start_line: 1,
                start_column: 0,
                end_line: 2,
                end_column: 0,
            }),
            kind: "struct".to_string(),
        }];
        let rendered = format_test_query("q", 20, None, &rows, dir.path());
        assert!(rendered.contains("## 1. missing"));
        assert!(
            !rendered.contains("```"),
            "missing file must render no snippet: {rendered}"
        );
        // Absolute / traversal path: refused without reading outside cwd.
        let traversal = ResultRow {
            name: "trav".to_string(),
            file: Some("../etc/passwd".to_string()),
            score: 1.0,
            matched: vec![],
            source: Some(SourceAnchor {
                file: "../etc/passwd".to_string(),
                start_byte: 0,
                end_byte: 1,
                start_line: 1,
                start_column: 0,
                end_line: 2,
                end_column: 0,
            }),
            kind: "struct".to_string(),
        };
        let rendered = format_test_query("q", 20, None, &[traversal], dir.path());
        assert!(
            !rendered.contains("root:"),
            "traversal path must not be read: {rendered}"
        );
        // Absolute path is a separate guard branch (`is_absolute`) from `..`
        // traversal; cover it explicitly.
        let abs = dir.path().join("src/real.rs").display().to_string();
        let absolute = ResultRow {
            name: "abs".to_string(),
            file: Some(abs.clone()),
            score: 1.0,
            matched: vec![],
            source: Some(SourceAnchor {
                file: abs,
                start_byte: 0,
                end_byte: 1,
                start_line: 1,
                start_column: 0,
                end_line: 2,
                end_column: 0,
            }),
            kind: "struct".to_string(),
        };
        let rendered = format_test_query("q", 20, None, &[absolute], dir.path());
        assert!(
            !rendered.contains("fn real"),
            "absolute source path must be refused: {rendered}"
        );
    }

    #[test]
    fn resolve_source_text_falls_back_to_git_toplevel_for_subdir_working_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Init a real git repo so `git rev-parse --show-toplevel` resolves.
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init"]) {
            return; // git not available; nothing to exercise.
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("main.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        git(&["add", "."]);
        if !git(&["commit", "-m", "init"]) {
            return;
        }
        // Working dir is the `sub` subdirectory; the source file lives at the
        // repo toplevel, so resolution must fall back to the git toplevel.
        let text = resolve_source_text(&root.join("sub"), "main.rs")
            .expect("source should resolve via git toplevel");
        assert!(
            text.contains("fn one() {}"),
            "expected toplevel-resolved text, got: {text}"
        );
    }

    // A pathological limit above u32::MAX must not wrap to 0 (which would violate
    // CodeQueryLimits::is_valid) and fail the query.
    #[test]
    fn huge_limit_is_clamped_not_wrapped() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        let graph_path = output_dir.join("compass-out/graph.json");
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine = compass_query::open(&graph_path, None, &output_dir).expect("open after build");

        // u32::MAX + 1 would wrap to 0 under a naive `as u32`. `authenticate`
        // (not `authentication`) matches the isolated project's single
        // `fn authenticate` node, so this also end-to-end verifies that a real
        // compass query renders a source snippet from the built index — not
        // just this tool's hand-built fixtures.
        let out = execute_query(
            Some(&engine),
            QueryIntent::Search,
            &input("authenticate"),
            u64::MAX as usize,
            false,
            &root,
            None,
        )
        .expect("query with clamped limit must succeed");
        assert!(
            out.contains("result(s)"),
            "expected a result report, got: {out}"
        );
        assert!(
            out.contains("fn authenticate"),
            "real compass query must render a source snippet, got: {out}"
        );
        assert!(
            out.contains("```"),
            "real compass query snippet must be fenced, got: {out}"
        );
    }

    // The path filter matches whole path segments, so `src` does not over-match
    // `src2`, and a file filter matches trailing segments.
    #[test]
    fn path_filter_matches_whole_segments() {
        assert!(path_matches_filter("src", "src/lib.rs"));
        assert!(path_matches_filter("src", "crates/x/src/lib.rs"));
        assert!(!path_matches_filter("src", "src2/lib.rs"));
        assert!(!path_matches_filter("src", "other/lib.rs"));
        assert!(path_matches_filter("src/lib.rs", "crates/x/src/lib.rs"));
        assert!(path_matches_filter("src/lib.rs", "src/lib.rs"));
        assert!(!path_matches_filter("src/lib.rs", "src/lib.rs.bak"));
        // An empty filter matches everything.
        assert!(path_matches_filter("", "anything"));
        assert!(path_matches_filter("/", "anything"));
        // `.` / `./` prefixes (and a bare `.`) normalize away and must not filter
        // everything out, which previously produced a silently empty report.
        assert!(path_matches_filter("./src", "src/lib.rs"));
        assert!(path_matches_filter("./src/lib.rs", "src/lib.rs"));
        assert!(path_matches_filter("./", "x/y"));
        assert!(path_matches_filter(".", "anything"));
        // A contiguous run of segments, not a scattered subsequence.
        assert!(path_matches_filter("crates/x", "crates/x/src/lib.rs"));
        assert!(!path_matches_filter("x/src", "crates/x/other/src/lib.rs"));
        // A filter with more segments than the path never matches (no panic).
        assert!(!path_matches_filter("a/b/c/d", "a/b"));
        assert!(!path_matches_filter("a", ""));
    }

    // The scalar operand for a single-symbol structural mode is `query`; an
    // operand-less call is a clear error.
    #[test]
    fn structural_operand_can_be_supplied_via_query() {
        let mut params = CompassQueryInput {
            query: Some("foo".to_string()),
            path: None,
            limit: None,
            mode: Some("callers".to_string()),
            symbols: None,
            source: None,
            target: None,
            relations: None,
            depth: None,
            include_heuristic: None,
        };
        assert_eq!(params.query_operand(), "foo");
        assert_eq!(params.display_target(QueryIntent::Callers), "foo");

        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::write(root.join("main.rs"), "fn foo() {}\n").unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine = compass_query::open(
            &output_dir.join("compass-out/graph.json"),
            None,
            &output_dir,
        )
        .expect("open after build");
        // An operand-less callers call errors rather than searching for "".
        params.query = None;
        let err = execute_query(Some(&engine), QueryIntent::Callers, &params, 20, false, &root, None)
            .expect_err("operand-less structural call must error");
        assert!(err.to_string().contains("requires an operand"), "got: {err}");
    }

    // A `traverse` call must name both endpoints in its header/title, not just the
    // source, so the report reads "from -> to".
    #[test]
    fn display_target_names_both_traverse_endpoints() {
        let params = CompassQueryInput {
            query: None,
            source: Some("crate::a".to_string()),
            target: Some("crate::b".to_string()),
            ..input("x")
        };
        assert_eq!(
            params.display_target(QueryIntent::Traverse),
            "crate::a -> crate::b"
        );

        // Only a target (e.g. a malformed call) still yields a label, not blank.
        let only_target = CompassQueryInput {
            query: None,
            source: None,
            target: Some("crate::b".to_string()),
            ..input("x")
        };
        assert_eq!(only_target.display_target(QueryIntent::Traverse), "crate::b");

        // The combined label stays bounded even for two long endpoints.
        let long = CompassQueryInput {
            query: None,
            source: Some("s".repeat(200)),
            target: Some("t".repeat(200)),
            ..input("x")
        };
        assert!(
            long.display_target(QueryIntent::Traverse).chars().count()
                <= MAX_DISPLAY_TARGET_CHARS + 1,
            "the two-endpoint label must be bounded"
        );
    }

    // `explore` takes a set via `symbols`; blank entries are skipped and the set
    // is summarized in the label. Other modes use `query`, ignoring `symbols`.
    #[test]
    fn symbols_is_the_explore_set_only() {
        let params = CompassQueryInput {
            query: Some("scalar".to_string()),
            symbols: Some(vec!["  ".to_string(), "real::sym".to_string()]),
            ..input("x")
        };
        // `symbols` drives the explore label (blanks skipped).
        assert_eq!(params.display_target(QueryIntent::Explore), "real::sym");
        // `query` drives the label for the single-symbol modes.
        assert_eq!(params.display_target(QueryIntent::Callers), "scalar");
        assert_eq!(params.first_symbol().as_deref(), Some("real::sym"));
        assert_eq!(params.query_operand(), "scalar");
    }

    // Semantic node roles render in Compass's own snake_case vocabulary, not via
    // Debug (which would mangle multi-word variants).
    #[test]
    fn role_name_uses_snake_case() {
        use compass_model::code_graph::NodeRole;
        assert_eq!(role_name(NodeRole::RouteHandler), "route_handler");
        assert_eq!(role_name(NodeRole::UiComponent), "ui_component");
        assert_eq!(role_name(NodeRole::Service), "service");
    }

    // A node label must never be blank: when the qualified name is empty it falls
    // back to the plain name, matching the search-hit render path.
    #[test]
    fn node_label_falls_back_to_name_when_qualified_name_empty() {
        let node = NodeView {
            id: "n:1".to_string(),
            name: "handler".to_string(),
            qualified_name: String::new(),
            kind: "function".to_string(),
            roles: Vec::new(),
            file: Some("a.rs".to_string()),
            source: None,
        };
        assert_eq!(format_node_label(&node), "`handler` (function, a.rs)");
    }

    /// The query intent parsed from the tool's `mode` field drives real Compass
    /// operations. Every advertised value must parse, an unknown value must be a
    /// clear error (not a silent fallback to search), and the structural set must be
    /// recognized so pre-warm guidance stays accurate.
    #[test]
    fn query_intent_parses_every_advertised_value() {
        assert_eq!(QueryIntent::parse(None).unwrap(), QueryIntent::Search);
        assert_eq!(QueryIntent::parse(Some("")).unwrap(), QueryIntent::Search);
        assert_eq!(QueryIntent::parse(Some("search")).unwrap(), QueryIntent::Search);
        assert_eq!(QueryIntent::parse(Some("CALLERS")).unwrap(), QueryIntent::Callers);
        assert_eq!(QueryIntent::parse(Some("callees")).unwrap(), QueryIntent::Callees);
        assert_eq!(QueryIntent::parse(Some("impact")).unwrap(), QueryIntent::Impact);
        assert_eq!(QueryIntent::parse(Some("explore")).unwrap(), QueryIntent::Explore);
        assert_eq!(QueryIntent::parse(Some("discover")).unwrap(), QueryIntent::Discover);
        // `discovery` is a legacy alias routed to discover.
        assert_eq!(QueryIntent::parse(Some("discovery")).unwrap(), QueryIntent::Discover);
        assert_eq!(QueryIntent::parse(Some("traverse")).unwrap(), QueryIntent::Traverse);
        assert_eq!(QueryIntent::parse(Some("path")).unwrap(), QueryIntent::Traverse);
        assert_eq!(QueryIntent::parse(Some("context")).unwrap(), QueryIntent::Context);
        assert_eq!(QueryIntent::parse(Some("affected")).unwrap(), QueryIntent::Affected);
        assert_eq!(QueryIntent::parse(Some("affected_nodes")).unwrap(), QueryIntent::Affected);
        assert_eq!(
            QueryIntent::parse(Some("orientation")).unwrap(),
            QueryIntent::Orientation
        );
        // `repo_map`/`map`/`overview` are aliases routed to orientation.
        assert_eq!(QueryIntent::parse(Some("repo_map")).unwrap(), QueryIntent::Orientation);
        assert_eq!(QueryIntent::parse(Some("map")).unwrap(), QueryIntent::Orientation);
        assert_eq!(QueryIntent::parse(Some("overview")).unwrap(), QueryIntent::Orientation);

        assert!(QueryIntent::parse(Some("nonsense")).is_err());
        assert!(!QueryIntent::Search.is_structural());
        for intent in [
            QueryIntent::Callers,
            QueryIntent::Callees,
            QueryIntent::Impact,
            QueryIntent::Explore,
            QueryIntent::Discover,
            QueryIntent::Traverse,
            QueryIntent::Context,
            QueryIntent::Affected,
            QueryIntent::Orientation,
        ] {
            assert!(intent.is_structural(), "{intent:?} must be structural");
        }
    }

    // A structural intent must actually invoke the matching Compass operation and
    // render its edges/nodes, not fall back to a keyword search. This drives the
    // real engine built from an isolated project with a genuine call edge.
    #[test]
    fn callers_intent_returns_call_graph_not_search_hits() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::write(
            root.join("main.rs"),
            "fn authenticate(user: &str) { let _ = user; }\n\
             fn login() { authenticate(\"x\"); }\n",
        )
        .unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        let out = execute_query(
            Some(&engine),
            QueryIntent::Callers,
            &input("authenticate"),
            20,
            false,
            &root,
            None,
        )
        .expect("callers query must succeed");

        assert!(
            out.contains("**Mode:** callers"),
            "report must name the callers mode, got: {out}"
        );
        // The caller (`login`) and the callee (`authenticate`) must both be listed
        // as resolved symbols, with at least one relationship edge rendered.
        assert!(
            out.contains("Resolved symbols"),
            "structural report must list resolved symbols, got: {out}"
        );
        assert!(
            out.contains("Relationships"),
            "callers report must render edges, got: {out}"
        );
    }

    // Build an isolated project whose call chain is `handler -> login ->
    // authenticate`, plus a second file under `sub/`, and return the graph path.
    fn build_affected_project() -> (tempfile::TempDir, PathBuf, PathBuf, Graph) {
        let (tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::write(
            root.join("main.rs"),
            "fn authenticate(user: &str) { let _ = user; }\n\
             fn login() { authenticate(\"x\"); }\n\
             fn handler() { login(); }\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/other.rs"), "pub fn other() {}\n").unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let graph_path = output_dir.join("compass-out/graph.json");
        let graph = Graph::load_for_affected(&graph_path).expect("load affected graph");
        (tmp, root, graph_path, graph)
    }

    // `affected` walks inbound edges (the reverse of `impact`): callers of the
    // seed, then callers of those callers, up to `depth`. It must name the seed,
    // list the dependents with their relation and file, and never leak raw ids.
    #[test]
    fn affected_lists_inbound_dependents_to_the_requested_depth() {
        let (_tmp, _root, _graph_path, graph) = build_affected_project();

        let mut params = input("authenticate");
        params.depth = Some(2);
        let out = execute_affected(&graph, "authenticate", &params, 20)
            .expect("affected query must succeed");

        assert!(out.contains("**Mode:** affected"), "got: {out}");
        assert!(out.contains("**Seed:** authenticate()"), "got: {out}");
        // Direct caller (`login`) and transitive caller (`handler`) both appear.
        assert!(out.contains("login() [calls]"), "got: {out}");
        assert!(out.contains("handler() [calls]"), "got: {out}");
        assert!(out.contains("**2 affected node(s)**"), "got: {out}");
        assert!(
            !out.contains("sha256:"),
            "the report must not leak raw node ids, got: {out}"
        );
    }

    // A depth of 1 stops at direct dependents, so the transitive caller is not
    // reported.
    #[test]
    fn affected_depth_bounds_the_traversal() {
        let (_tmp, _root, _graph_path, graph) = build_affected_project();

        let mut params = input("authenticate");
        params.depth = Some(1);
        let out = execute_affected(&graph, "authenticate", &params, 20)
            .expect("affected query must succeed");

        assert!(out.contains("login() [calls]"), "got: {out}");
        assert!(
            !out.contains("handler()"),
            "depth=1 must not report the transitive caller, got: {out}"
        );
    }

    // The header count must be the *in-scope* (post-filter) total, and the
    // truncation note must attribute a shortfall to the limit, not to a filter
    // that removed nothing. Uses two dependents in different directories so a
    // filter can remove one without removing all.
    #[test]
    fn affected_limit_and_filter_counts_are_accurate() {
        let dir = tempfile::tempdir().unwrap();
        let graph_path = dir.path().join("graph.json");
        let graph_json = serde_json::json!({
            "directed": true, "multigraph": true, "graph": {},
            "nodes": [
                {"id": "seed", "label": "seed()", "source_file": "a/seed.rs"},
                {"id": "caller_a", "label": "caller_a()", "source_file": "a/caller.rs"},
                {"id": "caller_b", "label": "caller_b()", "source_file": "b/caller.rs"}
            ],
            "links": [
                {"source": "caller_a", "target": "seed", "relation": "calls"},
                {"source": "caller_b", "target": "seed", "relation": "calls"}
            ]
        });
        std::fs::write(&graph_path, graph_json.to_string()).unwrap();
        let graph = Graph::load_for_affected(&graph_path).expect("load");

        // No filter, limit 1: two dependents in scope, one shown; the shortfall
        // is attributed to the limit, with no spurious filter note.
        let mut params = input("seed");
        params.limit = Some(1);
        let out = execute_affected(&graph, "seed", &params, 1).expect("affected");
        assert!(out.contains("**2 affected node(s)**"), "in-scope count: {out}");
        assert!(
            out.contains("showing 1 of 2 affected node(s)"),
            "limit shortfall must be attributed to the limit: {out}"
        );
        assert!(
            !out.contains("fell outside the `path` filter"),
            "no filter means no filter note: {out}"
        );

        // Filter to `a` keeps one of two: the header count is the in-scope total
        // (1) and the exclusion is reported separately.
        let mut params = input("seed");
        params.path = Some("a".to_string());
        let out = execute_affected(&graph, "seed", &params, 20).expect("affected");
        assert!(out.contains("**1 affected node(s)**"), "in-scope count: {out}");
        assert!(
            out.contains("1 affected node(s) fell outside the `path` filter"),
            "the exclusion must be reported: {out}"
        );
    }

    // A fresh-but-unparseable graph.json (corrupt artifact) must be rebuilt, not
    // surfaced as a hard failure - matching the engine path's behavior.
    #[test]
    fn ensure_fresh_graph_rebuilds_a_corrupt_index() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn authenticate() {}\n").unwrap();
        let edge = resolve_compass_cache(&root);
        let graph_path = edge.graph_path.clone();
        build_compass_index(&root, &edge.output_dir, &edge.ast_cache_root).expect("build");

        // Corrupt the graph artifact but keep the file present, so staleness checks
        // still call it "fresh" (only the content is unparseable).
        std::fs::write(&graph_path, b"{ not valid json").unwrap();
        // Drop only *this* path's cached graph so the load really re-reads the
        // corrupt file. Never clear the shared cache globally: sibling tests run
        // in parallel and rely on their own entries (e.g. the Arc::ptr_eq reuse
        // test).
        lock_cached(GRAPH_CACHE.get_or_init(|| Mutex::new(HashMap::new())))
            .retain(|(p, _), _| p != &graph_path);

        let graph = ensure_fresh_graph(&edge, &root)
            .expect("a corrupt index must be rebuilt, not surfaced as an error");
        assert!(
            graph.nodes().any(|(_, n)| n.label().contains("authenticate")),
            "the rebuilt graph must contain the real symbols"
        );
    }

    // A leftover code-query `index.lock` (Compass removes it only on a
    // successful build) must not wedge `compass_query` forever. This is the
    // exact production failure: a killed/panicked build leaves the lock, and
    // every later `open()` fails with `query_index_lock_timeout`, so the tool
    // reports "not available for this project yet" and the agent falls back to
    // agentgrep.
    #[test]
    fn recover_orphaned_code_query_builds_clears_dead_lock_and_temp() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn authenticate() {}\n").unwrap();
        let edge = resolve_compass_cache(&root);

        // Simulate a crashed build: a key dir with a lock and a partial index,
        // but no completed `index.sqlite3`.
        let key_dir = edge.output_dir.join(CODE_QUERY_DIR).join("deadbeefkey");
        std::fs::create_dir_all(&key_dir).unwrap();
        std::fs::write(key_dir.join(CODE_QUERY_LOCK_FILE), b"").unwrap();
        std::fs::write(key_dir.join("index.tmp-123-456"), b"partial").unwrap();
        std::fs::write(key_dir.join("index.tmp-123-456-journal"), b"j").unwrap();

        recover_orphaned_code_query_builds(&edge.output_dir);

        assert!(
            !key_dir.join(CODE_QUERY_LOCK_FILE).exists(),
            "the orphaned lock must be removed"
        );
        assert!(
            !key_dir.join("index.tmp-123-456").exists(),
            "the orphaned temp index must be reclaimed"
        );
        assert!(
            !key_dir.join("index.tmp-123-456-journal").exists(),
            "the orphaned temp journal must be reclaimed"
        );
    }

    // Recovery must also clear a lock that sits beside a *present but corrupt*
    // final index, not only when the index file is absent. A corrupt index
    // forces a rebuild that needs the lock, so skipping recovery when an index
    // merely exists would leave this exact case wedged.
    #[test]
    fn recover_orphaned_code_query_builds_clears_lock_beside_corrupt_index() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn authenticate() {}\n").unwrap();
        let edge = resolve_compass_cache(&root);

        let key_dir = edge.output_dir.join(CODE_QUERY_DIR).join("corruptkey");
        std::fs::create_dir_all(&key_dir).unwrap();
        // Present but not a valid index; only its presence matters to the old
        // (unsound) guard.
        std::fs::write(key_dir.join("index.sqlite3"), b"{ not a database").unwrap();
        std::fs::write(key_dir.join(CODE_QUERY_LOCK_FILE), b"").unwrap();

        recover_orphaned_code_query_builds(&edge.output_dir);

        assert!(
            !key_dir.join(CODE_QUERY_LOCK_FILE).exists(),
            "the lock must be cleared even when a (corrupt) index file is present"
        );
    }

    // A key dir that already holds a final index must never have that index
    // file removed, even though the lock is reclaimed unconditionally. Removing
    // the lock cannot disturb a healthy index (open() never consults the lock
    // once a valid index is present), so this guards the one thing that must
    // survive.
    #[test]
    fn recover_orphaned_code_query_builds_preserves_a_healthy_index() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn authenticate() {}\n").unwrap();
        let edge = resolve_compass_cache(&root);

        let key_dir = edge.output_dir.join(CODE_QUERY_DIR).join("healthkey");
        std::fs::create_dir_all(&key_dir).unwrap();
        std::fs::write(key_dir.join("index.sqlite3"), b"a-real-index").unwrap();
        std::fs::write(key_dir.join(CODE_QUERY_LOCK_FILE), b"").unwrap();

        recover_orphaned_code_query_builds(&edge.output_dir);

        assert!(
            key_dir.join("index.sqlite3").exists(),
            "a healthy final index must never be removed"
        );
        assert!(
            !key_dir.join(CODE_QUERY_LOCK_FILE).exists(),
            "the orphaned lock is reclaimed even alongside a present index"
        );
    }

    // End-to-end: `ensure_fresh_engine` must recover from a leftover lock and
    // return a working engine rather than the "not available" failure.
    //
    // Uses a git repo so the cache is the *shared* (per-SHA) one. That is the
    // production failure mode: `discard_stale_output` is a no-op for a shared
    // cache, so the orphaned lock survives a rebuild attempt and `open()` wedges
    // on it. (A non-git cache is fully removed on discard, so it would not
    // reproduce the wedge.) Skips when git is unavailable.
    #[test]
    fn ensure_fresh_engine_recovers_from_an_orphaned_code_query_lock() {
        let (_home, home) = HomeGuard::set();
        let root = home.join("repo");
        std::fs::create_dir_all(&root).unwrap();

        if !std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return; // git not available.
        }
        for (k, v) in [("user.email", "test@example.com"), ("user.name", "Test")] {
            std::process::Command::new("git")
                .args(["config", k, v])
                .current_dir(&root)
                .status()
                .ok();
        }
        std::fs::write(root.join("main.rs"), "fn authenticate() {}\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&root)
            .status()
            .ok();
        if !std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return;
        }

        let edge = resolve_compass_cache(&root);
        assert!(
            edge.is_shared,
            "a git repo must use the shared per-SHA cache"
        );

        // Build once so the graph exists and the engine knows its code-query key.
        ensure_fresh_engine(&edge, &root).expect("first build must succeed");

        // Simulate the crashed-build state for that key: drop the final index so
        // `open()` must rebuild, and leave a lock plus a partial temp, exactly as
        // a killed build does. A naive `open()` spins on the lock and fails with
        // `query_index_lock_timeout`.
        let code_query_root = edge.output_dir.join(CODE_QUERY_DIR);
        let mut armed = false;
        for entry in std::fs::read_dir(&code_query_root).unwrap().flatten() {
            let key_dir = entry.path();
            if !key_dir.is_dir() {
                continue;
            }
            let _ = std::fs::remove_file(key_dir.join("index.sqlite3"));
            std::fs::write(key_dir.join(CODE_QUERY_LOCK_FILE), b"").unwrap();
            std::fs::write(key_dir.join("index.tmp-999-1"), b"partial").unwrap();
            armed = true;
        }
        assert!(armed, "fixture must have armed an orphaned lock");

        let engine = ensure_fresh_engine(&edge, &root)
            .expect("an orphaned lock must be recovered, not surfaced as a failure");
        drop(engine);
    }

    // An explicit `depth: 0` must be honored (Compass treats it as "no traversal")
    // rather than silently clamped to 1, which would change the caller's request.
    #[test]
    fn affected_honors_explicit_zero_depth() {
        let (_tmp, _root, _graph_path, graph) = build_affected_project();

        let mut params = input("authenticate");
        params.depth = Some(0);
        let out = execute_affected(&graph, "authenticate", &params, 20)
            .expect("affected query must succeed");
        assert!(out.contains("**Depth:** 0"), "explicit depth 0 must be honored: {out}");
        assert!(
            !out.contains("login()"),
            "depth=0 must not traverse to any dependent, got: {out}"
        );
    }

    // An explicit `relations` list is honored; an all-blank override falls back
    // to the default set rather than silently following no relations.
    #[test]
    fn affected_relations_override_and_blank_fallback() {
        let (_tmp, _root, _graph_path, graph) = build_affected_project();

        let mut params = input("authenticate");
        params.relations = Some(vec!["imports".to_string()]);
        let out = execute_affected(&graph, "authenticate", &params, 20)
            .expect("affected query must succeed");
        assert!(out.contains("**Relations:** imports\n"), "got: {out}");
        assert!(out.contains("No affected nodes"), "got: {out}");

        let mut params = input("authenticate");
        params.relations = Some(vec!["  ".to_string()]);
        let out = execute_affected(&graph, "authenticate", &params, 20)
            .expect("affected query must succeed");
        // The blank override falls back to the default set, which includes `calls`.
        assert!(out.contains("calls"), "blank relations must fall back: {out}");
        assert!(out.contains("login() [calls]"), "got: {out}");
    }

    // An operand that resolves to no unique node reads as a clear "not found"
    // note (mirroring Compass's own message) rather than an empty body.
    #[test]
    fn affected_unknown_seed_is_reported_clearly() {
        let (_tmp, _root, _graph_path, graph) = build_affected_project();
        let out = execute_affected(&graph, "no_such_symbol_zzz", &input("x"), 20)
            .expect("affected query must succeed");
        assert!(
            out.contains("No unique node matched"),
            "an unknown seed must say so, got: {out}"
        );
        assert!(out.contains("mode=search"), "should point at search: {out}");
    }

    // A `path` filter is applied to the affected hits (Compass's traversal has no
    // path scope), and the report says how many fell outside it.
    #[test]
    fn affected_path_filter_scopes_hits_and_notes_exclusions() {
        let (_tmp, _root, _graph_path, graph) = build_affected_project();

        let mut params = input("authenticate");
        params.path = Some("sub".to_string());
        let out = execute_affected(&graph, "authenticate", &params, 20)
            .expect("affected query must succeed");
        assert!(out.contains("**Path filter:** sub"), "got: {out}");
        assert!(
            !out.contains("- login()"),
            "a scoped report must not list callers outside the filter: {out}"
        );
        assert!(
            out.contains("No affected nodes in the requested path"),
            "must say the filter excluded everything, got: {out}"
        );
    }

    // `affected`/`orientation` need the raw graph; without one (e.g. a caller that
    // has not loaded it) they error clearly instead of panicking.
    #[test]
    fn graph_modes_require_a_loaded_graph() {
        let dir = tempfile::tempdir().unwrap();
        let err = execute_query(
            None,
            QueryIntent::Affected,
            &input("foo"),
            20,
            false,
            dir.path(),
            None,
        )
        .expect_err("affected without a loaded graph must error");
        assert!(err.to_string().contains("needs the Compass graph"), "got: {err}");
        assert!(err.to_string().contains("affected"), "got: {err}");
    }

    // Digest detection must reject any scheme-qualified hex id, not just the
    // `sha256:`/`md5:` prefixes we happen to know, and must not misfire on a
    // normal symbol label.
    #[test]
    fn scheme_qualified_digest_detection() {
        let hex = "0123456789abcdef0123456789abcdef"; // 32 hex chars
        assert!(is_scheme_qualified_digest(&format!("sha256:{hex}")));
        assert!(is_scheme_qualified_digest(&format!("md5:{hex}")));
        assert!(is_scheme_qualified_digest(&format!("blake3:{hex}")));
        // Too short, not hex, or no scheme separator: not an id.
        assert!(!is_scheme_qualified_digest("sha256:abc"));
        assert!(!is_scheme_qualified_digest("http:0123456789abcdef")); // short
        assert!(!is_scheme_qualified_digest("crate::module::symbol"));
        assert!(!is_scheme_qualified_digest("HashMap::insert"));
        assert!(!is_scheme_qualified_digest("main.rs"));
    }

    // A dependent node whose only identity is an opaque `sha256:` id (no
    // label/name/qualified name/file) must not print that id: `display_label`
    // would fall back to the id, so the renderer substitutes the node kind.
    #[test]
    fn affected_never_renders_an_opaque_id_for_a_degenerate_node() {
        let dir = tempfile::tempdir().unwrap();
        let graph_path = dir.path().join("graph.json");
        let graph_json = serde_json::json!({
            "directed": true,
            "multigraph": true,
            "graph": {},
            "nodes": [
                {"id": "file", "label": "lib.rs", "source_file": "lib.rs"},
                {"id": "member", "label": "run()", "source_file": "lib.rs", "source_location": "L2"},
                {"id": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}
            ],
            "links": [
                {"source": "file", "target": "member", "relation": "contains"},
                {"source": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                 "target": "member", "relation": "calls"}
            ]
        });
        std::fs::write(&graph_path, graph_json.to_string()).unwrap();
        let graph = Graph::load_for_affected(&graph_path).expect("load");

        let out = execute_affected(&graph, "lib.rs", &input("x"), 20).expect("affected");
        assert!(
            !out.contains("sha256:"),
            "a degenerate dependent node must not leak its id, got: {out}"
        );
        assert!(
            out.contains("(symbol) [calls]"),
            "the degenerate node must fall back to a kind label, got: {out}"
        );

        // `orientation` renders labels from the same graph and must stay id-free
        // too (a file-less node is skipped by its placement rule, so the guard
        // there is defensive; the leak guarantee is what matters).
        let mut orient = input("");
        orient.query = None;
        let out = execute_orientation(&graph, &orient, 20).expect("orientation");
        assert!(
            !out.contains("sha256:"),
            "orientation must not leak a degenerate node id, got: {out}"
        );
    }

    // The graph-only modes reuse one loaded graph per (path, mtime): a second load
    // returns the same `Arc` (no re-parse), and touching the file (a rebuild)
    // yields a fresh one so a stale graph is never served.
    #[test]
    fn graph_cache_reuses_until_the_graph_changes() {
        let (_tmp, _root, graph_path, _direct_graph) = build_affected_project();

        let first = load_graph_cached(&graph_path).expect("first load");
        let second = load_graph_cached(&graph_path).expect("second load");
        assert!(
            Arc::ptr_eq(&first, &second),
            "an unchanged graph must be reused, not re-parsed"
        );

        // Simulate a rebuild rewriting graph.json: the mtime key must miss.
        // `load_graph_cached` reads through `Graph::load_for_affected`, which
        // reads the file; write a valid graph back with a newer mtime.
        let bytes = std::fs::read(&graph_path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&graph_path, &bytes).unwrap();
        let third = load_graph_cached(&graph_path).expect("third load");
        assert!(
            !Arc::ptr_eq(&first, &third),
            "a rebuilt graph must not be served from the old cache entry"
        );
    }

    // A caller-supplied `relations` list must not balloon the header: the
    // `Relations:` line is bounded like every other header, while the full
    // default set still renders in full.
    #[test]
    fn affected_relations_header_is_bounded() {
        let (_tmp, _root, _graph_path, graph) = build_affected_project();

        // The default set (149 chars) is shown in full.
        let out = execute_affected(&graph, "authenticate", &input("authenticate"), 20)
            .expect("affected");
        assert!(
            out.contains("renders"),
            "the full default relations must render: {out}"
        );

        // A pathological override is summarized, not printed in full.
        let mut params = input("authenticate");
        params.relations = Some((0..500).map(|i| format!("relation_{i}")).collect());
        let out = execute_affected(&graph, "authenticate", &params, 20)
            .expect("affected");
        let relations_line = out
            .lines()
            .find(|l| l.starts_with("**Relations:**"))
            .expect("relations line");
        assert!(
            relations_line.chars().count() <= MAX_RELATIONS_CHARS + 64,
            "relations line must be bounded, got {} chars: {relations_line}",
            relations_line.chars().count()
        );
        assert!(
            relations_line.contains("more)"),
            "an oversized list must be summarized: {relations_line}"
        );
    }

    // `affected` dispatches through `execute_query` and returns the report with NO
    // engine, which is exactly how the production path calls it for graph modes
    // (the engine is never opened for them).
    #[test]
    fn affected_dispatches_through_execute_query_without_an_engine() {
        let (_tmp, root, _graph_path, graph) = build_affected_project();

        let mut params = input("authenticate");
        params.mode = Some("affected".to_string());
        let out = execute_query(
            None,
            QueryIntent::Affected,
            &params,
            20,
            false,
            &root,
            Some(&graph),
        )
        .expect("affected must dispatch");
        assert!(out.contains("**Mode:** affected"), "got: {out}");
        assert!(out.contains("login() [calls]"), "got: {out}");
    }

    // `orientation` dispatches through `execute_query` with NO engine (the
    // production path for graph modes) and needs no operand.
    #[test]
    fn orientation_dispatches_through_execute_query_without_an_engine() {
        let (_tmp, root, _graph_path, graph) = build_affected_project();

        let mut params = input("");
        params.query = None;
        params.mode = Some("orientation".to_string());
        let out = execute_query(
            None,
            QueryIntent::Orientation,
            &params,
            20,
            false,
            &root,
            Some(&graph),
        )
        .expect("orientation must dispatch");
        assert!(out.contains("**Mode:** orientation"), "got: {out}");
        assert!(
            out.contains("## Hottest symbols"),
            "orientation must render its map: {out}"
        );
    }

    // An engine-backed mode with no engine is a clear error, not a panic.
    #[test]
    fn engine_mode_without_an_engine_errors() {
        let (_tmp, root, _graph_path, _graph) = build_affected_project();
        let err =
            execute_query(None, QueryIntent::Callers, &input("authenticate"), 20, false, &root, None)
                .expect_err("an engine mode with no engine must error");
        assert!(
            err.to_string().contains("needs the Compass query engine"),
            "got: {err}"
        );
    }

    // `orientation` composes a community-free map: file/symbol counts, top-level
    // directories by file count, and the most-connected symbols. It states that
    // no community artifact exists rather than pretending to have one.
    #[test]
    fn orientation_counts_distinct_files_not_file_nodes() {
        // A graph with source-anchored symbols but NO `file`-kind node: the
        // headline file count must still reflect the two distinct files, not 0.
        let dir = tempfile::tempdir().unwrap();
        let graph_path = dir.path().join("graph.json");
        let graph_json = serde_json::json!({
            "directed": true, "multigraph": true, "graph": {},
            "nodes": [
                {"id": "f1", "label": "a()", "source_file": "a/x.rs", "symbol_kind": "function"},
                {"id": "f2", "label": "b()", "source_file": "a/y.rs", "symbol_kind": "function"},
                {"id": "f3", "label": "c()", "source_file": "a/y.rs", "symbol_kind": "function"}
            ],
            "links": [{"source": "f1", "target": "f2", "relation": "calls"}]
        });
        std::fs::write(&graph_path, graph_json.to_string()).unwrap();
        let graph = Graph::load_for_affected(&graph_path).expect("load");

        let mut params = input("");
        params.query = None;
        let out = execute_orientation(&graph, &params, 20).expect("orientation");
        assert!(
            out.contains("2 file(s)"),
            "distinct source files must be counted even without file nodes: {out}"
        );
        assert!(out.contains("3 symbol(s)"), "got: {out}");
        assert!(out.contains("- `a` — 2 file(s)"), "got: {out}");
    }

    #[test]
    fn orientation_maps_the_repo_without_communities() {
        let (_tmp, _root, _graph_path, graph) = build_affected_project();

        let mut params = input("");
        params.query = None;
        let out = execute_orientation(&graph, &params, 20).expect("orientation");

        assert!(out.contains("**Mode:** orientation"), "got: {out}");
        // Two files (main.rs + sub/other.rs) and four symbols.
        assert!(out.contains("2 file(s)"), "got: {out}");
        assert!(out.contains("4 symbol(s)"), "got: {out}");
        assert!(out.contains("## Top-level directories"), "got: {out}");
        assert!(out.contains("- `sub` — 1 file(s)"), "got: {out}");
        assert!(out.contains("- `.` — 1 file(s)"), "got: {out}");
        assert!(
            out.contains("## Hottest symbols (most connections)"),
            "got: {out}"
        );
        assert!(out.contains("login() — 3 connection(s)"), "got: {out}");
        assert!(
            out.contains("no community clustering"),
            "the map must document the missing communities: {out}"
        );
        assert!(!out.contains("sha256:"), "must not leak ids: {out}");
    }

    // The orientation map's hottest-symbol section is bounded by `limit`.
    #[test]
    fn orientation_bounds_the_hot_symbol_rows() {
        let (_tmp, _root, _graph_path, graph) = build_affected_project();
        let mut params = input("");
        params.query = None;
        let out = execute_orientation(&graph, &params, 1).expect("orientation");
        assert_eq!(
            out.matches(" connection(s)").count(),
            1,
            "limit=1 must render a single hot-symbol row: {out}"
        );
    }

    // The new `relations`/`depth` fields are optional and must not break parsing
    // of the existing input shape.
    #[test]
    fn affected_fields_default_when_absent() {
        let parsed: CompassQueryInput =
            serde_json::from_value(json!({"mode": "affected", "query": "foo"})).unwrap();
        assert!(parsed.relations.is_none());
        assert!(parsed.depth.is_none());
        assert_eq!(QueryIntent::parse(parsed.mode.as_deref()).unwrap(), QueryIntent::Affected);

        let parsed: CompassQueryInput = serde_json::from_value(json!({
            "mode": "affected",
            "query": "foo",
            "relations": ["calls", "imports"],
            "depth": 3
        }))
        .unwrap();
        assert_eq!(parsed.relations.as_deref(), Some(&["calls".to_string(), "imports".to_string()][..]));
        assert_eq!(parsed.depth, Some(3));
    }

    // A `callees` call on a leaf symbol resolves only the seed (no edges). The
    // report must say there are no callees rather than presenting the seed row as
    // if it were a result.
    #[test]
    fn callees_of_a_leaf_notes_no_relationships() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::write(
            root.join("main.rs"),
            "fn authenticate(user: &str) { let _ = user; }\n\
             fn login() { authenticate(\"x\"); }\n",
        )
        .unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        // `authenticate` calls nothing.
        let out = execute_query(
            Some(&engine),
            QueryIntent::Callees,
            &input("crate::authenticate"),
            20,
            false,
            &root,
            None,
        )
        .expect("callees query must succeed");

        assert!(
            out.contains("No callees found"),
            "a leaf callees report must state there are no callees, got: {out}"
        );
        assert!(
            !out.contains("Relationships"),
            "a leaf callees report must have no edges, got: {out}"
        );
    }

    // `explore` on a leaf returns the seed's verified source (a real result), so
    // the "no related symbols" note must NOT fire even though there are no edges.
    #[test]
    fn explore_leaf_with_source_omits_the_no_neighborhood_note() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::write(
            root.join("main.rs"),
            "fn authenticate(user: &str) { let _ = user; }\n\
             fn login() { authenticate(\"x\"); }\n",
        )
        .unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        let out = execute_query(
            Some(&engine),
            QueryIntent::Explore,
            &input("crate::authenticate"),
            20,
            false,
            &root,
            None,
        )
        .expect("explore query must succeed");

        assert!(
            out.contains("### main.rs"),
            "explore must render the seed's verified source, got: {out}"
        );
        assert!(
            !out.contains("No related symbols found"),
            "the no-neighborhood note must not fire when source is rendered, got: {out}"
        );
    }

    // A `path` filter that excludes a caller's directory must not make the report
    // claim "No callers found" when a caller exists outside the filter.
    #[test]
    fn filtered_callers_do_not_claim_none_exist() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/seed.rs"), "pub fn seed() {}\n").unwrap();
        std::fs::write(
            root.join("b/caller.rs"),
            "pub fn caller() { crate::seed::seed(); }\n",
        )
        .unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        // Scope to `a`, which holds the seed but not the caller in `b`.
        let mut params = input("crate::seed::seed");
        params.path = Some("a".to_string());
        let out = execute_query(Some(&engine), QueryIntent::Callers, &params, 20, false, &root, None)
            .expect("callers query must succeed");

        assert!(
            !out.contains("No callers found"),
            "a filtered-out caller must not be reported as none existing, got: {out}"
        );
        assert!(
            out.contains("outside the `path` filter"),
            "the report should say relationships fell outside the filter, got: {out}"
        );
    }

    // `discover` must route a natural-language question to seed symbols and render
    // them, rather than failing or falling back to a bare search.
    #[test]
    fn discover_intent_routes_a_natural_language_question() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::write(
            root.join("main.rs"),
            "fn authenticate(user: &str) { let _ = user; }\n\
             fn login() { authenticate(\"x\"); }\n",
        )
        .unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        let out = execute_query(
            Some(&engine),
            QueryIntent::Discover,
            &input("authenticate"),
            20,
            false,
            &root,
            None,
        )
        .expect("discover query must succeed");

        assert!(
            out.starts_with("# Compass discover:"),
            "discover report must have its own header, got: {out}"
        );
        assert!(
            out.contains("**Mode:** discover"),
            "discover report must name the mode, got: {out}"
        );
    }

    // `discover` must honor a `path` filter as a source scope, so a question can be
    // constrained to a file or directory.
    #[test]
    fn discover_honors_path_scope() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/auth.rs"),
            "fn authenticate(user: &str) { let _ = user; }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("main.rs"),
            "fn unrelated() { let _ = 1; }\n",
        )
        .unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        let mut params = input("authenticate");
        params.path = Some("src".to_string());
        let out = execute_query(Some(&engine), QueryIntent::Discover, &params, 20, false, &root, None)
            .expect("scoped discover must succeed");

        assert!(
            out.starts_with("# Compass discover:"),
            "scoped discover report must have its own header, got: {out}"
        );
        assert!(
            out.contains("**Path filter:** src"),
            "scoped discover report must echo its path scope, got: {out}"
        );
    }

    // A discovery diagnostic can name an ambiguous seed by raw node id; the
    // report must relabel it to a name, matching the structural report.
    #[test]
    fn discover_diagnostics_name_seeds_not_raw_ids() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(
            root.join("a/m.rs"),
            "pub fn handler_request_flow() -> u32 { 1 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b/m.rs"),
            "pub fn handler_request_flow() -> u32 { 2 }\n",
        )
        .unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        // Two same-named functions make the seed ambiguous, which emits a
        // diagnostic that Compass interpolates with raw node ids.
        let out = execute_query(
            Some(&engine),
            QueryIntent::Discover,
            &input("handler request flow"),
            20,
            false,
            &root,
            None,
        )
        .expect("discover must succeed");

        assert!(
            !out.contains("sha256:"),
            "discover diagnostics must not leak raw node ids, got: {out}"
        );
        assert!(
            out.contains("crate::m::handler_request_flow"),
            "the seed should be named, got: {out}"
        );
    }

    // `explore` with a symbol set must gather the whole neighborhood in one call
    // (resolving every symbol), rather than requiring one call per symbol.
    #[test]
    fn explore_accepts_a_symbol_set() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::write(
            root.join("main.rs"),
            "fn authenticate(user: &str) { let _ = user; }\n\
             fn login() { authenticate(\"x\"); }\n\
             fn logout() { let _ = login; }\n",
        )
        .unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        let mut params = input("explore set");
        params.symbols = Some(vec![
            "crate::authenticate".to_string(),
            "crate::login".to_string(),
        ]);
        let out = execute_query(Some(&engine), QueryIntent::Explore, &params, 20, false, &root, None)
            .expect("multi-symbol explore must succeed");

        assert!(
            out.contains("**Mode:** explore"),
            "report must name the explore mode, got: {out}"
        );
        assert!(
            out.contains("# Compass query: crate::authenticate, crate::login"),
            "the header must name the whole symbol set, not just the first, got: {out}"
        );
        assert!(
            out.contains("authenticate"),
            "explore must resolve the first symbol, got: {out}"
        );
        assert!(
            out.contains("login"),
            "explore must resolve the second symbol, got: {out}"
        );
    }

    // The `context` intent must compose a task packet for a target (declaration +
    // callers + callees + tests + impact), resolving the target and surfacing its
    // source in one call.
    #[test]
    fn context_intent_composes_task_packet() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::write(
            root.join("main.rs"),
            "fn authenticate(user: &str) { let _ = user; }\n\
             fn login() { authenticate(\"x\"); }\n",
        )
        .unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        // `context` resolves exact ids/names/qualified names; Compass stores the
        // plain `name` as `authenticate()`, so use the qualified name here.
        let out = execute_query(
            Some(&engine),
            QueryIntent::Context,
            &input("crate::authenticate"),
            20,
            false,
            &root,
            None,
        )
        .expect("context query must succeed");

        assert!(
            out.contains("# Compass context: crate::authenticate"),
            "context report must name the target, got: {out}"
        );
        assert!(
            out.contains("**Resolved node:** `crate::authenticate`"),
            "the resolved node must be labeled by name, not a raw id, got: {out}"
        );
        assert!(
            !out.contains("sha256:"),
            "the context report must not leak raw node ids, got: {out}"
        );
        assert!(
            out.contains("fn authenticate"),
            "context must surface the declaration source, got: {out}"
        );
    }

    // An ambiguous/not-found `context` target must list candidates with their
    // resolved name/kind/file, not raw `sha256:` node ids, so the model can pick
    // the right symbol (and know what to refine to).
    #[test]
    fn context_ambiguous_candidates_show_names_not_raw_ids() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::write(
            root.join("main.rs"),
            "fn authenticate(user: &str) { let _ = user; }\n\
             fn login() { authenticate(\"x\"); }\n",
        )
        .unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        // `authenticate` (no namespace) does not resolve exactly, so Compass
        // returns a not-found target with candidate ids.
        let out = execute_query(
            Some(&engine),
            QueryIntent::Context,
            &input("authenticate"),
            20,
            false,
            &root,
            None,
        )
        .expect("context query must succeed");

        assert!(
            out.contains("crate::authenticate"),
            "candidates must be labeled with their qualified name, got: {out}"
        );
        assert!(
            !out.contains("sha256:"),
            "candidates must not leak raw node ids, got: {out}"
        );
    }

    // Oversized explore symbol sets must be trimmed to Compass's hard ceiling so the
    // call does not fail outright, and the requested/queried counts must be reported.
    #[test]
    fn clamp_explore_symbols_trims_to_cap() {
        let mut small = vec!["a".to_string(), "b".to_string()];
        assert_eq!(clamp_explore_symbols(&mut small), (2, 2));
        assert_eq!(small.len(), 2, "a set within the cap is untouched");

        let mut oversized: Vec<String> =
            (0..COMPASS_MAX_CANDIDATES + 5).map(|i| format!("s{i}")).collect();
        let (requested, queried) = clamp_explore_symbols(&mut oversized);
        assert_eq!(requested, COMPASS_MAX_CANDIDATES + 5);
        assert_eq!(queried, COMPASS_MAX_CANDIDATES);
        assert_eq!(oversized.len(), COMPASS_MAX_CANDIDATES);
        assert_eq!(oversized.first().map(String::as_str), Some("s0"));
    }

    // A file already rendered as digest-verified source must not also be emitted as
    // a resolved-node snippet, and a file appearing in several context sections must
    // be rendered once.
    #[test]
    fn render_does_not_duplicate_verified_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let anchor = anchor("a.rs", 1, 2);
        let view = ResponseView {
            hits: Vec::new(),
            nodes: vec![NodeView {
                id: "node:a".to_string(),
                name: "a".to_string(),
                qualified_name: "crate::a".to_string(),
                kind: "function".to_string(),
                roles: Vec::new(),
                file: Some("a.rs".to_string()),
                source: Some(anchor.clone()),
            }],
            edges: Vec::new(),
            paths: Vec::new(),
            // The same file appears twice (as two context sections would produce).
            files: vec![
                FileView {
                    path: "a.rs".to_string(),
                    digest: "sha256:test".to_string(),
                    source: Some("fn a() {}\n".to_string()),
                    truncated: false,
                },
                FileView {
                    path: "a.rs".to_string(),
                    digest: "sha256:test".to_string(),
                    source: Some("fn a() {}\n".to_string()),
                    truncated: false,
                },
            ],
            truncated: false,
            diagnostics: Vec::new(),
            filtered_out: false,
            filter_dropped_edges: false,
            diagnostic_codes: Default::default(),
        };
        let rendered = format_view("a", QueryIntent::Context, 20, None, &view, dir.path());
        let headers = rendered.matches("### a.rs").count();
        assert_eq!(headers, 1, "duplicate verified file must render once: {rendered}");
        let fences = rendered.matches("```").count();
        // exactly one fenced block (the single rendered file body).
        assert_eq!(fences, 2, "one fenced block expected, got {fences}: {rendered}");
    }

    // A stale verified file (source digest differs from disk) must NOT suppress the
    // node's own snippet: the snippet read from disk is the only fresh source.
    #[test]
    fn stale_verified_file_still_renders_node_snippet() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let view = ResponseView {
            hits: Vec::new(),
            nodes: vec![NodeView {
                id: "node:a".to_string(),
                name: "a".to_string(),
                qualified_name: "crate::a".to_string(),
                kind: "function".to_string(),
                roles: Vec::new(),
                file: Some("a.rs".to_string()),
                source: Some(anchor("a.rs", 1, 2)),
            }],
            edges: Vec::new(),
            paths: Vec::new(),
            files: vec![FileView {
                path: "a.rs".to_string(),
                    digest: "sha256:test".to_string(),
                source: None,
                truncated: false,
            }],
            truncated: false,
            diagnostics: Vec::new(),
            filtered_out: false,
            filter_dropped_edges: false,
            diagnostic_codes: Default::default(),
        };
        let rendered = format_view("a", QueryIntent::Explore, 20, None, &view, dir.path());
        assert!(
            rendered.contains("1| fn a() {}"),
            "stale verified file must still show the node snippet: {rendered}"
        );
        assert!(
            rendered.contains("source unavailable"),
            "stale verified file must be flagged: {rendered}"
        );
    }

    // A shared `rendered_files` set (the `context` section loop) must suppress a
    // file already emitted by a sibling section, whether it comes back as verified
    // source or as a node snippet.
    #[test]
    fn shared_rendered_set_suppresses_sibling_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let make_view = || ResponseView {
            hits: Vec::new(),
            nodes: vec![NodeView {
                id: "node:a".to_string(),
                name: "a".to_string(),
                qualified_name: "crate::a".to_string(),
                kind: "function".to_string(),
                roles: Vec::new(),
                file: Some("a.rs".to_string()),
                source: Some(anchor("a.rs", 1, 2)),
            }],
            edges: Vec::new(),
            paths: Vec::new(),
            files: vec![FileView {
                path: "a.rs".to_string(),
                    digest: "sha256:test".to_string(),
                source: Some("fn a() {}\n".to_string()),
                truncated: false,
            }],
            truncated: false,
            diagnostics: Vec::new(),
            filtered_out: false,
            filter_dropped_edges: false,
            diagnostic_codes: Default::default(),
        };
        let mut out = String::new();
        let mut rendered_files = RenderedFiles::default();
        let mut cache = SourceCache::default();
        render_view_body_inner(
            &mut out,
            &make_view(),
            dir.path(),
            MAX_SNIPPET_ROWS,
            false,
            &mut rendered_files,
            &mut cache,
        );
        // Sibling section with the same file: neither its body nor its node snippet
        // may be emitted again.
        render_view_body_inner(
            &mut out,
            &make_view(),
            dir.path(),
            MAX_SNIPPET_ROWS,
            false,
            &mut rendered_files,
            &mut cache,
        );
        assert_eq!(
            out.matches("### a.rs").count(),
            1,
            "sibling section must not re-emit a rendered file: {out}"
        );
    }

    // Rendering a structural view must surface edges, paths, and verified source,
    // and must not emit the empty-results message when it has content.
    #[test]
    fn structural_view_renders_edges_paths_and_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        let view = ResponseView {
            hits: Vec::new(),
            nodes: vec![NodeView {
                id: "node:a".to_string(),
                name: "a".to_string(),
                qualified_name: "crate::a".to_string(),
                kind: "function".to_string(),
                roles: vec!["service".to_string()],
                file: Some("a.rs".to_string()),
                source: Some(anchor("a.rs", 1, 2)),
            }],
            edges: vec![EdgeView {
                source: "node:caller".to_string(),
                target: "node:a".to_string(),
                kind: "calls".to_string(),
            }],
            paths: vec![PathView {
                node_ids: vec!["node:caller".to_string(), "node:a".to_string()],
                weakest_confidence: "exact".to_string(),
            }],
            files: vec![FileView {
                path: "a.rs".to_string(),
                digest: "sha256:test".to_string(),
                source: Some("fn a() {}\n".to_string()),
                truncated: false,
            }],
            truncated: false,
            diagnostics: Vec::new(),
            filtered_out: false,
            filter_dropped_edges: false,
            diagnostic_codes: Default::default(),
        };
        let rendered = format_view("a", QueryIntent::Callers, 20, None, &view, dir.path());
        assert!(rendered.contains("Relationships"), "got: {rendered}");
        assert!(rendered.contains("calls"), "edge kind must render: {rendered}");
        assert!(rendered.contains("Paths"), "paths must render: {rendered}");
        assert!(
            rendered.contains("node:caller -> crate::a"),
            "path chain must render resolved labels: {rendered}"
        );
        assert!(
            rendered.contains("### a.rs"),
            "verified source file header must render: {rendered}"
        );
        assert!(
            !rendered.contains("No matches"),
            "content view must not claim no matches: {rendered}"
        );
    }

    // More paths than MAX_PATH_ROWS must be truncated with an explicit omission
    // note, matching the nodes/edges sections (never silently dropped).
    #[test]
    fn paths_section_notes_omitted_rows() {
        let dir = tempfile::tempdir().unwrap();
        let paths = (0..MAX_PATH_ROWS + 3)
            .map(|i| PathView {
                node_ids: vec![format!("node:{i}")],
                weakest_confidence: "exact".to_string(),
            })
            .collect();
        let view = ResponseView {
            hits: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            paths,
            files: Vec::new(),
            truncated: false,
            diagnostics: Vec::new(),
            filtered_out: false,
            filter_dropped_edges: false,
            diagnostic_codes: Default::default(),
        };
        let rendered = format_view("a", QueryIntent::Impact, 20, None, &view, dir.path());
        assert!(
            rendered.contains("more path(s) omitted"),
            "an omitted-paths note must render, got: {rendered}"
        );
    }

    // A `path` filter must drop paths (and their nodes/edges) that fall outside
    // it, so a filtered `traverse` never shows a chain with hidden nodes as raw
    // ids.
    #[test]
    fn path_filter_drops_paths_outside_it() {
        let inside = compass_model::query_contract::QueryNode {
            id: "n:inside".to_string(),
            kind: compass_model::code_graph::NodeKind::Function,
            roles: Vec::new(),
            name: "inside".to_string(),
            qualified_name: "crate::inside".to_string(),
            language: None,
            framework: None,
            source: Some(anchor("src/a.rs", 1, 2)),
            details: None,
            evidence: Vec::new(),
        };
        let outside = compass_model::query_contract::QueryNode {
            id: "n:outside".to_string(),
            name: "outside".to_string(),
            qualified_name: "crate::outside".to_string(),
            source: Some(anchor("other/b.rs", 1, 2)),
            ..inside.clone()
        };
        let response = CodeQueryResponse {
            schema: compass_model::query_contract::CODE_QUERY_SCHEMA_V1.to_string(),
            operation: compass_model::query_contract::CodeQueryOperation::Callers,
            results: Vec::new(),
            nodes: vec![inside, outside],
            edges: Vec::new(),
            files: Vec::new(),
            paths: vec![compass_model::query_contract::QueryPath {
                id: "p1".to_string(),
                node_ids: vec!["n:inside".to_string(), "n:outside".to_string()],
                edge_ids: Vec::new(),
                weakest_resolution: compass_model::provenance::ResolutionState::Exact,
                weakest_confidence: compass_model::provenance::EvidenceConfidence::Exact,
            }],
            diagnostics: Vec::new(),
            limits: Default::default(),
            truncated: false,
        };
        let view = ResponseView::from(response, Some("src"));
        assert!(
            view.paths.is_empty(),
            "a path with any node outside the filter must be dropped"
        );
        assert_eq!(view.nodes.len(), 1, "only the in-filter node is kept");
        assert_eq!(view.nodes[0].id, "n:inside");
    }

    // A `path` filter that removes every result Compass returned must set
    // `filtered_out`, so an empty report blames the filter rather than the symbol.
    #[test]
    fn path_filter_that_excludes_all_sets_filtered_out() {
        let node = compass_model::query_contract::QueryNode {
            id: "n:a".to_string(),
            kind: compass_model::code_graph::NodeKind::Function,
            roles: Vec::new(),
            name: "a".to_string(),
            qualified_name: "crate::a".to_string(),
            language: None,
            framework: None,
            source: Some(anchor("src/a.rs", 1, 2)),
            details: None,
            evidence: Vec::new(),
        };
        let response = CodeQueryResponse {
            schema: compass_model::query_contract::CODE_QUERY_SCHEMA_V1.to_string(),
            operation: compass_model::query_contract::CodeQueryOperation::Callers,
            results: Vec::new(),
            nodes: vec![node],
            edges: Vec::new(),
            files: Vec::new(),
            paths: Vec::new(),
            diagnostics: Vec::new(),
            limits: Default::default(),
            truncated: false,
        };

        // Excluded by the filter -> flagged; kept -> not flagged; no filter -> not.
        assert!(ResponseView::from(response.clone(), Some("other")).filtered_out);
        assert!(!ResponseView::from(response.clone(), Some("src")).filtered_out);
        assert!(!ResponseView::from(response, None).filtered_out);
    }

    // When a path filter excluded everything, the report must say so instead of
    // advising a different symbol name (the symbol did resolve).
    #[test]
    fn filtered_out_report_blames_the_filter_not_the_symbol() {
        let view = ResponseView {
            hits: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            paths: Vec::new(),
            files: Vec::new(),
            truncated: false,
            diagnostics: Vec::new(),
            filtered_out: true,
            filter_dropped_edges: false,
            diagnostic_codes: Default::default(),
        };
        let dir = tempfile::tempdir().unwrap();
        let out = format_view("crate::a", QueryIntent::Explore, 20, Some("other"), &view, dir.path());
        assert!(
            out.contains("path") && out.contains("filter"),
            "the report should name the path filter as the cause, got: {out}"
        );
        assert!(
            !out.contains("Try a different symbol name"),
            "the report must not suggest the symbol was wrong, got: {out}"
        );
    }

    // An ambiguous operand must be reported as such, not as "no matches": the
    // symbol resolved, just to more than one node, so the advice is to qualify it.
    #[test]
    fn ambiguous_operand_report_advises_qualifying_not_retrying() {
        let view = ResponseView {
            hits: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            paths: Vec::new(),
            files: Vec::new(),
            truncated: false,
            diagnostics: vec!["Symbol \"handler\" matched 2 nodes".to_string()],
            filtered_out: false,
            filter_dropped_edges: false,
            diagnostic_codes: [compass_model::query_contract::QueryDiagnosticCode::AmbiguousMatch].into_iter().collect(),
        };
        let dir = tempfile::tempdir().unwrap();
        let out = format_view("handler", QueryIntent::Callers, 20, None, &view, dir.path());
        assert!(
            out.contains("ambiguous") && out.contains("qualified name"),
            "the report should say the operand is ambiguous and how to fix it, got: {out}"
        );
        assert!(
            !out.contains("Try a different symbol name"),
            "an ambiguous symbol is not a wrong symbol name, got: {out}"
        );
    }

    // A reversed trail (the two symbols are connected, just the other way) must be
    // reported as a direction problem, not as "no matches".
    #[test]
    fn reversed_trail_report_names_the_direction() {
        let view = ResponseView {
            hits: Vec::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
            paths: Vec::new(),
            files: Vec::new(),
            truncated: false,
            diagnostics: vec!["A trail connects a and b, but not in the requested direction"
                .to_string()],
            filtered_out: false,
            filter_dropped_edges: false,
            diagnostic_codes: [compass_model::query_contract::QueryDiagnosticCode::DirectionMismatch]
                .into_iter()
                .collect(),
        };
        let dir = tempfile::tempdir().unwrap();
        let out = format_view(
            "crate::a",
            QueryIntent::Traverse,
            20,
            None,
            &view,
            dir.path(),
        );
        assert!(
            out.contains("direction"),
            "the report should name the direction problem, got: {out}"
        );
        assert!(
            !out.contains("Try a different symbol name"),
            "a reversed trail is not a wrong symbol name, got: {out}"
        );
    }

    // A large symbol set must be summarized in the header, not printed in full.
    #[test]
    fn display_target_bounds_a_large_symbol_set() {
        let params = CompassQueryInput {
            symbols: Some((0..300).map(|i| format!("crate::some_symbol_{i}")).collect()),
            ..input("x")
        };
        let target = params.display_target(QueryIntent::Explore);
        assert!(
            target.chars().count() <= MAX_DISPLAY_TARGET_CHARS + 32,
            "the display target must be bounded, got {} chars",
            target.chars().count()
        );
        assert!(
            target.contains("more)"),
            "an omitted-tail marker must be present, got: {target}"
        );
    }

    // A pathological single symbol must not blow past the bound either.
    #[test]
    fn display_target_bounds_a_single_long_symbol() {
        let params = CompassQueryInput {
            symbols: Some(vec!["z".repeat(5000)]),
            ..input("x")
        };
        let target = params.display_target(QueryIntent::Explore);
        assert!(
            target.chars().count() <= MAX_DISPLAY_TARGET_CHARS + 8,
            "a single oversized symbol must be truncated, got {} chars",
            target.chars().count()
        );
    }

    // A long free-form query must not blow out the report header or the tool
    // title; whitespace is collapsed and the text is truncated.
    #[test]
    fn header_label_bounds_a_long_query() {
        let long = "z".repeat(10_000);
        let label = header_label(&long);
        assert!(
            label.chars().count() <= MAX_DISPLAY_TARGET_CHARS + 1,
            "the header label must be bounded, got {} chars",
            label.chars().count()
        );
        assert!(label.ends_with('…'), "truncation must be marked, got: {label}");

        assert_eq!(header_label("  fn   foo \n bar "), "fn foo bar");

        let params = CompassQueryInput {
            query: Some(long),
            ..input("x")
        };
        assert!(
            params.display_target(QueryIntent::Search).chars().count()
                <= MAX_DISPLAY_TARGET_CHARS + 1,
            "a long query operand must be bounded in the title too"
        );
    }

    // Every report header must bound its operand, including the `context` and
    // `discover` reports that build their own headers.
    #[test]
    fn context_and_discover_headers_are_bounded() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        let long = "x".repeat(3000);
        for intent in [QueryIntent::Context, QueryIntent::Discover] {
            let out = execute_query(Some(&engine), intent, &input(&long), 20, false, &root, None)
                .expect("query must succeed");
            let header = out.lines().next().unwrap_or_default();
            // The label is bounded to MAX_DISPLAY_TARGET_CHARS; allow the
            // `# Compass <mode>: ` prefix on top.
            assert!(
                header.chars().count() <= MAX_DISPLAY_TARGET_CHARS + 32,
                "the {intent:?} header must be bounded, got {} chars: {header}",
                header.chars().count()
            );
        }
    }

    // `traverse` without a target is a caller error, surfaced clearly rather than
    // silently degraded to a search.
    #[test]
    fn traverse_requires_a_target_symbol() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        let mut params = input("authenticate");
        params.mode = Some("traverse".to_string());
        let err = execute_query(
            Some(&engine),
            QueryIntent::Traverse,
            &params,
            20,
            false,
            &root,
            None,
        )
        .expect_err("traverse without target must error");
        assert!(
            err.to_string().contains("target"),
            "error should mention the missing target, got: {err}"
        );
    }

    // `traverse` with a target but no source must be a caller error too: an empty
    // source would otherwise reach Compass and come back as a bare "No matches",
    // which reads like a graph result rather than a malformed call.
    #[test]
    fn traverse_requires_a_source_symbol() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        let mut params = input("");
        params.query = None;
        params.mode = Some("traverse".to_string());
        params.target = Some("logout".to_string());
        let err = execute_query(
            Some(&engine),
            QueryIntent::Traverse,
            &params,
            20,
            false,
            &root,
            None,
        )
        .expect_err("traverse without source must error");
        assert!(
            err.to_string().contains("source"),
            "error should mention the missing source, got: {err}"
        );
    }

    // A `traverse` whose endpoints resolve but whose trail only exists in the
    // reverse direction must name its endpoints, not leak raw `sha256:` ids in the
    // direction-mismatch diagnostic.
    #[test]
    fn traverse_direction_mismatch_names_its_endpoints() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        std::fs::write(
            root.join("main.rs"),
            "fn authenticate(user: &str) { let _ = user; }\n\
             fn login() { authenticate(\"x\"); }\n",
        )
        .unwrap();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        // `authenticate` does not call `login`; the trail only exists reversed.
        let mut params = input("crate::authenticate");
        params.source = Some("crate::authenticate".to_string());
        params.target = Some("crate::login".to_string());
        let out = execute_query(Some(&engine), QueryIntent::Traverse, &params, 20, false, &root, None)
            .expect("traverse must succeed");

        assert!(
            !out.contains("sha256:"),
            "the traverse diagnostic must not leak raw node ids, got: {out}"
        );
        assert!(
            out.contains("crate::authenticate") && out.contains("crate::login"),
            "the diagnostic should name both endpoints, got: {out}"
        );
        assert!(
            out.contains("direction") && !out.contains("Try a different symbol name"),
            "a reversed trail must be reported as a direction problem, got: {out}"
        );
    }

    // `relabel_ids` replaces whole ids longest-first and leaves unknown text alone.
    #[test]
    fn relabel_ids_replaces_known_ids_only() {
        let mut labels = HashMap::new();
        labels.insert("sha256:aa".to_string(), "crate::a".to_string());
        labels.insert("sha256:aab".to_string(), "crate::ab".to_string());
        let text = "trail connects sha256:aab and sha256:aa, not sha256:zz";
        assert_eq!(
            relabel_ids(text, &labels),
            "trail connects crate::ab and crate::a, not sha256:zz"
        );
        assert_eq!(relabel_ids(text, &HashMap::new()), text);
    }

    // `context` composes its own report from Compass's task-context API, which
    // has no path scope; a `path` filter must be rejected rather than silently
    // ignored (which would return an unscoped packet the caller thinks is scoped).
    #[test]
    fn context_rejects_a_path_filter() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine =
            compass_query::open(&output_dir.join("compass-out/graph.json"), None, &output_dir)
                .expect("open after build");

        let params = CompassQueryInput {
            path: Some("src".to_string()),
            ..input("authenticate")
        };
        let err = execute_query(Some(&engine), QueryIntent::Context, &params, 20, false, &root, None)
            .expect_err("context with a path filter must error");
        assert!(
            err.to_string().contains("path"),
            "error should name the unsupported filter, got: {err}"
        );
    }

    // The staleness scan is throttled per cache dir: an unseen cache is never
    // short-circuited, and a just-recorded scan is treated as fresh for the TTL.
    // This guards the caveat that the mtime walk must not run on every query.
    // (The time-based expiry half is covered by STALE_RESCAN_TTL + the integration
    // behavior in fresh/stale index tests; we keep this deterministic and instant.)
    #[test]
    fn staleness_scan_is_throttled_per_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().to_path_buf();

        assert!(
            !recently_scanned(&cache),
            "an unseen cache must never be short-circuited as fresh"
        );
        record_scan(&cache);
        assert!(
            recently_scanned(&cache),
            "a just-recorded scan must throttle the next reuse in this window"
        );

        // A different cache dir is tracked independently.
        let other = dir.path().join("other");
        assert!(
            !recently_scanned(&other),
            "throttle state must be per-cache, not global"
        );
    }

    // A git branch/commit switch must mark the index stale even when no source
    // file mtime advances. We isolate the SHA-based detection by amending the
    // commit (new SHA, identical tree) so the mtime walk alone would report
    // "not stale". Skips gracefully when git is unavailable in the test env.
    #[test]
    fn branch_change_forces_stale() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init"]) {
            return; // git not available; nothing to exercise.
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        git(&["add", "."]);
        if !git(&["commit", "-m", "init"]) {
            return;
        }
        // Sanity: the feature this test exercises needs a real SHA.
        if current_git_sha(&root).is_none() {
            return;
        }

        let output_dir = root.join(".jcode/cache/compass");
        let ast_cache_root = root.join(".jcode/cache/.ast-cache");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::create_dir_all(&ast_cache_root).unwrap();
        let graph_path = output_dir.join("compass-out/graph.json");
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");

        // Sidecar was written at build time and matches HEAD.
        let sha1 = current_git_sha(&root).expect("sha after init");
        assert_eq!(index_git_sha(&output_dir).as_deref(), Some(sha1.as_str()));

        // A freshly built index is not stale against its own commit.
        assert!(
            !index_is_stale(&root, &graph_path, current_git_sha(&root).as_deref(), &output_dir, false),
            "just-built index should not be stale against its own commit"
        );

        // Re-point HEAD at a new commit with an identical tree: file mtimes are
        // unchanged, so only the SHA mismatch can detect staleness.
        // Use a distinct commit message to guarantee a new SHA even if
        // timestamps are clamped by the test environment.
        assert!(git(&["commit", "--amend", "-m", "init-amended"]), "amend should succeed");
        let sha2 = current_git_sha(&root).expect("sha after amend");
        assert_ne!(sha1, sha2, "amend must produce a new commit SHA");

        assert!(
            index_is_stale(&root, &graph_path, current_git_sha(&root).as_deref(), &output_dir, false),
            "branch/commit change must mark the index stale even with unchanged mtimes"
        );
    }

    // `current_git_sha_cached` must resolve a real repo's HEAD and reuse it
    // within the TTL (so we don't fork `git` on every query), while a non-git
    // dir falls back to None just like the raw `current_git_sha`. Skips when git
    // is unavailable.
    #[test]
    fn git_sha_is_cached_per_working_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Non-git dir: neither raw nor cached should resolve a SHA.
        assert!(current_git_sha(&root).is_none());
        assert!(current_git_sha_cached(&root).is_none());

        let ok = std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return; // git not available.
        }
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&root)
            .status()
            .ok();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .status()
            .ok();
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&root)
            .status()
            .ok();
        if !std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return;
        }

        let sha1 = current_git_sha_cached(&root).expect("cached sha on a real repo");
        let sha2 = current_git_sha_cached(&root).expect("cached sha reused");
        assert_eq!(sha1, sha2, "SHA must be reused within the cache TTL");
    }

    // `git_repo_identity` must return one stable absolute value from any
    // subdirectory of a repo. This is what makes the shared cache key identical
    // across all worktrees of one repo. Skips when git is unavailable.
    #[test]
    fn git_repo_identity_is_stable_across_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("a/b/main.rs"), "fn a() {}\n").unwrap();
        git(&["add", "."]);
        if !git(&["commit", "-m", "init"]) {
            return;
        }

        let top = git_repo_identity(&root).expect("identity in repo");
        assert!(std::path::Path::new(&top).is_absolute(), "identity must be absolute: {top}");
        let sub = git_repo_identity(&root.join("a/b")).expect("identity in subdir");
        assert_eq!(top, sub, "identity must be identical from any subdir");
        assert!(
            !top.is_empty(),
            "identity must not be empty"
        );
    }
    #[test]
    fn resolve_compass_cache_uses_shared_path_for_git_repos() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Init a real git repo so current_git_sha succeeds.
        let ok = std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return; // git not available.
        }
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&root)
            .status()
            .ok();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .status()
            .ok();
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&root)
            .status()
            .ok();
        if !std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return;
        }

        let (_home, _home_path) = HomeGuard::set();
        let cache = resolve_compass_cache(&root);
        assert!(cache.is_shared, "git repo should use shared cache");
        assert!(
            cache
                .output_dir
                .to_string_lossy()
                .contains(std::path::Path::new(COMPASS_CACHE_HOME).to_string_lossy().as_ref()),
            "shared cache should be under the jcode home /compass dir: {}",
            cache.output_dir.display()
        );
        assert!(cache.graph_path.ends_with("compass-out/graph.json"));
    }

    #[test]
    fn resolve_compass_cache_falls_back_to_local_for_non_git() {
        let (_home, home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let cache = resolve_compass_cache(&root);
        assert!(!cache.is_shared, "non-git dir should not use a git per-SHA cache");
        assert!(
            cache.output_dir.starts_with(&home_path),
            "non-git cache should live under the jcode home, not the project: {}",
            cache.output_dir.display()
        );
        assert!(!cache.output_dir.starts_with(&root), "cache must not be inside the project dir");
        assert!(cache.graph_path.ends_with("compass-out/graph.json"));
    }
    #[test]
    fn stale_index_cleanup_removes_sidecar_and_lock() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Create a git repo with one commit.
        let ok = std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&root)
            .status()
            .ok();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .status()
            .ok();
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&root)
            .status()
            .ok();
        if !std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return;
        }

        let (_home, _home_path) = HomeGuard::set();
        let cache = resolve_compass_cache(&root);
        assert!(cache.is_shared);
        let graph_path = &cache.graph_path;
        let output_dir = &cache.output_dir;

        // Build the index manually to create sidecar files.
        build_compass_index(&root, output_dir, &cache.ast_cache_root).expect("build should succeed");

        // Verify sidecar files exist.
        assert!(
            output_dir.join(GIT_SHA_FILE).exists(),
            "git-sha sidecar should exist after build"
        );
        // Note: .compass-build.lock is only created during concurrent builds via with_build_lock,
        // so we don't assert its existence here.

        // Verify the fresh index is not stale.
        assert!(
            !index_is_stale(&root, graph_path, current_git_sha(&root).as_deref(), output_dir, true),
            "fresh index should not be stale"
        );
    }
    #[test]
    fn shared_cache_ignores_uncommitted_local_edits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Init a real git repo.
        let ok = std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return;
        }
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&root)
            .status()
            .ok();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&root)
            .status()
            .ok();
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&root)
            .status()
            .ok();
        if !std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return;
        }

        let (_home, _home_path) = HomeGuard::set();
        let cache = resolve_compass_cache(&root);
        assert!(cache.is_shared);
        let graph_path = &cache.graph_path;
        let output_dir = &cache.output_dir;

        // Build the index.
        build_compass_index(&root, output_dir, &cache.ast_cache_root).expect("build should succeed");

        // Verify the index is fresh initially.
        assert!(
            !index_is_stale(&root, graph_path, current_git_sha(&root).as_deref(), output_dir, true),
            "fresh index should not be stale"
        );

        // Uncommitted local edits in one worktree must NOT make the shared index
        // stale: the shared index is keyed by commit SHA and represents only the
        // committed tree. A local edit must never force a shared rebuild from a
        // dirty worktree (which would leak that worktree's uncommitted code into
        // the index all clean worktrees on the same SHA also read).
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(root.join("modified.rs"), "fn b() {}\n").unwrap();

        assert!(
            !index_is_stale(&root, graph_path, current_git_sha(&root).as_deref(), output_dir, true),
            "shared index must stay fresh under uncommitted local edits (SHA unchanged)"
        );
    }

    // For a shared cache, staleness is driven purely by the commit SHA.
    // Advancing HEAD (amending produces a new SHA with an identical tree) must
    // mark the shared index stale, since a shared index represents exactly one
    // committed tree keyed by that SHA.
    #[test]
    fn shared_cache_rebuilds_on_commit_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        git(&["add", "."]);
        if !git(&["commit", "-m", "init"]) {
            return;
        }
        if current_git_sha(&root).is_none() {
            return;
        }

        let (_home, _home_path) = HomeGuard::set();
        let cache = resolve_compass_cache(&root);
        assert!(cache.is_shared);
        let graph_path = &cache.graph_path;
        let output_dir = &cache.output_dir;
        build_compass_index(&root, output_dir, &cache.ast_cache_root).expect("build");

        // Fresh against its own commit.
        assert!(
            !index_is_stale(&root, graph_path, current_git_sha(&root).as_deref(), output_dir, true),
            "shared index should be fresh against its own commit"
        );

        // Amend -> new SHA, identical tree -> shared index must turn stale.
        assert!(git(&["commit", "--amend", "-m", "init-amended"]), "amend should succeed");
        assert!(
            index_is_stale(&root, graph_path, current_git_sha(&root).as_deref(), output_dir, true),
            "shared index must be stale after the commit SHA changes"
        );
    }

    #[test]
    fn looks_like_sha_classifies_commit_hashes() {
        assert!(looks_like_sha(&"a".repeat(40)));
        assert!(looks_like_sha(&"0".repeat(40)));
        assert!(looks_like_sha(&"a".repeat(64)), "sha256-object-format hash");
        assert!(!looks_like_sha("short"));
        assert!(!looks_like_sha(&"g".repeat(40)), "non-hex must not match");
        assert!(!looks_like_sha(&"g".repeat(64)), "non-hex 64 must not match");
        assert!(!looks_like_sha(AST_CACHE_DIR));
        assert!(!looks_like_sha(WORKSPACE_DIR));
    }

    // The shared-cache project id must be deterministic and stable: the same
    // repo id must hash to the same value across calls (and thus across
    // processes/builds), so an on-disk cache is never orphaned by id drift.
    #[test]
    fn short_id_is_deterministic_and_hex() {
        let a = short_id("/some/repo/.git");
        let b = short_id("/some/repo/.git");
        assert_eq!(a, b, "same input must hash identically");
        assert_eq!(a.len(), a.chars().count());
        assert_eq!(a.chars().count(), 64, "full 32-byte SHA-256 digest = 64 hex chars");
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit()),
            "id must be hex only, got {a}"
        );
        // Different inputs differ.
        assert_ne!(short_id("/repo/one/.git"), short_id("/repo/two/.git"));
    }

    #[test]
    fn prune_stale_sha_outputs_spares_reachable_shared_and_workspace() {
        let (_home, _home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // A real git repo so `git rev-list --all` yields a reachable SHA.
        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        git(&["add", "."]);
        if !git(&["commit", "-m", "init"]) {
            return;
        }
        let Some(reachable) = git_reachable_shas(&root) else {
            return;
        };
        assert_eq!(reachable.len(), 1, "exactly one commit in the fresh repo");
        let head_sha = reachable.iter().next().unwrap().clone();

        // Simulate the shared layout: reachable SHA dir, an unreachable old fake
        // SHA dir, the AST cache, and a workspace dir.
        let project_root = root.join("compass/proj");
        std::fs::create_dir_all(project_root.join(&head_sha)).unwrap();
        std::fs::create_dir_all(project_root.join(AST_CACHE_DIR)).unwrap();
        std::fs::create_dir_all(project_root.join(WORKSPACE_DIR)).unwrap();
        let stale_sha = "f".repeat(40);
        std::fs::create_dir_all(project_root.join(&stale_sha)).unwrap();
        // Backdate the unreachable dir beyond the retention window.
        let old = std::time::SystemTime::now()
            .checked_sub(SHA_RETENTION_TTL + std::time::Duration::from_secs(1))
            .unwrap();
        let filetime_old = filetime::FileTime::from_system_time(old);
        filetime::set_file_mtime(project_root.join(&stale_sha), filetime_old).unwrap();

        prune_stale_sha_outputs(&project_root, &root, &head_sha);

        assert!(
            project_root.join(&head_sha).exists(),
            "reachable SHA dir must be kept"
        );
        assert!(
            project_root.join(AST_CACHE_DIR).exists(),
            "shared AST cache must be kept"
        );
        assert!(
            project_root.join(WORKSPACE_DIR).exists(),
            "workspace dir must be kept"
        );
        assert!(
            !project_root.join(&stale_sha).exists(),
            "old, unreachable per-SHA dir must be pruned"
        );
    }

    // The current HEAD's per-SHA dir must survive GC even when that commit is a
    // detached checkout (unreachable from any ref): pruning it would delete the
    // index the very worktree currently uses. We simulate this directly: a
    // 40-hex `current_sha` that is NOT in `git rev-list --all` (so reachability
    // alone would not protect it), with an old mtime beyond the retention window.
    // It must still be kept purely because it equals the active HEAD.
    #[test]
    fn prune_keeps_detached_head_even_if_unreachable() {
        let (_home, _home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        git(&["add", "."]);
        if !git(&["commit", "-qm", "init"]) {
            return;
        }
        let Some(reachable) = git_reachable_shas(&root) else {
            return;
        };

        // A detached-HEAD sha that is NOT reachable from any ref (so reachability
        // alone would NOT protect it), yet describes the currently checked-out
        // commit and must survive GC.
        let detached_sha = "a".repeat(40);
        assert!(
            !reachable.contains(&detached_sha),
            "detached_sha must be unreachable so the test isolates the HEAD guard"
        );

        let project_root = root.join("compass/proj");
        std::fs::create_dir_all(project_root.join(&detached_sha)).unwrap();
        let old = std::time::SystemTime::now()
            .checked_sub(SHA_RETENTION_TTL + std::time::Duration::from_secs(1))
            .unwrap();
        filetime::set_file_mtime(
            project_root.join(&detached_sha),
            filetime::FileTime::from_system_time(old),
        )
        .unwrap();

        // GC with the active detached HEAD equal to detached_sha.
        prune_stale_sha_outputs(&project_root, &root, &detached_sha);
        assert!(
            project_root.join(&detached_sha).exists(),
            "detached current HEAD must be kept even though it is unreachable and old"
        );
    }

    // Per-commit graph dirs can remain *reachable* from any ref (e.g. a backup
    // branch) and would otherwise accumulate forever (each can be very large).
    // The hard cap must still prune the oldest beyond `SHA_INDEX_MAX_KEPT`
    // survivors, while always keeping the current HEAD — even when the HEAD is
    // itself unreachable. This is what actually bounds `~/.jcode/compass/` when
    // the 14-day TTL cannot (reachable SHAs never age out of that rule).
    #[test]
    fn prune_caps_total_kept_shas_regardless_of_reachability() {
        let (_home, _home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        git(&["add", "."]);
        if !git(&["commit", "-qm", "init"]) {
            return;
        }

        // Create more *reachable* commits than the cap by making a backup branch
        // and advancing it: these SHAs are in `git rev-list --all`, so the old
        // TTL-only GC would keep them forever. The hard cap must still prune the
        // oldest once we exceed SHA_INDEX_MAX_KEPT.
        for i in 0..(SHA_INDEX_MAX_KEPT + 2) {
            std::fs::write(root.join("main.rs"), format!("fn a{}() {{}}\n", i)).unwrap();
            git(&["add", "."]);
            // First advance off the current branch; keep committing on `backup`.
            if i == 0 {
                git(&["checkout", "-qb", "backup"]);
            }
            if !git(&["commit", "-qm", &format!("backup {i}")]) {
                return;
            }
        }
        let Some(reachable) = git_reachable_shas(&root) else {
            return;
        };
        // HEAD is now the last backup commit; `git_reachable_shas` returns all.
        assert!(
            reachable.len() >= SHA_INDEX_MAX_KEPT + 3,
            "expected >= {} reachable commits, got {}",
            SHA_INDEX_MAX_KEPT + 3,
            reachable.len()
        );
        let head_sha = current_git_sha(&root).expect("HEAD sha");

        // Create a per-SHA dir for every reachable commit, each stamped with a
        // distinct mtime so the survivors are well-defined.
        let project_root = root.join("compass/proj");
        for (i, sha) in reachable.iter().enumerate() {
            std::fs::create_dir_all(project_root.join(sha)).unwrap();
            let mtime = std::time::SystemTime::now()
                .checked_sub(std::time::Duration::from_secs((i as u64) * 3600))
                .unwrap();
            filetime::set_file_mtime(
                project_root.join(sha),
                filetime::FileTime::from_system_time(mtime),
            )
            .unwrap();
        }

        prune_stale_sha_outputs(&project_root, &root, &head_sha);

        // HEAD always survives.
        assert!(
            project_root.join(&head_sha).exists(),
            "current HEAD must survive the cap"
        );
        // HEAD is always kept; every other reachable dir is eligible for the cap,
        // so at most SHA_INDEX_MAX_KEPT non-HEAD reachable dirs may survive. This
        // is the whole point: the old TTL-only GC kept reachable dirs forever.
        let surviving: Vec<_> = reachable
            .iter()
            .filter(|n| *n != &head_sha && project_root.join(n).exists())
            .collect();
        assert!(
            surviving.len() <= SHA_INDEX_MAX_KEPT,
            "cap violated: {} non-HEAD reachable dirs survived, cap is {}",
            surviving.len(),
            SHA_INDEX_MAX_KEPT
        );
    }

    // Reachable per-SHA dirs must never be pruned by the 14-day TTL, and must not
    // be pruned by the hard cap when the total is at or below SHA_INDEX_MAX_KEPT.
    // This pins the guarantee that the cap only removes *excess* reachable dirs,
    // never everything just because they are old — otherwise long-lived stable
    // branches would be re-extracted needlessly after a 14-day idle window.
    #[test]
    fn prune_keeps_reachable_shas_when_within_cap_even_if_old() {
        let (_home, _home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        std::fs::write(root.join("main.rs"), "fn a() {}\n").unwrap();
        git(&["add", "."]);
        if !git(&["commit", "-qm", "init"]) {
            return;
        }
        // One reachable commit is enough; the fresh HEAD is protected by the
        // current_sha guard. Create the project root and a per-SHA dir for HEAD.
        let head_sha = current_git_sha(&root).expect("HEAD sha");
        let project_root = root.join("compass/proj");
        std::fs::create_dir_all(project_root.join(&head_sha)).unwrap();
        // Also add one old unreachable SHA dir past the TTL: it must be pruned
        // even though it is only one dir (TTL applies regardless of count).
        let stale_sha = "f".repeat(40);
        std::fs::create_dir_all(project_root.join(&stale_sha)).unwrap();
        let old = std::time::SystemTime::now()
            .checked_sub(SHA_RETENTION_TTL + std::time::Duration::from_secs(1))
            .unwrap();
        filetime::set_file_mtime(
            project_root.join(&stale_sha),
            filetime::FileTime::from_system_time(old),
        )
        .unwrap();

        prune_stale_sha_outputs(&project_root, &root, &head_sha);

        assert!(
            project_root.join(&head_sha).exists(),
            "reachable current dir must be kept even though it is old"
        );
        assert!(
            !project_root.join(&stale_sha).exists(),
            "unreachable TTL-expired dir must still be pruned"
        );
    }

    // When git is unavailable (git_reachable_shas → None), the hard cap must
    // still bound the cache. Without reachability info we conservatively never
    // TTL-prune, but the count cap (keep newest SHA_INDEX_MAX_KEPT + current)
    // must still apply — otherwise a git-less environment never bounds the dir.
    #[test]
    fn prune_caps_even_when_git_unavailable() {
        let _ = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // working_dir is a NON-git dir: `git rev-list --all` in it fails, so
        // git_reachable_shas returns None.
        std::fs::create_dir_all(&root).unwrap();

        let project_root = root.join("compass/proj");
        // Create more per-SHA dirs than the cap, plus a "current" sha string that
        // does not need to exist as a real git object (prune only string-compares
        // it against dir names).
        let current_sha = "a".repeat(40);
        for i in 0..(SHA_INDEX_MAX_KEPT + 3) {
            // Distinct hex names (0..n zero-padded).
            let name = format!("{i:040x}");
            std::fs::create_dir_all(project_root.join(&name)).unwrap();
        }

        prune_stale_sha_outputs(&project_root, &root, &current_sha);

        // The cap must have left at most SHA_INDEX_MAX_KEPT non-current dirs.
        let surviving: Vec<_> = std::fs::read_dir(&project_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| looks_like_sha(n) && n != &current_sha)
            .collect();
        assert!(
            surviving.len() <= SHA_INDEX_MAX_KEPT,
            "cap must still apply when git is unavailable: {} survivors > {}",
            surviving.len(),
            SHA_INDEX_MAX_KEPT
        );
    }

    // A per-SHA dir whose mtime is in the FUTURE (clock skew, `touch -d future`)
    // used to make `now.duration_since(mtime)` fail and be `continue`d — skipping
    // it entirely so it never counted toward `SHA_INDEX_MAX_KEPT`. That let a
    // future-mtime dir bypass the hard cap and accumulate unbounded. Now such a
    // dir is clamped to age 0 (newest) and still counts toward the cap.
    #[test]
    fn prune_caps_future_mtime_dirs_instead_of_bypassing_cap() {
        let _ = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // Non-git working dir so reachability is unknown and every per-SHA dir is
        // treated as "reachable" (counting toward the cap, never TTL-pruned).
        std::fs::create_dir_all(&root).unwrap();

        let project_root = root.join("compass/proj");
        let current_sha = "a".repeat(40);
        // Create more dirs than the cap; make ALL of them future-mtime so the old
        // bug would have skipped every one of them and the cap would be bypassed.
        for i in 0..(SHA_INDEX_MAX_KEPT + 3) {
            let name = format!("{i:040x}");
            std::fs::create_dir_all(project_root.join(&name)).unwrap();
            let future = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
            filetime::set_file_mtime(
                project_root.join(&name),
                filetime::FileTime::from_system_time(future),
            )
            .unwrap();
        }

        prune_stale_sha_outputs(&project_root, &root, &current_sha);

        let surviving: Vec<_> = std::fs::read_dir(&project_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| looks_like_sha(n) && n != &current_sha)
            .collect();
        assert!(
            surviving.len() <= SHA_INDEX_MAX_KEPT,
            "future-mtime dirs must still count toward the cap: {} survivors > {}",
            surviving.len(),
            SHA_INDEX_MAX_KEPT
        );
    }

    // The production caller now invokes the prune even when the current SHA
    // cannot be resolved (git unavailable, transient git failure), passing an
    // empty `current_sha`. An empty string never matches a real SHA, so no dir
    // is name-protected, but the hard cap must still be enforced. Without this
    // the cap's "applies regardless of git availability" guarantee was unreachable
    // in production (the old caller skipped the prune whenever git was missing).
    #[test]
    fn prune_caps_when_current_sha_is_empty() {
        let _ = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // Non-git working dir: reachability unknown, all dirs treated reachable.
        std::fs::create_dir_all(&root).unwrap();

        let project_root = root.join("compass/proj");
        // Empty current_sha, as the caller passes when git is unavailable.
        let current_sha = "";
        for i in 0..(SHA_INDEX_MAX_KEPT + 3) {
            let name = format!("{i:040x}");
            std::fs::create_dir_all(project_root.join(&name)).unwrap();
        }

        prune_stale_sha_outputs(&project_root, &root, current_sha);

        let surviving: Vec<_> = std::fs::read_dir(&project_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| looks_like_sha(n) && n != current_sha)
            .collect();
        assert!(
            surviving.len() <= SHA_INDEX_MAX_KEPT,
            "cap must still apply with empty current_sha: {} survivors > {}",
            surviving.len(),
            SHA_INDEX_MAX_KEPT
        );
    }

    // Pre-warm is the session-subscribe hook that kicks the cold build off the
    // query path. It must (a) return true and schedule a build for a git dir
    // with no index, (b) not build again once the index exists, and (c) swallow
    // failures for non-git/empty dirs without panicking.
    #[test]
    fn prewarm_schedules_build_then_noops_when_warm() {
        let (_home, _home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let mut f = std::fs::File::create(main.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);

        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&main)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        // No index yet: pre-warm must schedule a background build.
        let edge = resolve_compass_cache(&main);
        assert!(!edge.graph_path.is_file(), "fixture should start cold");
        assert!(
            prewarm_compass_index(&main),
            "cold git working dir must schedule a pre-warm build"
        );

        // Wait (bounded) for the background build to finish and produce an index.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !edge.graph_path.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            edge.graph_path.is_file(),
            "pre-warm background build must produce a graph.json"
        );

        // Now warm: a second pre-warm must not schedule a redundant build.
        assert!(
            !prewarm_compass_index(&main),
            "pre-warm must no-op once an index exists"
        );
    }

    // A non-git working dir must not spawn a pre-warm build (nothing authoritative
    // to index, and resolve_compass_cache falls back to a local dir). It should
    // return false quietly — the pre-warm path must never panic or disturb bind.
    #[test]
    fn prewarm_noops_for_non_git_dir() {
        let (_home, _home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let mut f = std::fs::File::create(scratch.join("notes.txt")).unwrap();
        writeln!(f, "not really source code").unwrap();
        drop(f);
        assert!(
            !prewarm_compass_index(&scratch),
            "non-git dir must not schedule a pre-warm build"
        );
    }

    // When a session-subscribe pre-warm is still building a project's index,
    // a `compass_query` must NOT join that build (which would block the turn
    // on the shared per-project build lock). Instead it returns a retryable
    // "building in background" message directing the agent to agentgrep.
    #[tokio::test]
    async fn execute_fails_fast_while_prewarm_in_flight() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let mut f = std::fs::File::create(root.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);
        let edge = resolve_compass_cache(&root);
        assert!(!edge.graph_path.is_file(), "fixture should start cold");

        // Simulate a background pre-warm still in flight for this project.
        lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())))
            .insert(edge.output_dir.clone(), std::sync::Arc::new(tokio::sync::Notify::new()));

        let ctx = ToolContext {
            session_id: "s".into(),
            message_id: "m".into(),
            tool_call_id: "t".into(),
            working_dir: Some(root.clone()),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        };
        let out = CompassQueryTool::new()
            .execute(serde_json::json!({ "query": "authentication" }), ctx)
            .await
            .expect("execute");
        assert!(
            out.output.contains("being built for this workspace in the background"),
            "must report the index is still building, got: {}",
            out.output
        );
        assert!(
            out.output.contains("agentgrep"),
            "must suggest agentgrep as a fallback for a keyword search, got: {}",
            out.output
        );

        // A STRUCTURAL intent (e.g. callers) cannot be served by agentgrep, so
        // the fail-fast must point the agent at retrying compass_query rather
        // than at a grep that cannot produce structural results.
        let ctx2 = ToolContext {
            session_id: "s".into(),
            message_id: "m".into(),
            tool_call_id: "t2".into(),
            working_dir: Some(root.clone()),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        };
        let out2 = CompassQueryTool::new()
            .execute(
                serde_json::json!({ "query": "callers of authenticate", "mode": "callers" }),
                ctx2,
            )
            .await
            .expect("execute structural");
        assert!(
            out2.output.contains("being built for this workspace in the background"),
            "structural query during pre-warm must still fail fast, got: {}",
            out2.output
        );
        assert!(
            out2.output.contains("structural"),
            "must say grep cannot fully substitute for a structural query, got: {}",
            out2.output
        );
        assert!(
            out2.output.contains("Retry `compass_query`"),
            "must point the agent at retrying compass_query for a structural query, got: {}",
            out2.output
        );

        // Clean up the in-flight marker so other tests are unaffected.
        lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())))
            .remove(&edge.output_dir);
    }

    // A second pre-warm call while a build is already in flight for the same
    // project must NOT spawn a duplicate build. It returns `true` (a build is,
    // or will be, happening) without inserting a second marker. This guards
    // against multiple sessions / reconnect storming a cold project. Needs a
    // real git dir (like prewarm_schedules) so the function passes its git
    // gate before reaching the dedup branch.
    #[test]
    fn prewarm_dedups_concurrent_in_flight_builds() {
        let (_home, _home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let mut f = std::fs::File::create(main.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);

        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&main)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        let edge = resolve_compass_cache(&main);
        assert!(!edge.graph_path.is_file(), "fixture should start cold");

        // Simulate a build already in flight for this project.
        lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())))
            .insert(edge.output_dir.clone(), std::sync::Arc::new(tokio::sync::Notify::new()));
        // Cleanup guard to avoid leaking the marker for the rest of the suite.
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())))
                    .remove(&self.0);
            }
        }
        let _cleanup = Cleanup(edge.output_dir.clone());

        // The caller must see "already building" (true) and NOT schedule a second.
        assert!(
            prewarm_compass_index(&main),
            "pre-warm must report in-flight (true) without spawning a duplicate"
        );
        // Still exactly one marker present (no duplicate insert).
        let map = lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())));
        assert!(map.get(&edge.output_dir).is_some(), "one in-flight marker");
    }

    // Real-concurrency version of the dedup guarantee: N threads call
    // `prewarm_compass_index` on the same cold git dir at once. The internal
    // mutex must make check-and-insert atomic, so exactly ONE marker lands and
    // every caller sees `true` (a build is, or will be, happening). This
    // guards the actual swarm/reconnect storm path rather than a pre-inserted
    // marker.
    #[test]
    fn prewarm_concurrent_calls_land_one_marker() {
        let (_home, _home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let mut f = std::fs::File::create(main.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);

        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&main)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        let edge = resolve_compass_cache(&main);
        assert!(!edge.graph_path.is_file(), "fixture should start cold");

        // Spawn several threads that all try to pre-warm the same project.
        let results: Vec<bool> = std::thread::scope(|s| {
            let mut handles = Vec::new();
            for _ in 0..4 {
                let main = main.clone();
                handles.push(s.spawn(move || prewarm_compass_index(&main)));
            }
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or(false))
                .collect()
        });

        // Every caller must observe a scheduled (or already-scheduled) build.
        assert!(
            results.iter().all(|&r| r),
            "all concurrent pre-warm calls must return true, got {results:?}"
        );
        // Exactly one in-flight marker survives the race (check + insert is
        // atomic under the process-global mutex).
        let map = lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())));
        assert!(
            map.get(&edge.output_dir).is_some(),
            "exactly one in-flight marker must exist after the race"
        );
        // Clean up the marker; there is no real build backing it in this test.
        drop(map);
        lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())))
            .remove(&edge.output_dir);
    }

    // After a failed pre-warm build, `prewarm_compass_index` must back off for
    // `PREWARM_FAIL_COOLDOWN` instead of re-spawning a full multi-minute build
    // on every subscribe for a project Compass cannot index. We seed
    // `PREWARM_LAST_FAILED` with a just-now failure and assert the next call
    // returns false (no new build). The query path's on-demand build still
    // surfaces failures; pre-warm just stops amplifying them.
    #[test]
    fn prewarm_backs_off_after_recent_failure() {
        let (_home, _home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let mut f = std::fs::File::create(main.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);

        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&main)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        let edge = resolve_compass_cache(&main);
        assert!(!edge.graph_path.is_file(), "fixture should start cold");

        // Seed a "just failed" marker. Clean it up afterwards so other tests
        // are unaffected. The cooldown is keyed by the project (ast_cache_root),
        // not the per-SHA output_dir.
        lock_cached(PREWARM_LAST_FAILED.get_or_init(|| Mutex::new(HashMap::new())))
            .insert(edge.ast_cache_root.clone(), SystemTime::now());
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                lock_cached(PREWARM_LAST_FAILED.get_or_init(|| Mutex::new(HashMap::new())))
                    .remove(&self.0);
            }
        }
        let _cleanup = Cleanup(edge.ast_cache_root.clone());

        // Within the cooldown window, pre-warm must not re-spawn a build.
        assert!(
            !prewarm_compass_index(&main),
            "pre-warm must back off within the failure cooldown"
        );
        // And it must not have inserted an in-flight marker (no build started).
        let map = lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())));
        assert!(
            !map.contains_key(&edge.output_dir),
            "no build may start while backing off after a recent failure"
        );
    }

    // The failure cooldown must be keyed by *project*, not by the per-SHA
    // output_dir. Make an uncommitted second commit so the current SHA (and the
    // per-SHA output_dir) differs, then prove a failure recorded under the first
    // SHA still blocks a pre-warm for the second — the whole point of backing off
    // an unindexable project across a branch/commit switch. The project key is
    // `ast_cache_root`, which is stable across SHAs.
    #[test]
    fn prewarm_cooldown_is_keyed_by_project_across_sha_change() {
        let (_home, _home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let mut f = std::fs::File::create(main.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);

        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&main)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["add", "."]);
        if !git(&["commit", "-qm", "init"]) {
            return;
        }

        // First SHA, cache resolved under it.
        let edge_sha1 = resolve_compass_cache(&main);
        assert!(!edge_sha1.graph_path.is_file(), "fixture should start cold");

        // Move to a second commit; the per-SHA output_dir changes but the
        // project (ast_cache_root) must not. Sleep past `GIT_SHA_CACHE_TTL` so
        // `current_git_sha_cached` re-reads HEAD (both resolves share the 2s
        // SHA cache and would otherwise return the first commit for both).
        let mut f2 = std::fs::File::create(main.join("lib.rs")).unwrap();
        writeln!(f2, "fn helper() {{}}").unwrap();
        drop(f2);
        git(&["add", "."]);
        if !git(&["commit", "-qm", "second"]) {
            return;
        }
        std::thread::sleep(Duration::from_millis(2100));
        let edge_sha2 = resolve_compass_cache(&main);
        assert_ne!(
            edge_sha1.output_dir, edge_sha2.output_dir,
            "per-SHA output dirs must differ across commits"
        );
        assert_eq!(
            edge_sha1.ast_cache_root, edge_sha2.ast_cache_root,
            "project cache root must be stable across commits"
        );

        // Seed a recent failure under the (project-keyed) cooldown, clean up later.
        lock_cached(PREWARM_LAST_FAILED.get_or_init(|| Mutex::new(HashMap::new())))
            .insert(edge_sha1.ast_cache_root.clone(), SystemTime::now());
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                lock_cached(PREWARM_LAST_FAILED.get_or_init(|| Mutex::new(HashMap::new())))
                    .remove(&self.0);
            }
        }
        let _cleanup = Cleanup(edge_sha1.ast_cache_root.clone());

        // Even though the HEAD/output_dir changed, a pre-warm for the second SHA
        // must back off because the *project* recently failed.
        assert!(
            !prewarm_compass_index(&main),
            "cooldown must persist across a SHA change (project keyed)"
        );
        let map = lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())));
        assert!(
            !map.contains_key(&edge_sha2.output_dir),
            "no build may start for the new SHA while the project is in cooldown"
        );
    }

    // The cooldown must EXPIRE: after `PREWARM_FAIL_COOLDOWN` elapses since the
    // last failure, `prewarm_compass_index` must be willing to retry (a new
    // build starts) instead of backing off forever. Seeds a failure older than
    // the cooldown and asserts a build begins (an in-flight marker appears).
    #[test]
    fn prewarm_retries_after_cooldown_expiry() {
        let (_home, _home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let mut f = std::fs::File::create(main.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);

        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&main)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        let edge = resolve_compass_cache(&main);
        assert!(!edge.graph_path.is_file(), "fixture should start cold");

        // Seed a failure from just before the cooldown window (older than
        // PREWARM_FAIL_COOLDOWN), so expiry must have already happened. The
        // cooldown is keyed by the project (ast_cache_root).
        let expired = SystemTime::now()
            .checked_sub(PREWARM_FAIL_COOLDOWN + Duration::from_secs(1))
            .expect("cooldown overflow");
        lock_cached(PREWARM_LAST_FAILED.get_or_init(|| Mutex::new(HashMap::new())))
            .insert(edge.ast_cache_root.clone(), expired);
        struct Cleanup {
            proj: PathBuf,
            out: PathBuf,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                // Failure map is keyed by project; in-flight marker by output_dir.
                lock_cached(PREWARM_LAST_FAILED.get_or_init(|| Mutex::new(HashMap::new())))
                    .remove(&self.proj);
                lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())))
                    .remove(&self.out);
            }
        }
        let _cleanup = Cleanup {
            proj: edge.ast_cache_root.clone(),
            out: edge.output_dir.clone(),
        };

        // After expiry, pre-warm must proceed and start a build.
        assert!(
            prewarm_compass_index(&main),
            "pre-warm must retry once the failure cooldown has expired"
        );
        let map = lock_cached(PREWARM_IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new())));
        assert!(
            map.contains_key(&edge.output_dir),
            "a build must start after the cooldown expires"
        );
    }

    // END-TO-END: a real background pre-warm must produce an index that a
    // subsequent `execute` query can actually serve (real results, not a stuck
    // "building" fail-fast). This is the composition the feature promises: pre-
    // warm off the query path, then the query hits a warm index for real.
    #[tokio::test]
    async fn prewarm_then_query_succeeds() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        let mut f = std::fs::File::create(root.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        let edge = resolve_compass_cache(&root);
        assert!(!edge.graph_path.is_file(), "fixture should start cold");

        // Trigger a real background pre-warm.
        assert!(prewarm_compass_index(&root), "pre-warm must schedule");

        // Wait (bounded) for the pre-warm build to produce graph.json.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !edge.graph_path.is_file() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(edge.graph_path.is_file(), "pre-warm must finish the index");

        // A subsequent query must be served from the warm index and not be the
        // fail-fast "still building" message.
        let ctx = ToolContext {
            session_id: "s".into(),
            message_id: "m".into(),
            tool_call_id: "t".into(),
            working_dir: Some(root),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        };
        let out = CompassQueryTool::new()
            .execute(serde_json::json!({ "query": "authentication" }), ctx)
            .await
            .expect("execute");
        assert!(
            !out.output.contains("being built for this workspace in the background"),
            "query after completed pre-warm must not fail fast, got: {}",
            out.output
        );
        assert!(
            out.output.contains("Compass query"),
            "query after completed pre-warm must return a real report, got: {}",
            out.output
        );
    }

    // A query racing a concurrent pre-warm must never corrupt or error: it is
    // served either the retryable "building in background" fail-fast or real
    // results once the pre-warm finishes. This is the live embodiment of the
    // concurrency-safe contract (fail-fast intercepts, and any query that does
    // reach the build is serialized by the shared project flock). No timing
    // assumption — we only assert the outcome is one of the two valid ones.
    #[tokio::test]
    async fn query_racing_prewarm_is_safe() {
        let (_home, root) = HomeGuard::set();
        let root = root.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return;
        }
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        let mut f = std::fs::File::create(root.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        let edge = resolve_compass_cache(&root);
        assert!(!edge.graph_path.is_file(), "fixture should start cold");

        // Fire a real background pre-warm, then immediately run a query while
        // it may still be building.
        assert!(prewarm_compass_index(&root), "pre-warm must schedule");
        let ctx = ToolContext {
            session_id: "s".into(),
            message_id: "m".into(),
            tool_call_id: "t".into(),
            working_dir: Some(root),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        };
        let out = CompassQueryTool::new()
            .execute(serde_json::json!({ "query": "authentication" }), ctx)
            .await
            .expect("execute");
        // Either "still building" (pre-warm in flight) or a real report (pre-
        // warm already finished) — never a corruption/crash.
        let building = out.output.contains("being built for this workspace in the background");
        let report = out.output.contains("Compass query");
        assert!(
            building || report,
            "query racing pre-warm must fail-fast OR return a report, got: {}",
            out.output
        );
    }

    // END-USER ACCEPTANCE PATH: exercise the real CompassQueryTool::execute
    // against actual linked git worktrees. Two worktrees of the same repo must
    // (a) resolve to the SAME per-SHA output dir and AST cache root (from the
    // git common dir), and (b) running the real tool in each worktree produces
    // real results with only ONE on-disk index, proving the second worktree
    // reused the first's shared index rather than building its own.
    async fn run_execute(working_dir: &std::path::Path) -> bool {
        let ctx = ToolContext {
            session_id: "s".into(),
            message_id: "m".into(),
            tool_call_id: "t".into(),
            working_dir: Some(working_dir.to_path_buf()),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        };
        match CompassQueryTool::new()
            .execute(serde_json::json!({ "query": "authentication" }), ctx)
            .await
        {
            Ok(out) => {
                out.output.contains("**Found ") && out.output.contains(" result(s)**")
            }
            Err(_) => false,
        }
    }

    #[tokio::test]
    async fn linked_worktrees_share_one_index_end_to_end() {
        let (_home, _home_path) = HomeGuard::set();
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();

        let git = |args: &[&str], cwd: &std::path::Path| -> bool {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"], &main) {
            return;
        }
        git(&["config", "user.email", "test@example.com"], &main);
        git(&["config", "user.name", "Test"], &main);
        std::fs::write(main.join("main.rs"), "fn a() {}\n").unwrap();
        git(&["add", "."], &main);
        if !git(&["commit", "-qm", "init"], &main) {
            return;
        }
        git(&["branch", "shared"], &main);
        if !std::process::Command::new("git")
            .args(["worktree", "add", "-q", wt.to_str().unwrap(), "shared"])
            .current_dir(&main)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return;
        }

        let main_cache = resolve_compass_cache(&main);
        let wt_cache = resolve_compass_cache(&wt);
        assert!(main_cache.is_shared && wt_cache.is_shared);
        assert_eq!(main_cache.output_dir, wt_cache.output_dir);
        assert_eq!(main_cache.ast_cache_root, wt_cache.ast_cache_root);

        // Run the real tool in BOTH worktrees; each must return indexed results.
        assert!(run_execute(&main).await, "main worktree must return results");
        // The second run (in the linked worktree) must reuse the shared index.
        let shared_graph = main_cache.output_dir.join("compass-out/graph.json");
        assert!(shared_graph.exists(), "shared index must exist after first execute");
        assert!(
            run_execute(&wt).await,
            "linked worktree must also return results via the shared index"
        );

        // Exactly one index must exist for both worktrees (the sharing guarantee).
        let mut index_count = 0usize;
        if let Ok(entries) = std::fs::read_dir(&main_cache.output_dir) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy() == "compass-out" {
                    index_count += 1;
                }
            }
        }
        assert_eq!(index_count, 1, "one shared index, not one per worktree");
    }
}
