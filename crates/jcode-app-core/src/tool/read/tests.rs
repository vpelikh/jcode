use super::*;
use crate::tool::{ToolContext, ToolExecutionMode};
use serde_json::json;

fn make_ctx(working_dir: std::path::PathBuf) -> ToolContext {
    ToolContext {
        session_id: "test-session".to_string(),
        message_id: "test-message".to_string(),
        tool_call_id: "test-call".to_string(),
        working_dir: Some(working_dir),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::Direct,
    }
}

#[test]
fn normalize_read_range_supports_start_and_end_lines() {
    let params: ReadInput = serde_json::from_value(json!({
        "file_path": "src/lib.rs",
        "start_line": 10,
        "end_line": 20
    }))
    .expect("deserialize params");

    let range = normalize_read_range(&params).expect("normalize range");
    assert_eq!(
        range,
        NormalizedReadRange {
            offset: 9,
            limit: 11,
            style: ReadRangeStyle::StartEnd,
        }
    );
}

#[test]
fn normalize_read_range_supports_start_line_and_limit() {
    let params: ReadInput = serde_json::from_value(json!({
        "file_path": "src/lib.rs",
        "start_line": 10,
        "limit": 20
    }))
    .expect("deserialize params");

    let range = normalize_read_range(&params).expect("start_line + limit should work");
    assert_eq!(
        range,
        NormalizedReadRange {
            offset: 9,
            limit: 20,
            style: ReadRangeStyle::StartEnd,
        }
    );
}

#[test]
fn normalize_read_range_prefers_end_line_over_limit() {
    let params: ReadInput = serde_json::from_value(json!({
        "file_path": "src/lib.rs",
        "start_line": 10,
        "end_line": 20,
        "limit": 999
    }))
    .expect("deserialize params");

    let range = normalize_read_range(&params).expect("end_line should take precedence");
    assert_eq!(
        range,
        NormalizedReadRange {
            offset: 9,
            limit: 11,
            style: ReadRangeStyle::StartEnd,
        }
    );
}

#[test]
fn normalize_read_range_rejects_start_line_and_offset() {
    let params: ReadInput = serde_json::from_value(json!({
        "file_path": "src/lib.rs",
        "start_line": 10,
        "offset": 20
    }))
    .expect("deserialize params");

    let err = normalize_read_range(&params).expect_err("mixed range styles should fail");
    assert!(
        err.to_string().contains("Use either start_line/end_line")
            || err.to_string().contains("not both"),
        "unexpected error: {err}"
    );
}

#[test]
fn normalize_read_range_accepts_matching_start_line_and_offset() {
    let params: ReadInput = serde_json::from_value(json!({
        "file_path": "src/lib.rs",
        "start_line": 10,
        "offset": 9,
        "limit": 20
    }))
    .expect("deserialize params");

    let range = normalize_read_range(&params).expect("matching range styles should work");
    assert_eq!(
        range,
        NormalizedReadRange {
            offset: 9,
            limit: 20,
            style: ReadRangeStyle::StartEnd,
        }
    );
}

#[test]
fn normalize_read_range_accepts_end_line_with_zero_offset() {
    let params: ReadInput = serde_json::from_value(json!({
        "file_path": "src/lib.rs",
        "end_line": 20,
        "offset": 0
    }))
    .expect("deserialize params");

    let range = normalize_read_range(&params).expect("redundant zero offset should work");
    assert_eq!(
        range,
        NormalizedReadRange {
            offset: 0,
            limit: 20,
            style: ReadRangeStyle::StartEnd,
        }
    );
}

#[test]
fn normalize_read_range_rejects_invalid_end_before_start() {
    let params: ReadInput = serde_json::from_value(json!({
        "file_path": "src/lib.rs",
        "start_line": 20,
        "end_line": 10
    }))
    .expect("deserialize params");

    let err = normalize_read_range(&params).expect_err("invalid range should fail");
    assert!(
        err.to_string()
            .contains("greater than or equal to start_line"),
        "unexpected error: {err}"
    );
}

