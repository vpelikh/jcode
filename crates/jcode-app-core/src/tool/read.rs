#![cfg_attr(test, allow(clippy::items_after_test_module))]

use super::{Tool, ToolContext, ToolOutput};
use crate::bus::{Bus, BusEvent, FileOp, FileTouch};
use crate::config;
use crate::message::ContentBlock;
use crate::session::Session;
use anyhow::Result;
use async_trait::async_trait;
use jcode_terminal_image::{ImageDisplayParams, ImageProtocol, display_image};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

const DEFAULT_LIMIT: usize = 5000;
const MAX_LINE_LEN: usize = 2000;

/// Hard cap on rendered characters for a single `read` call.
///
/// The default `limit` is 5000 lines, so an unbounded read of a large file can
/// emit tens of thousands of normal-width lines. That is a real token cost, and
/// past the downstream context guard's ~50k-token single-output ceiling the
/// whole result is *withheld* rather than returned, so the model loses the
/// content entirely and often re-issues a narrower call.
///
/// Capping the rendered output here (well under that ceiling) keeps the useful
/// *beginning* of the file and always appends an explicit continuation hint, so
/// a default full-file read degrades to a bounded, useful prefix rather than a
/// refusal. Explicitly bounded calls (small `limit`/`end_line`) are unaffected
/// because they almost always land under the cap.
const MAX_READ_OUTPUT_CHARS: usize = 120_000;

/// Minimum requested line count before we bother attempting read dedup.
///
/// `dedup_already_read` loads the persisted session from disk to find prior
/// reads of the same range, which is a real per-call IO cost (and now that
/// `read_dedup` is on by default, it runs on every `read`). For small reads the
/// pointer-vs-content saving is negligible, so gating them here skips the
/// session load entirely and keeps the common "read a few lines" case free of
/// that overhead. Only reads requesting at least this many lines pay for the
/// dedup lookup.
const READ_DEDUP_MIN_LINES: usize = 50;

pub struct ReadTool;

