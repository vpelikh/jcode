//! The **prune** stage: deterministic, model-free context reclamation.
//!
//! Takeaway #6 ("Separate 'prune' from 'summarize'") calls for splitting the
//! cheap, repeatable, non-model work from the expensive model-driven summary.
//! Everything in this module is a *single-node surface replacement*: it shrinks
//! (or replaces with a short marker) an oversized content node without invoking
//! any model and without restructuring the conversation. It therefore:
//!
//! - is deterministic and replayable (each change is a node-local replacement,
//!   which the session layer records as a `ReplaceMessages` event);
//! - is cheap enough to run on a frequent schedule (every step) rather than
//!   only at compaction time;
//! - is reusable verbatim by the HTTP 413 "request too large" recovery path,
//!   which shares the exact same deterministic shrink-and-mark semantics.
//!
//! The summarizer stays free to assume tool-result sizes are already bounded
//! and to focus purely on summarizing the remaining history.

use jcode_message_types::ContentBlock;

use crate::{
    EMERGENCY_IMAGE_MAX_CHARS, EMERGENCY_TOOL_RESULT_MAX_CHARS,
    PAYLOAD_IMAGE_EMERGENCY_CHAR_BUDGET, PAYLOAD_TOOL_RESULT_CHAR_BUDGET,
    emergency_truncate_tool_results_in_contents, emergency_truncated_tool_result,
    strip_large_images_in_contents,
};

/// A policy describing what a single prune pass should do.
///
/// Two families of budgets exist, mirroring the takeaway's two uses:
/// - **Per-node caps** (every-step, cheap): snap any oversized node down to a
///   fixed character bound so a pathological single result or screenshot does
///   not dominate the transcript.
/// - **Total budgets** (413 recovery): strip *oldest-first* (images) or
///   *largest-first* (tool results) until the whole category fits under a byte
///   budget, for the case where the *aggregate* (accumulated screenshots or
///   file/cat results) blew the provider body cap.
#[derive(Debug, Clone)]
pub struct PrunePolicy {
    /// Per-node cap on inline-image payload. A single image larger than this is
    /// replaced with a descriptive text marker.
    pub image_max_chars: Option<usize>,
    /// Per-node cap on tool-result text. A single result larger than this keeps
    /// its head and tail but shrinks to this bound.
    pub tool_result_max_chars: Option<usize>,
    /// Strip *oldest-first* until total inline-image payload fits under this
    /// budget (replacing dropped images with markers).
    pub image_total_budget: Option<usize>,
    /// Trim *largest-first* until total tool-result payload fits under this
    /// budget, keeping each affected result's head and tail.
    pub tool_result_total_budget: Option<usize>,
}

impl PrunePolicy {
    /// Per-node caps only: the cheap, run-every-step policy. Bounds any single
    /// oversized node but never performs aggregate oldest-first surgery.
    pub fn node_caps() -> Self {
        Self {
            image_max_chars: Some(EMERGENCY_IMAGE_MAX_CHARS),
            tool_result_max_chars: Some(EMERGENCY_TOOL_RESULT_MAX_CHARS),
            image_total_budget: None,
            tool_result_total_budget: None,
        }
    }

    /// The HTTP 413 "request too large" recovery policy: aggregate, oldest-first
    /// (images) / largest-first (tool results) budgets that mirror the emergency
    /// image budget and the payload tool-result budget. This is behaviorally
    /// identical to the historical 413 recovery: strip images to the emergency
    /// budget, and only if that strips no image, trim tool results to the
    /// payload budget. No per-node caps (the aggregate budget already bounds
    /// the whole category, and a single node far over it simply gets dropped
    /// by the aggregate pass).
    pub fn payload_413() -> Self {
        Self {
            image_max_chars: None,
            tool_result_max_chars: None,
            image_total_budget: Some(PAYLOAD_IMAGE_EMERGENCY_CHAR_BUDGET),
            tool_result_total_budget: Some(PAYLOAD_TOOL_RESULT_CHAR_BUDGET),
        }
    }
}

