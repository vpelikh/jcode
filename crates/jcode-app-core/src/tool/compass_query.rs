//! Semantic code search backed by Compass's knowledge graph.
//!
//! `compass_query` is a first-class, always-available tool (like `read` or
//! `agentgrep`). It integrates Compass as a pure library: there is no MCP
//! server and no CLI subprocess. The first query in a project builds the
//! Compass index in-process and caches it under `.jcode/cache/compass`; every
//! later query reuses that cache, so the (relatively expensive) build runs at
//! most once per project. Re-running a query after deleting the cache dir
//! forces a rebuild.
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use compass_core::{build_graph_with_layers, BuildOptions, BuildPurpose};
use compass_model::query_contract::{CodeQueryLimits, SearchRequest};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;

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

        // Open the Compass query engine. If no index exists yet, build one
        // in-process using Compass's library API (compass_core::build_graph_with_layers),
        // then reopen. The index is written into cache_dir/compass-out/graph.json.
        // The cold build is serialized with an exclusive flock so that parallel
        // calls (this tool is concurrency-safe) cannot race on the same index.
        let graph_path = cache_dir.join("compass-out").join("graph.json");
        let engine = match compass_query::open(&graph_path, None, &cache_dir) {
            Ok(engine) => engine,
            Err(open_err) => {
                // Cold cache. The in-process index build can take seconds for a
                // large project, so it must not run on the async executor. Offload
                // the whole probe+build+reopen to a blocking thread; the flock is
                // process-wide so it still serializes concurrent builds correctly.
                let result: anyhow::Result<compass_query::CodeQueryEngine> =
                    tokio::task::spawn_blocking({
                        let graph_path = graph_path.clone();
                        let cache_dir = cache_dir.clone();
                        let working_dir = working_dir.clone();
                        move || {
                            with_build_lock(&cache_dir, || {
                                // Another call may have finished the build while we
                                // waited for the lock; reuse it instead of rebuilding.
                                if let Ok(engine) =
                                    compass_query::open(&graph_path, None, &cache_dir)
                                {
                                    return Ok(engine);
                                }
                                build_compass_index(&working_dir, &cache_dir)?;
                                compass_query::open(&graph_path, None, &cache_dir).map_err(|e| {
                                    anyhow!("Index was built but could not be opened: {}", e)
                                })
                            })
                        }
                    })
                    .await
                    .expect("compass index build task panicked");
                match result {
                    Ok(engine) => engine,
                    Err(build_err) => {
                        return Ok(ToolOutput::new(format_index_unavailable(
                            &open_err.to_string(),
                            &build_err.to_string(),
                        )));
                    }
                }
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

/// Build a Compass knowledge-graph index for the project in-process, using the
/// Compass library API (`compass_core::build_graph_with_layers`). The resulting
/// store is written into `output_dir` so the project tree stays untouched.
///
/// This runs once per project: the output is cached under `output_dir` and the
/// caller re-opens it via `compass_query::open`, so subsequent queries skip the
/// (relatively expensive) build entirely.
fn build_compass_index(
    root: &std::path::Path,
    output_dir: &std::path::Path,
) -> Result<(), anyhow::Error> {
    let mut options = BuildOptions::new(root);
    options.output_root = Some(output_dir.to_path_buf());
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
            max_nodes: limit as u32,
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
}