impl ReadTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct ReadInput {
    file_path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    end_line: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadRangeStyle {
    OffsetLimit,
    StartEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NormalizedReadRange {
    offset: usize,
    limit: usize,
    style: ReadRangeStyle,
}

fn normalize_read_range(params: &ReadInput) -> Result<NormalizedReadRange> {
    let has_start_end = params.start_line.is_some() || params.end_line.is_some();
    let has_mixed_offset = match (params.start_line, params.end_line, params.offset) {
        (Some(start_line), _, Some(offset)) => {
            if start_line == 0 {
                true
            } else {
                offset.checked_add(1) != Some(start_line)
            }
        }
        (None, Some(_), Some(offset)) => offset != 0,
        _ => params.offset.is_some(),
    };

    if has_start_end && has_mixed_offset {
        return Err(anyhow::anyhow!(
            "Use either start_line/end_line (1-based) or offset (0-based), not both. `limit` may be used with either style."
        ));
    }

    if has_start_end {
        let start_line = params.start_line.unwrap_or(1);
        if start_line == 0 {
            return Err(anyhow::anyhow!(
                "start_line must be 1 or greater (it is 1-based)."
            ));
        }

        let limit = if let Some(end_line) = params.end_line {
            if end_line == 0 {
                return Err(anyhow::anyhow!(
                    "end_line must be 1 or greater (it is 1-based)."
                ));
            }
            if end_line < start_line {
                return Err(anyhow::anyhow!(
                    "end_line ({}) must be greater than or equal to start_line ({}).",
                    end_line,
                    start_line
                ));
            }
            end_line - start_line + 1
        } else {
            params.limit.unwrap_or(DEFAULT_LIMIT)
        };

        return Ok(NormalizedReadRange {
            offset: start_line - 1,
            limit,
            style: ReadRangeStyle::StartEnd,
        });
    }

    Ok(NormalizedReadRange {
        offset: params.offset.unwrap_or(0),
        limit: params.limit.unwrap_or(DEFAULT_LIMIT),
        style: ReadRangeStyle::OffsetLimit,
    })
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file. Supports text files, image files, and PDFs."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["file_path"],
            "properties": {
                "intent": super::intent_schema_property(),
                "file_path": {
                    "type": "string",
                    "description": "Path to a file."
                },
                "start_line": {
                    "type": "integer",
                    "description": "1-based start line for text files."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max text lines to read. Default 5000."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: ReadInput = serde_json::from_value(input)?;
        let range = normalize_read_range(&params)?;

        let path = ctx.resolve_path(Path::new(&params.file_path));

        // Check if file exists
        if !path.exists() {
            // Try to find similar files
            let suggestions = find_similar_files(&path);
            if suggestions.is_empty() {
                return Err(anyhow::anyhow!("File not found: {}", params.file_path));
            } else {
                return Err(anyhow::anyhow!(
                    "File not found: {}\nDid you mean: {}",
                    params.file_path,
                    suggestions.join(", ")
                ));
            }
        }

        // Check for image files and display in terminal if supported
        if is_image_file(&path) {
            return handle_image_file(&path, &params.file_path);
        }

        // Check for PDF files and extract text
        if is_pdf_file(&path) {
            return handle_pdf_file(&path, &params.file_path);
        }

        // Check for binary files
        if is_binary_file(&path) {
            return Ok(ToolOutput::new(format!(
                "Binary file detected: {}\nUse appropriate tools to handle binary files.",
                params.file_path
            )));
        }

        // Dedup: if enabled, and this exact file range was already read earlier
        // in this session (still in active context, file unchanged), return a
        // compact pointer instead of re-reading the file.
        //
        // Only bother for reads of a meaningful size: attempting it loads the
        // persisted session from disk, so tiny reads (under READ_DEDUP_MIN_LINES)
        // skip that overhead entirely since the saving would be negligible.
        if config::config().tools.read_dedup
            && range.limit >= READ_DEDUP_MIN_LINES
            && let Some(pointer) = dedup_already_read(&ctx, &path, &range)
        {
            Bus::global().publish(BusEvent::FileTouch(FileTouch {
                session_id: ctx.session_id.clone(),
                path: path.to_path_buf(),
                op: FileOp::Read,
                intent: None,
                summary: Some(format!(
                    "read dedup: lines {}-{} of {} already in context",
                    range.offset + 1,
                    range.offset + range.limit,
                    Path::new(&params.file_path).display()
                )),
                detail: None,
            }));
            crate::logging::info(&format!(
                "[tool:read] dedup for {} in session {} (range={}..{})",
                params.file_path, ctx.session_id, range.offset + 1, range.offset + range.limit
            ));
            return Ok(ToolOutput::new(pointer));
        }

        // Read file
        let content = tokio::fs::read_to_string(&path).await?;

        // Single-pass: count lines while building output, bounded by the limit
        // and the max rendered character cap. We still count every line up to the
        // requested range so `total_lines` and the continuation hint stay exact.
        let mut output = String::with_capacity(range.limit.min(2000) * 80);
        let mut total_lines = 0usize;
        let mut truncated_line_count = 0usize;
        let mut budget_exhausted = false;
        let mut rendered_chars = 0usize;
        let mut last_rendered_line = range.offset;
        let end_exclusive = range.offset + range.limit;
        {
            use std::fmt::Write;
            for (i, line) in content.lines().enumerate() {
                total_lines = i + 1;
                if i < range.offset {
                    continue;
                }
                if i >= end_exclusive {
                    // Still need to count remaining lines
                    continue;
                }
                if rendered_chars >= MAX_READ_OUTPUT_CHARS {
                    // Stop rendering, but keep counting remaining lines so the
                    // continuation hint below is exact. Once the budget is
                    // exhausted it stays exhausted for the rest of the range.
                    budget_exhausted = true;
                    continue;
                }
                let line_num = i + 1;
                last_rendered_line = i;
                if line.len() > MAX_LINE_LEN {
                    truncated_line_count += 1;
                    let window = crate::util::truncate_str(line, MAX_LINE_LEN);
                    let _ = writeln!(output, "{:>5}\t{}...", line_num, window);
                    // `{:>5}` pads the number to width 5, then tab + content +
                    // the "..." suffix + newline.
                    rendered_chars += 5 + 1 + window.chars().count() + 3 + 1;
                } else {
                    let _ = writeln!(output, "{:>5}\t{}", line_num, line);
                    rendered_chars += 5 + 1 + line.chars().count() + 1;
                }
            }
        }

        let end = if budget_exhausted {
            last_rendered_line + 1
        } else {
            end_exclusive.min(total_lines)
        };

        // Publish file touch event for swarm coordination
        Bus::global().publish(BusEvent::FileTouch(FileTouch {
            session_id: ctx.session_id.clone(),
            path: path.to_path_buf(),
            op: FileOp::Read,
            intent: None,
            summary: Some(format!(
                "read lines {}-{} of {}",
                range.offset + 1,
                end,
                total_lines
            )),
            detail: None,
        }));

        if truncated_line_count > 0 || end < total_lines {
            crate::logging::warn(&format!(
                "[tool:read] returned truncated output for {} in session {} (tool_call={} range={}..{} total_lines={} truncated_lines={})",
                params.file_path,
                ctx.session_id,
                ctx.tool_call_id,
                range.offset + 1,
                end,
                total_lines,
                truncated_line_count
            ));
        }

        // Add metadata
        if end < total_lines {
            let continuation_hint = match range.style {
                // The next line after the last *rendered* line. When the output
                // budget is exhausted this is earlier than offset+limit, so it
                // tells the model exactly where to resume rather than skipping
                // the unrendered portion of the requested range.
                ReadRangeStyle::OffsetLimit => format!("offset={}", end),
                ReadRangeStyle::StartEnd => format!("start_line={}", end + 1),
            };
            output.push_str(&format!(
                "\n... {} more lines (use {} to continue)\n",
                total_lines - end,
                continuation_hint
            ));
        }

        if output.is_empty() {
            Ok(ToolOutput::new("(empty file)"))
        } else {
            Ok(ToolOutput::new(output))
        }
    }
}

#[cfg(test)]
mod tests;

/// Try to deduplicate this read against an earlier read in the same session.
///
/// Returns `Some(pointer)` when the exact requested range was already read
/// earlier in this session, that earlier result is still part of the *active*
/// (un-compacted) context, and the file has not changed since that read. The
/// returned pointer tells the model the content is already in context rather
/// than re-emitting the full text. Repeated reads of unchanged file ranges are
/// collapsed to a pointer. Fully conservative gating means we never serve stale or
/// already-summarized content.
///
/// Returns `None` (meaning "read normally") when dedup does not apply.
fn dedup_already_read(
    ctx: &ToolContext,
    path: &Path,
    range: &NormalizedReadRange,
) -> Option<String> {
    // File freshness anchor: the file must not be newer than the prior read.
    let metadata = std::fs::metadata(path).ok()?;
    let current_mtime = metadata.modified().ok()?;

    // Load the session (cheap disk read; only reached when read_dedup is on).
    let session = Session::load(&ctx.session_id).ok()?;

    // Compaction cutoff: messages before this index were summarized away, so a
    // prior read there is no longer verbatim in context.
    let compaction_cutoff = session
        .compaction
        .as_ref()
        .map(|state| state.covers_up_to_turn)
        .unwrap_or(0);

    let prior_candidates =
        collect_prior_read_candidates(&session.messages, compaction_cutoff);

    decide_dedup(path, range, current_mtime, &prior_candidates)
}

/// Collect prior `read` tool calls from session messages that are still in the
/// active (un-compacted) context, as `(file_path, prior_range, read_at)` triples.
///
/// Separated from the rest of dedup so the session-scrape -> candidate
/// conversion is unit-testable without a persisted on-disk session.
fn collect_prior_read_candidates(
    messages: &[crate::session::StoredMessage],
    compaction_cutoff: usize,
) -> Vec<(String, (usize, usize), chrono::DateTime<chrono::Utc>)> {
    let mut prior_candidates = Vec::new();
    for (message_index, msg) in messages.iter().enumerate() {
        if message_index < compaction_cutoff {
            // Compacted away; the content is no longer verbatim in context.
            continue;
        }
        for block in &msg.content {
            if let ContentBlock::ToolUse { name, input, .. } = block
                && name == "read"
            {
                let prior = range_from_tool_input(input);
                let Some(prior_file) = prior.file_path else { continue };
                let Some(ts) = msg.timestamp else { continue };
                prior_candidates.push((prior_file, prior.range, ts));
            }
        }
    }
    prior_candidates
}

/// Core dedup decision, separated from session IO so it is unit-testable.
///
/// `prior_candidates` are `(path_str, prior 1-based inclusive range, read_at)`.
/// Returns the pointer message when a prior read of the same file, still in
/// active context, unchanged since it was read, fully covers the requested
/// range.
fn decide_dedup(
    path: &Path,
    range: &NormalizedReadRange,
    current_mtime: std::time::SystemTime,
    prior_candidates: &[(String, (usize, usize), chrono::DateTime<chrono::Utc>)],
) -> Option<String> {
    for (prior_file, prior_range, ts) in prior_candidates {
        if paths_equivalent(prior_file, path)
            && file_unchanged_since(current_mtime, ts)
            && coverage_covers(*prior_range, range)
        {
            return Some(dedup_pointer_message(path, *prior_range, ts));
        }
    }
    None
}

/// A prior read's file path and normalized line range, parsed from a read tool
/// input object.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RangeRequest {
    file_path: Option<String>,
    range: (usize, usize), // (start_line, end_line), 1-based inclusive
}

fn range_from_tool_input(input: &Value) -> RangeRequest {
    let file_path = input
        .get("file_path")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let range = normalize_read_range_from_tool_input(input);
    RangeRequest { file_path, range }
}

/// Normalize a read tool input's `start_line`/`end_line`/`offset`/`limit` into
/// a 1-based inclusive `(start_line, end_line)` range. Mirrors
/// [`normalize_read_range`] but for already-serialized tool inputs.
fn normalize_read_range_from_tool_input(input: &Value) -> (usize, usize) {
    let start_line = input
        .get("start_line")
        .and_then(|value| value.as_u64())
        .map(|value| value as usize);
    let end_line = input
        .get("end_line")
        .and_then(|value| value.as_u64())
        .map(|value| value as usize);
    let offset = input
        .get("offset")
        .and_then(|value| value.as_u64())
        .unwrap_or(0) as usize;
    let limit = input
        .get("limit")
        .and_then(|value| value.as_u64())
        .unwrap_or(DEFAULT_LIMIT as u64) as usize;

    match (start_line, end_line) {
        (Some(start), Some(end)) => (start.max(1), end.max(start.max(1))),
        (Some(start), None) => (start.max(1), start.saturating_add(limit.saturating_sub(1))),
        (None, Some(end)) => (1, end),
        (None, None) => (offset.saturating_add(1), offset.saturating_add(limit)),
    }
}

/// Whether a requested range is fully covered by a prior range, both as
/// 1-based inclusive `(start_line, end_line)`.
fn coverage_covers(prior: (usize, usize), requested: &NormalizedReadRange) -> bool {
    // NormalizedReadRange is 0-based `offset` + `limit`; convert to 1-based
    // inclusive end for comparison.
    let requested_start = requested.offset + 1;
    let requested_end = requested.offset + requested.limit;
    prior.0 <= requested_start && prior.1 >= requested_end
}

/// Whether two paths resolve to the same file (by canonicalized absolute path).
fn paths_equivalent(a: &str, b: &Path) -> bool {
    let a_path = Path::new(a);
    let canon_a = std::fs::canonicalize(a_path).unwrap_or_else(|_| a_path.to_path_buf());
    let canon_b = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    canon_a == canon_b
}

/// Whether the file was not modified after the prior-read time `since`.
///
/// We pass the file's *current* mtime and compare it to the prior read's
/// recorded time. The prior read time is the message timestamp, which is set
/// when the read happened. If the file's mtime is strictly earlier than that
/// read time, the file was not edited after the read (editing bumps mtime to a
/// newer value), so dedup is safe. If mtime is at or after the read time, the
/// file may have changed since, so we do not dedup.
fn file_unchanged_since(
    current_mtime: std::time::SystemTime,
    since: &chrono::DateTime<chrono::Utc>,
) -> bool {
    // `Err` means current_mtime < since (strictly before the read) => the file
    // was not modified after the read => unchanged => safe to dedup.
    current_mtime
        .duration_since(std::time::SystemTime::from(*since))
        .is_err()
}

/// Build the compact pointer message returned instead of re-reading.
fn dedup_pointer_message(
    path: &Path,
    prior: (usize, usize),
    read_at: &chrono::DateTime<chrono::Utc>,
) -> String {
    format!(
        "Already in context: {} (lines {}-{}), read in this session at {}.\n\
         The file has not changed since that read, so re-reading would return the exact same \
         bytes; they are already available in the earlier tool result and are not re-sent here. \
         If you expected different content, the file must have been edited (in that case this \
         pointer would not have been returned).",
        path.display(),
        prior.0,
        prior.1,
        read_at
    )
}

fn is_binary_file(path: &Path) -> bool {
    // Check by extension first (no I/O needed)
    if let Some(ext) = path.extension() {
        let ext = ext.to_string_lossy().to_lowercase();
        let binary_exts = [
            "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "zip", "tar", "gz", "bz2", "xz",
            "7z", "rar", "exe", "dll", "so", "dylib", "o", "a", "class", "pyc", "wasm", "mp3",
            "mp4", "avi", "mov", "mkv", "flac", "ogg", "wav",
        ];
        if binary_exts.contains(&ext.as_str()) {
            return true;
        }
    }

    // Read only the first 8KB to check for binary content (not the entire file)
    use std::io::Read;
    if let Ok(mut file) = std::fs::File::open(path) {
        let mut buf = [0u8; 8192];
        if let Ok(n) = file.read(&mut buf)
            && n > 0
        {
            let null_count = buf[..n].iter().filter(|&&b| b == 0).count();
            return null_count > n / 10;
        }
    }

    false
}

fn find_similar_files(path: &Path) -> Vec<String> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let filename = path.file_name().map(|s| s.to_string_lossy().to_lowercase());

    let mut suggestions = Vec::new();

    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if let Some(ref target) = filename {
                // Simple similarity check
                let target_str: &str = target.as_ref();
                if name.contains(target_str) || target_str.contains(&name as &str) {
                    suggestions.push(entry.path().display().to_string());
                    if suggestions.len() >= 3 {
                        break;
                    }
                }
            }
        }
    }

    suggestions
}