#[test]
fn read_tool_schema_avoids_openai_incompatible_combinators() {
    let schema = ReadTool::new().parameters_schema();

    assert_eq!(schema.get("type"), Some(&json!("object")));
    assert!(schema.get("allOf").is_none());
    assert!(schema.get("not").is_none());
}

#[test]
fn read_tool_schema_advertises_only_canonical_public_fields() {
    let schema = ReadTool::new().parameters_schema();
    let properties = schema["properties"]
        .as_object()
        .expect("read schema properties should be an object");

    assert!(properties.contains_key("file_path"));
    assert!(properties.contains_key("start_line"));
    assert!(properties.contains_key("limit"));
    assert!(!properties.contains_key("end_line"));
    assert!(!properties.contains_key("offset"));
}

#[test]
fn read_tool_description_advertises_supported_file_types() {
    let tool = ReadTool::new();
    let description = tool.description().to_lowercase();
    assert!(description.contains("text"), "description={description}");
    assert!(description.contains("image"), "description={description}");
    assert!(description.contains("pdf"), "description={description}");

    let schema = tool.parameters_schema();
    let file_path_description = schema["properties"]["file_path"]["description"]
        .as_str()
        .expect("file_path should have a description");
    assert_eq!(file_path_description, "Path to a file.");
}

#[tokio::test]
async fn read_tool_supports_start_line_and_end_line() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("sample.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").expect("write sample file");

    let tool = ReadTool::new();
    let output = tool
        .execute(
            json!({
                "file_path": "sample.txt",
                "start_line": 2,
                "end_line": 4
            }),
            make_ctx(temp.path().to_path_buf()),
        )
        .await
        .expect("read execution should succeed");

    assert!(
        output.output.contains("2\ttwo"),
        "output={:?}",
        output.output
    );
    assert!(
        output.output.contains("3\tthree"),
        "output={:?}",
        output.output
    );
    assert!(
        output.output.contains("4\tfour"),
        "output={:?}",
        output.output
    );
    assert!(
        !output.output.contains("1\tone"),
        "output={:?}",
        output.output
    );
    assert!(
        !output.output.contains("5\tfive"),
        "output={:?}",
        output.output
    );
}

#[tokio::test]
async fn read_tool_continuation_hint_matches_start_line_style() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("sample.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").expect("write sample file");

    let tool = ReadTool::new();
    let output = tool
        .execute(
            json!({
                "file_path": "sample.txt",
                "start_line": 2,
                "end_line": 3
            }),
            make_ctx(temp.path().to_path_buf()),
        )
        .await
        .expect("read execution should succeed");

    assert!(
        output.output.contains("use start_line=4 to continue"),
        "output={:?}",
        output.output
    );
}

#[tokio::test]
async fn read_tool_supports_start_line_with_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("sample.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").expect("write sample file");

    let tool = ReadTool::new();
    let output = tool
        .execute(
            json!({
                "file_path": "sample.txt",
                "start_line": 2,
                "limit": 2
            }),
            make_ctx(temp.path().to_path_buf()),
        )
        .await
        .expect("read execution should succeed");

    assert!(
        output.output.contains("2\ttwo"),
        "output={:?}",
        output.output
    );
    assert!(
        output.output.contains("3\tthree"),
        "output={:?}",
        output.output
    );
    assert!(
        !output.output.contains("4\tfour"),
        "output={:?}",
        output.output
    );
    assert!(
        output.output.contains("use start_line=4 to continue"),
        "output={:?}",
        output.output
    );
}

