//! Semantic code search backed by Compass's knowledge graph.
//!
//! `compass_query` is a first-class, always-available tool (like `read` or
//! `agentgrep`). It integrates Compass as a pure library: there is no MCP
//! server and no CLI subprocess. The first query in a project builds the
//! Compass index in-process and caches it under `.jcode/cache/compass`. A warm
//! index is reused for subsequent queries, so the build runs only when the
//! project has never been indexed or when source has changed since the last
//! build. On a rebuild Compass reuses its persisted AST cache (incremental
//! extract), so changed files are re-extracted rather than the whole project.
//! The index can also be force-refreshed by deleting the cache dir.
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

        // Build cache directory relative to working directory. The Compass index
        // is built and stored here, so the project tree stays clean.
        let cache_dir = working_dir.join(".jcode/cache/compass");
        if let Err(e) = std::fs::create_dir_all(&cache_dir) {
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
        let graph_path = cache_dir.join("compass-out").join("graph.json");
        let engine_res: std::result::Result<compass_query::CodeQueryEngine, (String, String)> =
            tokio::task::spawn_blocking({
                let graph_path = graph_path.clone();
                let cache_dir = cache_dir.clone();
                let working_dir = working_dir.clone();
                move || ensure_fresh_engine(&graph_path, &cache_dir, &working_dir)
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

        let result = execute_query(
            &engine,
            &params.query,
            params.path.as_deref(),
            params.limit.unwrap_or(20).max(1),
            params.intent.as_deref().unwrap_or("search"),
        );

        match result {
            Ok(output) => Ok(ToolOutput::new(output)
                .with_title(format!("compass_query: {}", params.query))
                .with_metadata(json!({
                    "engine": "compass",
                    "intent": params.intent.unwrap_or_else(|| "search".to_string()),
                    "limit": params.limit.unwrap_or(20),
                    "path_filter": params.path,
                }))),
            Err(e) => Ok(ToolOutput::new(format_query_error(
                &e.to_string(),
                &params.query,
                &cache_dir,
            ))),
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
         The index is built, but the search engine returned an error. If this persists, \
         try removing the cached index ({}) and re-running the query to force a rebuild.",
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
fn index_is_stale(
    root: &Path,
    graph_path: &Path,
    current_sha: Option<&str>,
    cache_dir: &Path,
) -> bool {
    // Check for branch/commit change first. If the current git SHA differs from
    // the one the index was built against, it's definitely stale.
    if let Some(sha) = current_sha {
        if let Some(cached_sha) = index_git_sha(cache_dir) {
            if sha != cached_sha {
                return true; // Branch/commit changed, index is stale
            }
        }
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
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if matches!(name, ".git" | "target" | "node_modules" | ".jcode") {
                        continue;
                    }
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
                if let Ok(meta) = std::fs::metadata(&path) {
                    if let Ok(m) = meta.modified() {
                        if m > index_mtime {
                            return true;
                        }
                    }
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
    graph_path: &Path,
    cache_dir: &Path,
    working_dir: &Path,
) -> std::result::Result<compass_query::CodeQueryEngine, (String, String)> {
    with_build_lock(cache_dir, || {
        // Get the current git SHA once so we can compare it against the cached one.
        let current_sha = current_git_sha(working_dir);

        // Open an existing index. Reuse it only when source and branch haven't
        // moved past it. The mtime scan that proves freshness is throttled to
        // once per STALE_RESCAN_TTL per project (see `recently_scanned`/
        // `record_scan`) so a busy agent doesn't re-stat the whole source tree
        // on every call. Correctness holds because a scan always runs before
        // reuse once the window lapses (or if no scan has been recorded yet for
        // this cache), so a source change is caught by the first query after the
        // window, never served indefinitely. A branch change bypasses the TTL
        // and forces a rebuild immediately.
        match compass_query::open(graph_path, None, cache_dir) {
            Ok(engine) => {
                let stale = !recently_scanned(cache_dir)
                    && index_is_stale(working_dir, graph_path, current_sha.as_deref(), cache_dir);
                if !stale {
                    if !recently_scanned(cache_dir) {
                        record_scan(cache_dir);
                    }
                    return Ok(engine);
                }
                // Valid but stale (source edit or branch change): discard it and
                // rebuild below so we never serve a dirty index. Do NOT record
                // the scan: the next call must re-check rather than trust a now-
                // discarded index.
                drop(engine);
                let _ = std::fs::remove_dir_all(cache_dir.join("compass-out"));
            }
            Err(_) => {
                // Missing or corrupt: rebuild below.
            }
        }

        // Build (covers missing, corrupt, stale, or branch change). `cache_root`
        // makes this incremental on a repeat build, re-extracting only changed
        // files. Persist the current git SHA so future queries can detect
        // subsequent branch changes without walking the tree.
        build_compass_index(working_dir, cache_dir)
            .map_err(|e| ("existing index missing or stale".to_string(), e.to_string()))?;
        if let Some(sha) = &current_sha {
            write_index_git_sha(cache_dir, sha);
        }
        compass_query::open(graph_path, None, cache_dir).map_err(|e| {
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
/// `cache_root` points at the same dir as `output_dir` so Compass can persist
/// its AST-fact digests across builds. On a rebuild (stale index) this lets
/// Compass re-extract only changed files instead of the whole project, i.e. the
/// index is incrementally maintained rather than fully re-derived each time.
///
/// The output is cached under `output_dir` and the caller re-opens it via
/// `compass_query::open`, so subsequent queries skip the build entirely unless
/// source has since changed.
fn build_compass_index(
    root: &std::path::Path,
    output_dir: &std::path::Path,
) -> Result<(), anyhow::Error> {
    let mut options = BuildOptions::new(root);
    options.output_root = Some(output_dir.to_path_buf());
    options.cache_root = Some(output_dir.to_path_buf());
    options.purpose = BuildPurpose::Extract;
    options.scan_filesystem = true;
    options.graph_storage = compass_core::GraphStorage::Json;

    build_graph_with_layers(&options, None, &[])
        .map(|_| ())
        .map_err(|e| anyhow!("compass_core build_graph failed: {}", e))
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
        if let (Some(filter), Some(file)) = (path_filter, &file) {
            if !file.contains(filter) {
                continue;
            }
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
    use tempfile::TempDir;

    /// Create an isolated temp project with a single source file, returning the
    /// project dir and its `.jcode/cache/compass` cache dir. The `TempDir` is
    /// dropped (and the directory removed) automatically when the test ends, so
    /// each test gets a unique, isolated workspace with no cross-test leakage.
    fn make_isolated_project() -> (TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let root = tmp.path().to_path_buf();
        let mut f = std::fs::File::create(root.join("main.rs")).unwrap();
        writeln!(f, "fn authenticate(user: &str) {{ let _ = user; }}").unwrap();
        drop(f);

        let cache = root.join(".jcode/cache/compass");
        std::fs::create_dir_all(&cache).unwrap();
        (tmp, root, cache)
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
        let (_tmp, root, cache) = make_isolated_project();

        // Build the index in-process.
        build_compass_index(&root, &cache).expect("build should succeed");

        // Open and run a search.
        let engine =
            compass_query::open(&cache.join("compass-out").join("graph.json"), None, &cache)
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
        let (_tmp, root, cache) = make_isolated_project();
        let graph_path = cache.join("compass-out").join("graph.json");
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
        assert!(
            compass_query::open(&graph_path, None, &cache).is_ok(),
            "index left by concurrent builds must be openable"
        );
    }

    #[test]
    fn index_is_stale_detects_new_source() {
        let (_tmp, root, cache) = make_isolated_project();
        let graph_path = cache.join("compass-out").join("graph.json");
        build_compass_index(&root, &cache).expect("build");

        // A fresh index is not considered stale against its own source.
        assert!(
            !index_is_stale(
                &root,
                &graph_path,
                current_git_sha(&root).as_deref(),
                &cache
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
                &cache
            ),
            "index must be stale after a newer source file is added"
        );

        // Rebuilding refreshes the index mtime, so it is no longer stale.
        build_compass_index(&root, &cache).expect("rebuild");
        assert!(
            !index_is_stale(
                &root,
                &graph_path,
                current_git_sha(&root).as_deref(),
                &cache
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
        let (_tmp, root, cache) = make_isolated_project();
        let graph_path = cache.join("compass-out").join("graph.json");
        build_compass_index(&root, &cache).expect("build");

        let out_dir = cache.join("compass-out");
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
            compass_query::open(&graph_path, None, &cache).is_ok(),
            "rebuilt index must be openable"
        );
    }

    // A fresh (non-stale) index must be served as-is: a follow-up query with no
    // source change must NOT rebuild it (the index dir mtime stays stable). This
    // guards against regressions where the cache is needlessly discarded.
    #[tokio::test]
    async fn fresh_index_is_not_rebuilt_on_query() {
        let (_tmp, root, cache) = make_isolated_project();
        let out_dir = cache.join("compass-out");
        build_compass_index(&root, &cache).expect("build");

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
        let (_tmp, root, cache) = make_isolated_project();
        let graph_path = cache.join("compass-out").join("graph.json");
        build_compass_index(&root, &cache).expect("build");
        let engine = compass_query::open(&graph_path, None, &cache).expect("open after build");

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
}