/// Check if a file is an image based on extension
fn is_image_file(path: &Path) -> bool {
    if let Some(ext) = path.extension() {
        let ext = ext.to_string_lossy().to_lowercase();
        matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico"
        )
    } else {
        false
    }
}

/// Handle reading an image file - display in terminal if supported AND return base64 for model vision
fn handle_image_file(path: &Path, file_path: &str) -> Result<ToolOutput> {
    let protocol = ImageProtocol::detect();

    let data = std::fs::read(path)?;
    let file_size = data.len() as u64;

    let dimensions = get_image_dimensions_from_data(&data);

    let dim_str = dimensions
        .map(|(w, h)| format!("{}x{}", w, h))
        .unwrap_or_else(|| "unknown".to_string());

    let size_str = if file_size < 1024 {
        format!("{} bytes", file_size)
    } else if file_size < 1024 * 1024 {
        format!("{:.1} KB", file_size as f64 / 1024.0)
    } else {
        format!("{:.1} MB", file_size as f64 / 1024.0 / 1024.0)
    };

    let mut terminal_displayed = false;
    if protocol.is_supported() {
        let params = ImageDisplayParams::from_terminal();
        match display_image(path, &params) {
            Ok(true) => {
                terminal_displayed = true;
            }
            Ok(false) => {}
            Err(e) => {
                crate::logging::info(&format!("Warning: Failed to display image: {}", e));
            }
        }
    }

    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let media_type = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => "image/png",
    };

    const MAX_IMAGE_SIZE: u64 = 20 * 1024 * 1024;
    let mut output = if file_size <= MAX_IMAGE_SIZE {
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
        let display_note = if terminal_displayed {
            "Displayed in terminal. "
        } else {
            ""
        };
        ToolOutput::new(format!(
            "Image: {} ({})\nDimensions: {}\n{}Image sent to model for vision analysis.",
            file_path, size_str, dim_str, display_note
        ))
        .with_labeled_image(media_type, b64, file_path.to_string())
    } else {
        let display_note = if terminal_displayed {
            "\nDisplayed in terminal."
        } else {
            ""
        };
        ToolOutput::new(format!(
            "Image: {} ({})\nDimensions: {}\nImage too large for vision (max 20MB).{}",
            file_path, size_str, dim_str, display_note
        ))
    };

    output = output.with_title(format!("📷 {}", file_path));
    Ok(output)
}