#[tokio::test]
async fn read_tool_caps_rendered_output_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("big.txt");
    // ~60k lines of moderate width will exceed the rendered char budget at
    // line numbers + content, so output must be capped while the continuation
    // hint stays exact.
    let line = "x".repeat(60);
    let content: String = std::iter::repeat_with(|| format!("{line}\n"))
        .take(60_000)
        .collect();
    std::fs::write(&path, &content).expect("write big file");

    let tool = ReadTool::new();
    let output = tool
        .execute(
            json!({ "file_path": "big.txt" }),
            make_ctx(temp.path().to_path_buf()),
        )
        .await
        .expect("read should succeed");

    // Never exceeds the hard rendered budget even for a large default read.
    assert!(
        output.output.len() <= MAX_READ_OUTPUT_CHARS + MAX_LINE_LEN + 500,
        "rendered output ({}) exceeded budget {}",
        output.output.len(),
        MAX_READ_OUTPUT_CHARS
    );
    // The beginning of the file is present.
    assert!(output.output.contains("1\t"), "missing first line: {:?}", output.output);
    // A continuation hint tells the model where to resume.
    assert!(
        output.output.contains("more lines (use offset="),
        "missing continuation hint: {:?}",
        output.output
    );
}

#[tokio::test]
async fn read_tool_budget_continuation_hint_points_at_unrendered_region() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("wide.txt");
    // Wide lines near MAX_LINE_LEN quickly blow the rendered budget, so a
    // default read stops early; the continuation offset must be the line after
    // the last rendered line, not the full requested range.
    let content: String = std::iter::repeat_with(|| format!("{}\n", "y".repeat(1800)))
        .take(2_000)
        .collect();
    std::fs::write(&path, &content).expect("write wide file");

    let tool = ReadTool::new();
    let output = tool
        .execute(
            json!({ "file_path": "wide.txt", "start_line": 1, "end_line": 2000 }),
            make_ctx(temp.path().to_path_buf()),
        )
        .await
        .expect("read should succeed");

    assert!(
        output.output.len() <= MAX_READ_OUTPUT_CHARS + MAX_LINE_LEN + 500,
        "rendered output ({}) exceeded budget {}",
        output.output.len(),
        MAX_READ_OUTPUT_CHARS
    );
    // The continuation hint must not skip the unrendered portion: it must point
    // after the last rendered line (a specific start_line), not at 2000.
    assert!(
        output.output.contains("more lines (use start_line="),
        "missing continuation hint: {:?}",
        output.output
    );
    assert!(
        !output.output.contains("more lines (use start_line=2000"),
        "hint should point at the unrendered region, not the full range: {:?}",
        output.output
    );
}

#[tokio::test]
async fn read_tool_small_bounded_read_is_unaffected_by_budget() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("small.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").expect("write file");

    let tool = ReadTool::new();
    let output = tool
        .execute(
            json!({ "file_path": "small.txt", "start_line": 1, "end_line": 3 }),
            make_ctx(temp.path().to_path_buf()),
        )
        .await
        .expect("read should succeed");

    // A small, explicitly bounded read is unchanged and contains no budget
    // truncation hint beyond the normal range continuation (none needed).
    assert!(
        output.output.len() < MAX_READ_OUTPUT_CHARS,
        "small read should stay small: {:?}",
        output.output
    );
    assert!(output.output.contains("1\tone"), "output={:?}", output.output);
    assert!(output.output.contains("3\tthree"), "output={:?}", output.output);
}

#[tokio::test]
async fn read_tool_prefers_end_line_over_limit() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("sample.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").expect("write sample file");

    let tool = ReadTool::new();
    let output = tool
        .execute(
            json!({
                "file_path": "sample.txt",
                "start_line": 2,
                "end_line": 3,
                "limit": 50
            }),
            make_ctx(temp.path().to_path_buf()),
        )
        .await
        .expect("read execution should succeed");

    assert!(
        output.output.contains("2\ttwo"),
        "output={:?}",
        output.output
    );
    assert!(
        output.output.contains("3\tthree"),
        "output={:?}",
        output.output
    );
    assert!(
        !output.output.contains("4\tfour"),
        "output={:?}",
        output.output
    );
    assert!(
        output.output.contains("use start_line=4 to continue"),
        "output={:?}",
        output.output
    );
}