impl Default for PrunePolicy {
    fn default() -> Self {
        Self::node_caps()
    }
}

/// The outcome report of a prune pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Number of inline-image nodes replaced with a text marker.
    pub images_stripped: usize,
    /// Number of tool-result content nodes truncated to their cap or budget.
    pub tool_results_truncated: usize,
}

impl PruneReport {
    /// Whether any node was changed by the pass.
    pub fn is_empty(&self) -> bool {
        self.images_stripped == 0 && self.tool_results_truncated == 0
    }
}

/// Apply the policy to a slice of content-block vectors (the stored-message
/// representation, shared by the session and the provider view).
///
/// Order, deliberately:
/// 1. **Oldest-first image strip** to the aggregate image budget — the classic
///    413 driver is accumulated screenshots, so remove the oldest first rather
///    than trimming the newest.
/// 2. **Largest-first tool-result trim** to the aggregate tool-result budget —
///    but only if step 1 reclaimed no image (mirrors the historical 413
///    escalation: images first, then tool results — never both for the same
///    failure).
/// 3. **Per-node caps** for both images and tool results, so no single node can
///    dominate even when the aggregate is already under budget.
///
/// Returns a report of what changed.
pub fn prune_contents(
    contents: &mut [&mut Vec<ContentBlock>],
    policy: &PrunePolicy,
) -> PruneReport {
    let mut report = PruneReport::default();

    // Step 1: aggregate image budget (oldest-first).
    if let Some(budget) = policy.image_total_budget {
        report.images_stripped += strip_large_images_in_contents(contents, budget);
    }

    // Step 2: aggregate tool-result budget, but only when no image byte was
    // reclaimed (mirrors historical escalation: never require images and tool
    // results both).
    if report.images_stripped == 0 {
        if let Some(budget) = policy.tool_result_total_budget {
            report.tool_results_truncated +=
                emergency_truncate_tool_results_in_contents(contents, budget);
        }
    }

    // Step 3: per-node caps.
    if let Some(cap) = policy.image_max_chars {
        report.images_stripped += strip_oversized_images_node(contents, cap);
    }
    if let Some(cap) = policy.tool_result_max_chars {
        report.tool_results_truncated += truncate_oversized_tool_results_node(contents, cap);
    }

    report
}

/// The text marker used to replace a dropped inline image. Exposed so callers
/// can render their own user-facing description of what was pruned.
pub fn prune_image_marker(media_type: String, original_base64_chars: usize) -> String {
    format!(
        "[Image omitted during context pruning: media_type={media_type}, original_base64_chars={original_base64_chars}. Rely on adjacent browser/tool text, screenshots saved to disk, or re-open/re-screenshot if visual details are needed.]"
    )
}

/// Per-node image cap: replace any inline image whose base64 payload exceeds
/// `max_chars` with a descriptive text marker. Returns the number replaced.
fn strip_oversized_images_node(contents: &mut [&mut Vec<ContentBlock>], max_chars: usize) -> usize {
    let mut replaced = 0;
    for content in contents.iter_mut() {
        for block in content.iter_mut() {
            if let ContentBlock::Image { media_type, data } = block {
                if data.len() > max_chars {
                    let marker = prune_image_marker(media_type.clone(), data.len());
                    *block = ContentBlock::Text {
                        text: marker,
                        cache_control: None,
                    };
                    replaced += 1;
                }
            }
        }
    }
    replaced
}