/// Get image dimensions from raw data (duplicated from tui::image for convenience)
fn get_image_dimensions_from_data(data: &[u8]) -> Option<(u32, u32)> {
    // PNG: check signature and parse IHDR chunk
    if data.len() > 24 && &data[0..8] == b"\x89PNG\r\n\x1a\n" {
        let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return Some((width, height));
    }

    // JPEG: look for SOF0/SOF2 markers
    if data.len() > 2 && data[0] == 0xFF && data[1] == 0xD8 {
        let mut i = 2;
        while i + 9 < data.len() {
            if data[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = data[i + 1];
            // SOF0 (baseline) or SOF2 (progressive)
            if marker == 0xC0 || marker == 0xC2 {
                let height = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                let width = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
                return Some((width, height));
            }
            // Skip to next marker
            if i + 3 < data.len() {
                let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
                i += 2 + len;
            } else {
                break;
            }
        }
    }

    // GIF: parse header
    if data.len() > 10 && (&data[0..6] == b"GIF87a" || &data[0..6] == b"GIF89a") {
        let width = u16::from_le_bytes([data[6], data[7]]) as u32;
        let height = u16::from_le_bytes([data[8], data[9]]) as u32;
        return Some((width, height));
    }

    None
}

/// Check if a file is a PDF based on extension
fn is_pdf_file(path: &Path) -> bool {
    if let Some(ext) = path.extension() {
        ext.to_string_lossy().to_lowercase() == "pdf"
    } else {
        false
    }
}

/// Handle reading a PDF file - extract text content
#[cfg(feature = "pdf")]
fn handle_pdf_file(path: &Path, file_path: &str) -> Result<ToolOutput> {
    // Get file metadata
    let metadata = std::fs::metadata(path)?;
    let file_size = metadata.len();

    let size_str = if file_size < 1024 {
        format!("{} bytes", file_size)
    } else if file_size < 1024 * 1024 {
        format!("{:.1} KB", file_size as f64 / 1024.0)
    } else {
        format!("{:.1} MB", file_size as f64 / 1024.0 / 1024.0)
    };

    // Extract text from PDF
    match jcode_pdf::extract_text(path) {
        Ok(text) => {
            let mut output = String::new();
            output.push_str(&format!("PDF: {} ({})\n", file_path, size_str));
            output.push_str(&format!("{}\n", "=".repeat(60)));

            // Split into pages (pdf_extract uses form feed \x0c as page separator)
            let pages: Vec<&str> = text.split('\x0c').collect();
            let page_count = pages.len();

            output.push_str(&format!("Pages: {}\n\n", page_count));

            for (i, page) in pages.iter().enumerate() {
                let page_text = page.trim();
                if !page_text.is_empty() {
                    output.push_str(&format!("--- Page {} ---\n", i + 1));
                    // Limit each page to reasonable length
                    if page_text.len() > 10000 {
                        output.push_str(crate::util::truncate_str(page_text, 10000));
                        output.push_str("\n... (page truncated)\n");
                    } else {
                        output.push_str(page_text);
                    }
                    output.push_str("\n\n");
                }
            }

            Ok(ToolOutput::new(output))
        }
        Err(e) => {
            // Fall back to metadata only if text extraction fails
            Ok(ToolOutput::new(format!(
                "PDF: {} ({})\nCould not extract text: {}\nThis may be a scanned/image-based PDF.",
                file_path, size_str, e
            )))
        }
    }
}

/// Handle reading a PDF file when PDF support is not compiled in.
#[cfg(not(feature = "pdf"))]
fn handle_pdf_file(path: &Path, file_path: &str) -> Result<ToolOutput> {
    let metadata = std::fs::metadata(path)?;
    let file_size = metadata.len();

    let size_str = if file_size < 1024 {
        format!("{} bytes", file_size)
    } else if file_size < 1024 * 1024 {
        format!("{:.1} KB", file_size as f64 / 1024.0)
    } else {
        format!("{:.1} MB", file_size as f64 / 1024.0 / 1024.0)
    };

    Ok(ToolOutput::new(format!(
        "PDF: {} ({})\nPDF text extraction is not available in this build. Rebuild with the `pdf` feature enabled to extract text.",
        file_path, size_str
    )))
}