#[test]
fn normalize_read_range_from_tool_input_handles_all_styles() {
    // start_line + end_line
    let json = json!({ "file_path": "f.rs", "start_line": 10, "end_line": 20 });
    let range = normalize_read_range_from_tool_input(&json);
    assert_eq!(range, (10, 20));

    // start_line + limit
    let json = json!({ "file_path": "f.rs", "start_line": 5, "limit": 10 });
    let range = normalize_read_range_from_tool_input(&json);
    assert_eq!(range, (5, 14));

    // offset + limit (originally public read parameters)
    let json = json!({ "file_path": "f.rs", "offset": 9, "limit": 20 });
    let range = normalize_read_range_from_tool_input(&json);
    assert_eq!(range, (10, 29));

    // only file_path (default full read): 1-based [1, 5000]
    let json = json!({ "file_path": "f.rs" });
    let range = normalize_read_range_from_tool_input(&json);
    assert_eq!(range, (1, DEFAULT_LIMIT));
}

#[test]
fn coverage_covers_requires_prior_range_to_fully_contain_request() {
    let requested = NormalizedReadRange {
        offset: 4, // line 5
        limit: 10, // lines 5..=14
        style: ReadRangeStyle::StartEnd,
    };

    // Prior covers lines 1..=20 -> fully contains 5..=14.
    assert!(coverage_covers((1, 20), &requested));
    // Prior ends before the request -> not covered.
    assert!(!coverage_covers((1, 10), &requested));
    // Prior starts after the request -> not covered.
    assert!(!coverage_covers((6, 20), &requested));
    // Exact match -> covered.
    assert!(coverage_covers((5, 14), &requested));
}

#[test]
fn coverage_covers_against_range_request_uses_1based_inclusive() {
    // Prior read lines [2,4]; request NormalizedReadRange offset=1 limit=3 => lines [2,4].
    let requested = NormalizedReadRange {
        offset: 1,
        limit: 3,
        style: ReadRangeStyle::StartEnd,
    };
    assert!(coverage_covers((2, 4), &requested));
    assert!(!coverage_covers((2, 3), &requested));
}

#[test]
fn file_unchanged_since_returns_true_for_older_mtime() {
    let now = std::time::SystemTime::now();
    let since = chrono::DateTime::<chrono::Utc>::from(now + std::time::Duration::from_secs(10));
    // File mtime (now) is strictly older than the read time (now + 10s) =>
    // file predates the read => unchanged since => safe to dedup.
    assert!(file_unchanged_since(now, &since));
}

#[test]
fn file_unchanged_since_returns_false_for_newer_or_equal_mtime() {
    let now = std::time::SystemTime::now();
    // File mtime equal to read time => ambiguous, do not dedup.
    let at = chrono::DateTime::<chrono::Utc>::from(now);
    assert!(!file_unchanged_since(now, &at));
    // File mtime strictly newer than read time => changed after read => no dedup.
    let earlier =
        chrono::DateTime::<chrono::Utc>::from(now - std::time::Duration::from_secs(10));
    assert!(!file_unchanged_since(now, &earlier));
}

#[test]
fn paths_equivalent_canonicalizes_relative_and_absolute() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("target.txt");
    std::fs::write(&path, "x").expect("write");

    assert!(paths_equivalent(path.to_str().unwrap(), &path));
    // Same file via a `./`-prefixed relative path under the same parent.
    let rel = path
        .parent()
        .unwrap()
        .join(format!("./{}", path.file_name().unwrap().to_string_lossy()));
    assert!(paths_equivalent(rel.to_str().unwrap(), &path));
    // A different file is not equivalent.
    let other = temp.path().join("other.txt");
    std::fs::write(&other, "y").expect("write");
    assert!(!paths_equivalent(other.to_str().unwrap(), &path));
}