/// Per-node tool-result cap: shorten any single tool-result text above
/// `max_chars`, keeping head and tail. Returns the number truncated.
fn truncate_oversized_tool_results_node(
    contents: &mut [&mut Vec<ContentBlock>],
    max_chars: usize,
) -> usize {
    let mut truncated = 0;
    for content in contents.iter_mut() {
        for block in content.iter_mut() {
            if let ContentBlock::ToolResult { content: text, .. } = block {
                if text.len() > max_chars {
                    *text = emergency_truncated_tool_result(text, max_chars);
                    truncated += 1;
                }
            }
        }
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_block(data_len: usize) -> ContentBlock {
        ContentBlock::Image {
            media_type: "image/png".to_string(),
            data: "x".repeat(data_len),
        }
    }

    fn tool_block(text_len: usize) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "y".repeat(text_len),
            is_error: None,
        }
    }

    fn to_contents<'a>(blocks: &'a mut Vec<Vec<ContentBlock>>) -> Vec<&'a mut Vec<ContentBlock>> {
        blocks.iter_mut().collect()
    }

    #[test]
    fn node_caps_prune_oversized_single_nodes() {
        let mut blocks = vec![
            vec![image_block(2000), tool_block(10_000)],
            vec![tool_block(5), image_block(100)],
        ];
        let mut v = to_contents(&mut blocks);
        let report = prune_contents(&mut v, &PrunePolicy::node_caps());
        assert_eq!(report.images_stripped, 1, "only the >1024 image is pruned");
        assert_eq!(
            report.tool_results_truncated, 1,
            "only the >4000 tool result is pruned"
        );
        // Image 2 survived (100 chars) and the small tool result (5) survived.
        // The 10_000 result was shortened to <= 4000.
        let second = &blocks[0];
        assert!(matches!(second[0], ContentBlock::Text { .. }));
        assert!(matches!(second[1], ContentBlock::ToolResult { .. }));
        let tool_len = match &second[1] {
            ContentBlock::ToolResult { content, .. } => content.len(),
            _ => 0,
        };
        assert!(tool_len <= 4000);
    }

    #[test]
    fn node_caps_noop_when_everything_under_caps() {
        let mut blocks = vec![
            vec![image_block(100), tool_block(200)],
            vec![tool_block(10)],
        ];
        let mut v = to_contents(&mut blocks);
        let report = prune_contents(&mut v, &PrunePolicy::node_caps());
        assert!(report.is_empty());
    }

    #[test]
    fn payload_413_applies_aggregate_image_budget_then_tool_results() {
        // Two 3 MB images total 6 MB > the 4 MiB emergency image budget, so the
        // aggregate pass drops the oldest until under budget.
        let mut blocks = vec![
            vec![image_block(3_000_000), image_block(3_000_000)],
            vec![tool_block(100)],
        ];
        let mut v = to_contents(&mut blocks);
        let report = prune_contents(&mut v, &PrunePolicy::payload_413());
        assert!(
            report.images_stripped >= 1,
            "images dropped to stay under 4 MB"
        );
        // Tool-result pass is skipped when images reclaimed something.
        assert_eq!(report.tool_results_truncated, 0);
    }

    #[test]
    fn payload_413_truncates_tool_results_when_no_images() {
        let mut blocks = vec![
            vec![image_block(100)],
            vec![tool_block(9_000_000), tool_block(9_000_000)],
        ];
        let mut v = to_contents(&mut blocks);
        let report = prune_contents(&mut v, &PrunePolicy::payload_413());
        // No image over the image budget, so the tool-result pass applies.
        assert_eq!(report.images_stripped, 0);
        // The two 9 MB results are trimmed under the 8 MiB tool budget.
        assert!(report.tool_results_truncated > 0);
    }

    #[test]
    fn report_is_empty_until_something_changes() {
        let mut a = vec![vec![image_block(100)]];
        let mut v = to_contents(&mut a);
        assert!(prune_contents(&mut v, &PrunePolicy::node_caps()).is_empty());
        let mut b = vec![vec![image_block(10_000)]];
        let mut v2 = to_contents(&mut b);
        assert!(!prune_contents(&mut v2, &PrunePolicy::node_caps()).is_empty());
    }
}
