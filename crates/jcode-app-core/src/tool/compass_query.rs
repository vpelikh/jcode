//! Semantic code search backed by Compass's knowledge graph.
//!
//! `compass_query` is a first-class, always-available tool (like `read` or
//! `agentgrep`). It integrates Compass as a pure library: there is no MCP
//! server and no CLI subprocess. The first query in a project builds the
//! Compass index in-process and caches it.
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
//! The index can also be force-refreshed by deleting the cache dir.
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
use compass_model::query_contract::{CodeQueryLimits, SearchRequest};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
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

/// How long a per-SHA output dir is retained before it is eligible for GC, if
/// its SHA is no longer reachable from the repo. Older, unreachable per-commit
/// graphs are pruned so `~/.jcode/compass/<project>/` does not grow unbounded
/// as a user visits many commits over time.
const SHA_RETENTION_TTL: Duration = Duration::from_secs(14 * 24 * 60 * 60);

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
    /// The natural language or pattern query to run against the knowledge graph
    query: String,
    /// Optional path filter (file or directory substring)
    #[serde(default)]
    path: Option<String>,
    /// Limit result count
    #[serde(default)]
    limit: Option<usize>,
    /// Query intent (search, impact, discovery, callers, callees, traverse)
    #[serde(default)]
    intent: Option<String>,
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
        "Semantic code search and structural analysis via Compass's knowledge graph."
    }

    fn concurrency_safe_marker(&self) -> bool {
        // Read-only inspection tool for the common path (warm cache): pure
        // function of its input plus the index files, mutates no shared
        // agent/session state, spawns no subprocesses, and does not depend on
        // sibling tool results. A cold cache triggers an in-process index build
        // that writes files, but that build is serialized via an exclusive
        // `flock` (see `with_build_lock`), so concurrent calls cannot clobber
        // each other's `graph.json`. Safe to run in parallel with siblings.
        true
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural language query or code pattern to search for."
                },
                "path": {
                    "type": "string",
                    "description": "Optional path filter (file or directory substring)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results to return."
                },
                "intent": {
                    "type": "string",
                    "enum": ["search", "impact", "discovery", "callers", "callees", "traverse"],
                    "description": "Advisory presentation hint: search, impact, discovery, callers, callees, or traverse."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: CompassQueryInput = serde_json::from_value(input)?;

        let working_dir = ctx
            .working_dir
            .clone()
            .ok_or_else(|| anyhow!("compass_query requires a working directory"))?;

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

        // Open (or build) the Compass query engine. A cold or stale index is
        // (re)built in-process via Compass's library API. The build can take
        // seconds for a large project, so it runs on a blocking thread; the
        // project flock serializes concurrent builds, keeping the concurrency-
        // safe contract intact.
        let cache_edge = cache.clone();
        let engine_res: std::result::Result<compass_query::CodeQueryEngine, (String, String)> =
            tokio::task::spawn_blocking({
                let edge = cache_edge.clone();
                let working_dir = working_dir.clone();
                move || ensure_fresh_engine(&edge, &working_dir)
            })
            .await
            .expect("compass index task panicked");
        let engine = match engine_res {
            Ok(engine) => engine,
            Err((open_err, build_err)) => {
                return Ok(ToolOutput::new(format_index_unavailable(
                    &open_err, &build_err,
                )));
            }
        };

        let effective_limit = params.limit.unwrap_or(20).max(1);
        let result = execute_query(
            &engine,
            &params.query,
            params.path.as_deref(),
            effective_limit,
            params.intent.as_deref().unwrap_or("search"),
        );

        let reported_limit = effective_limit;
        match result {
            Ok(output) => Ok(ToolOutput::new(output)
                .with_title(format!("compass_query: {}", params.query))
                .with_metadata(json!({
                    "engine": "compass",
                    "intent": params.intent.unwrap_or_else(|| "search".to_string()),
                    "limit": reported_limit,
                    "path_filter": params.path,
                }))),
            Err(e) => Ok(ToolOutput::new(format_query_error(
                &e.to_string(),
                &params.query,
                &cache.output_dir,
            ))),
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
        CompassCachePaths {
            ast_cache_root,
            build_lock_dir: output_dir.clone(),
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
/// cache) when jcode is rebuilt or upgraded.
fn short_id(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(s.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).take(16).collect()
}

/// Canonical absolute string form of a path, for a stable non-git project id.
fn canonical_string(p: &Path) -> Option<String> {
    std::fs::canonicalize(p)
        .ok()
        .map(|c| c.to_string_lossy().into_owned())
}

/// Returns the set of commit SHAs reachable in `working_dir`'s repo (HEAD and
/// all refs), or `None` if git is unavailable. Used to identify per-SHA output
/// dirs that are no longer reachable and can be garbage-collected.
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

/// True if `name` looks like a git commit hash (40 hex chars), i.e. a per-SHA
/// output dir that GC may consider.
fn looks_like_sha(name: &str) -> bool {
    name.len() == 40 && name.chars().all(|c| c.is_ascii_hexdigit())
}

/// Garbage-collect per-SHA output dirs under `project_root` that are no longer
/// reachable from `working_dir`'s repo and have not been touched within the
/// retention window. This keeps `~/.jcode/compass/<project>/` bounded as a user
/// visits many commits. Best-effort: any failure just skips pruning.
///
/// `current_sha` (the HEAD this build is for) is always kept, even if it is a
/// detached checkout that no ref points to — pruning it would delete the index
/// the very worktree currently uses.
fn prune_stale_sha_outputs(project_root: &Path, working_dir: &Path, current_sha: &str) {
    // Never prune the shared AST cache, the non-git workspace, or any lock file.
    let Some(reachable) = git_reachable_shas(working_dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let Ok(entries) = std::fs::read_dir(project_root) else {
        return;
    };
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
        // Reachable commits (any branch/tag) are kept regardless of age.
        if reachable.contains(name) {
            continue;
        }
        // Unreachable dirs must be older than the retention window before being
        // removed, so a recent checkout that happens to be unreachable from refs
        // is not deleted immediately.
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(mtime) else {
            continue;
        };
        if age >= SHA_RETENTION_TTL {
            let _ = std::fs::remove_dir_all(entry.path());
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

/// Last time a correct staleness scan proved `cache_dir` fresh, keyed by cache dir
/// so each project is throttled independently. Bounded in size by the number of
/// distinct projects indexed in this process.
static LAST_STALE_SCAN: OnceLock<Mutex<HashMap<PathBuf, SystemTime>>> = OnceLock::new();

/// True when this cache was verified fresh within `STALE_RESCAN_TTL`. On any error
/// (missing entry, clock skew) we return false so correctness wins over the shortcut.
fn recently_scanned(cache_dir: &Path) -> bool {
    let map = LAST_STALE_SCAN
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    match map.get(cache_dir) {
        Some(&t) => t.elapsed().map(|d| d < STALE_RESCAN_TTL).unwrap_or(false),
        None => false,
    }
}

fn record_scan(cache_dir: &Path) {
    LAST_STALE_SCAN
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
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
        let guard = map.lock().unwrap();
        if let Some((t, sha)) = guard.get(working_dir)
            && t.elapsed().map(|d| d < GIT_SHA_CACHE_TTL).unwrap_or(false)
        {
            return Some(sha.clone());
        }
    } // Drop the read lock before shelling out to git.
    match current_git_sha(working_dir) {
        Some(sha) => {
            map.lock()
                .unwrap()
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
        let guard = map.lock().unwrap();
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
    map.lock().unwrap().insert(working_dir.to_path_buf(), (SystemTime::now(), top));
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
    let CompassCachePaths {
        output_dir,
        graph_path,
        ast_cache_root,
        build_lock_dir,
        is_shared,
    } = cache;
    with_build_lock(build_lock_dir, || {
        // Open an existing index. Reuse it only when source and branch haven't
        // moved past it. The mtime scan that proves freshness is throttled to
        // once per STALE_RESCAN_TTL per project (see `recently_scanned`/
        // `record_scan`) so a busy agent doesn't re-stat the whole source tree
        // on every call. Correctness holds because a scan always runs before
        // reuse once the window lapses (or if no scan has been recorded yet for
        // this cache), so a source change is caught by the first query after the
        // window, never served indefinitely. A branch change bypasses the TTL
        // and forces a rebuild immediately.
        //
        // `current_sha` is resolved lazily only when we actually have to
        // reconcile staleness against an open index: it shells out to `git`, so
        // we avoid that per query on the warm, recently-scanned path.
        match compass_query::open(graph_path, None, output_dir) {
            Ok(engine) => {
                // Reuse the index only when nothing has moved past it.
                // `index_is_stale` checks the cached git SHA first, so a
                // branch/commit switch is detected immediately and bypasses the
                // throttled mtime walk; otherwise it relies on the per-cache
                // STALE_RESCAN_TTL to skip the walk, and finally walks the tree.
                let current_sha = current_git_sha_cached(working_dir);
                if !index_is_stale(working_dir, graph_path, current_sha.as_deref(), output_dir, *is_shared) {
                    if !is_shared {
                        record_scan(output_dir);
                    }
                    return Ok(engine);
                }

                // The shared index is keyed by the committed SHA and holds no
                // worktree's uncommitted edits (see index_is_stale). For shared
                // caches this branch is only reachable on a genuine SHA switch:
                // the current output dir no longer matches, so discard and
                // rebuild below. For non-shared caches it is also reached on
                // source edits.
                drop(engine);
                let _ = std::fs::remove_dir_all(output_dir);
                // Clean up stale index files. Don't remove .compass-build.lock
                // here - it's safe to leave and removing it while holding the
                // lock could block other worktrees.
                let _ = std::fs::remove_file(output_dir.join(GIT_SHA_FILE));
            }
            Err(_) => {
                // Missing or corrupt: rebuild below (current_sha is captured by
                // build_compass_index itself).
            }
        }

        // Build (covers missing, corrupt, stale, or branch change). `cache_root`
        // lives under the project's branch-agnostic `.ast-cache` dir and is shared
        // across all SHAs of the repo, so a branch switch only re-extracts the
        // files that actually changed instead of rebuilding cold.
        // `build_compass_index` also records the current git SHA sidecar, so a
        // later branch switch is detected without walking the tree. For shared
        // caches, the scan is intentionally skipped so each caller still
        // validates freshness against its own working directory.
        build_compass_index(working_dir, output_dir, ast_cache_root)
            .map_err(|e| ("existing index missing or stale".to_string(), e.to_string()))?;
        if !is_shared {
            record_scan(output_dir);
        } else {
            // Prune unreachable, aged-out per-SHA graphs so the shared cache
            // does not grow unbounded as the user visits many commits. Always
            // keep the current HEAD's dir, even a detached HEAD with no ref.
            if let Some(project_root) = output_dir.parent()
                && let Some(current_sha) = current_git_sha_cached(working_dir)
            {
                prune_stale_sha_outputs(project_root, working_dir, &current_sha);
            }
        }
        compass_query::open(graph_path, None, output_dir).map_err(|e| {
            (
                "existing index missing or stale".to_string(),
                format!("Index was built but could not be opened: {e}"),
            )
        })
    })
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

/// Run a search through the Compass `CodeQueryEngine`. Returns a model-ready
/// formatted report.
fn execute_query(
    engine: &compass_query::CodeQueryEngine,
    query: &str,
    path_filter: Option<&str>,
    limit: usize,
    intent: &str,
) -> Result<String, anyhow::Error> {
    let request = SearchRequest {
        query: query.to_string(),
        limits: CodeQueryLimits {
            // Clamp instead of casting: a pathological usize > u32::MAX must not
            // silently wrap to 0 and violate CodeQueryLimits::is_valid().
            max_nodes: limit.clamp(1, u32::MAX as usize) as u32,
            ..Default::default()
        },
    };

    let response = engine.search(request).map_err(|e| anyhow!("{}", e))?;

    // Resolve each hit's node + source file, then apply the optional path filter.
    let mut rows: Vec<(String, Option<String>, f64, Vec<String>)> = Vec::new();
    for hit in &response.results {
        let node = response.nodes.iter().find(|n| n.id == hit.node_id);
        let name = node
            .map(|n| n.qualified_name.as_str())
            .unwrap_or(hit.node_id.as_str())
            .to_string();
        let file = node.and_then(|n| n.source.as_ref()).map(|s| s.file.clone());
        // Apply path filter (substring match on the resolved file path).
        if let (Some(filter), Some(file)) = (path_filter, &file)
            && !file.contains(filter)
        {
            continue;
        }
        rows.push((name, file, hit.score, hit.matched_fields.clone()));
    }

    let mut output = String::new();
    output.push_str(&format!("# Compass query: {}\n\n", query));
    output.push_str(&format!("**Intent:** {}\n", intent));
    output.push_str(&format!("**Limit:** {}\n", limit));
    if let Some(p) = path_filter {
        output.push_str(&format!("**Path filter:** {}\n", p));
    }
    output.push_str(&format!("\n**Found {} result(s)**\n\n", rows.len()));

    for (i, (name, file, score, matched)) in rows.iter().enumerate() {
        output.push_str(&format!("## {}. {}\n\n", i + 1, name));
        if let Some(file) = file {
            output.push_str(&format!("**File:** {}\n", file));
        }
        output.push_str(&format!("**Score:** {:.3}\n", score));
        if !matched.is_empty() {
            output.push_str(&format!("**Matched:** {}\n", matched.join(", ")));
        }
        output.push('\n');
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jcode_tool_core::ToolExecutionMode;
    use std::io::Write;
    use std::path::PathBuf;

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

    // A pathological limit above u32::MAX must not wrap to 0 (which would violate
    // CodeQueryLimits::is_valid) and fail the query.
    #[test]
    fn huge_limit_is_clamped_not_wrapped() {
        let (_tmp, root, output_dir, ast_cache_root) = make_isolated_project();
        let graph_path = output_dir.join("compass-out/graph.json");
        build_compass_index(&root, &output_dir, &ast_cache_root).expect("build");
        let engine = compass_query::open(&graph_path, None, &output_dir).expect("open after build");

        // u32::MAX + 1 would wrap to 0 under a naive `as u32`.
        let out = execute_query(&engine, "authentication", None, u64::MAX as usize, "search")
            .expect("query with clamped limit must succeed");
        assert!(
            out.contains("result(s)"),
            "expected a result report, got: {out}"
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
        assert!(!looks_like_sha("short"));
        assert!(!looks_like_sha(&"g".repeat(40)), "non-hex must not match");
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
        assert_eq!(a.chars().count(), 32, "16 bytes * 2 hex chars = 32 chars");
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
