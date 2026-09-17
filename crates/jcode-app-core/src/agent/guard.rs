//! Guard rails for runaway agent-loop behaviour (deepseek-harness takeaway #7).
//!
//! `deepseek-harness` ships a *repeat-tool reminder*: when the model repeats the
//! exact same tool call, inject a reminder to change approach or finish, so it
//! stops burning tokens in a stuck loop. This module implements that detector
//! purely and unit-testably so the live turn loops can consult it before
//! dispatching repeated tool calls.
//!
//! The detector is intentionally model-free: it costs no API call and no extra
//! latency. It reads the already-committed transcript (which also captures calls
//! committed earlier in the same batch) and, when the same tool call (name +
//! serialized input) has already appeared `threshold - 1` times in a row
//! immediately before it, returns a short prompt-visible nudge asking the model
//! to change its approach or finish. The threshold is configurable via
//! `[loop_guard] repeat_tool_threshold` (default 4; 0 disables the guard).

use jcode_message_types::{ContentBlock, Role};
use jcode_session_types::StoredMessage;

/// Read the configured repeat-tool threshold that the live loops use. Reads
/// `[loop_guard] repeat_tool_threshold` (default 4). A value of `0` disables the
/// guard entirely.
pub(crate) fn repeat_reminder_threshold() -> usize {
    crate::config::config().loop_guard.repeat_tool_threshold
}

/// Short prompt-visible nudge, kept minimal so it adds negligible token cost
/// (the whole point is to avoid burning tokens in a stuck loop).
pub const REPEAT_TOOL_REMINDER: &str = concat!(
    "[You have called the exact same tool with the exact same arguments ",
    "multiple times in a row without making progress. ",
    "Please change your approach or finish this step.]"
);

/// Canonical, order-insensitive serialization of a tool call's arguments so two
/// JSON documents with keys in different order compare equal. Recursively sorts
/// object keys (and arrays) to a canonical form. Falls back to the raw debug
/// text if JSON serialization fails (should not happen in practice).
fn tool_signature(name: &str, input: &serde_json::Value) -> String {
    let canonical = canonicalize(input);
    match serde_json::to_string(&canonical) {
        Ok(text) => format!("{name}\u{0}{text}"),
        Err(_) => format!("{name}\u{0}{input}"),
    }
}

/// Return a canonicalized copy of `value`: object keys sorted (recursively),
/// array/object nesting preserved. Leaf scalars are unchanged.
fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = map
                .iter()
                .map(|(k, v)| (k.clone(), canonicalize(v)))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Map::from_iter(entries).into()
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize).collect())
        }
        other => other.clone(),
    }
}

/// Count consecutive `ToolUse` blocks at the end of the transcript that are
/// identical to `(name, input)`. Non-tool content blocks (e.g. interleaved text)
/// do not reset the run, because a model can interleave prose and still be
/// stuck on the same call.
fn consecutive_identical_tail(
    messages: &[StoredMessage],
    name: &str,
    input: &serde_json::Value,
) -> usize {
    let sig = tool_signature(name, input);
    let mut count = 0usize;
    for message in messages.iter().rev() {
        for block in message.content.iter().rev() {
            if let ContentBlock::ToolUse {
                name: n,
                input: i,
                ..
            } = block
            {
                if tool_signature(n, i) == sig {
                    count += 1;
                } else {
                    return count;
                }
            }
        }
    }
    count
}

/// Inspect a fully-committed transcript and, if the *most recent* tool call has
/// a trailing run of at least `threshold` identical occurrences, return the
/// model-visible reminder. This is the simplest wiring for loops that commit
/// the whole batch (and its results) before continuing, because by then the
/// newest `ToolUse` is exactly the candidate.
///
/// `threshold` is the number of *consecutive identical* tool calls that trip
/// the reminder. Pass `0` (or a value below the natural minimum) to disable the
/// guard entirely. The shipped default is 4 (see `[loop_guard]
/// repeat_tool_threshold`); callers read `config().loop_guard.repeat_tool_threshold`
/// via [`repeat_reminder_threshold`] so the user can tune sensitivity.
pub fn repeat_reminder_from_transcript(
    messages: &[StoredMessage],
    threshold: usize,
) -> Option<StoredMessage> {
    if threshold == 0 {
        return None;
    }
    let mut last: Option<(&str, &serde_json::Value)> = None;
    // Find the newest ToolUse block (last stored tool call).
    'outer: for message in messages.iter().rev() {
        for block in message.content.iter().rev() {
            if let ContentBlock::ToolUse { name, input, .. } = block {
                last = Some((name, input));
                break 'outer;
            }
        }
    }
    let (name, input) = last?;
    // The candidate is the newest committed `ToolUse`, so `consecutive_identical_tail`
    // already counts it (and its identical predecessors). We must NOT add a phantom
    // `+1` for the candidate (it is already in the transcript): the run length is the
    // full committed run, and the reminder trips once that run reaches `threshold`.
    let run_len = consecutive_identical_tail(messages, name, input);
    (run_len >= threshold).then(|| repeat_reminder_message_build(name, run_len))
}

/// Build the model-visible, user-role reminder message for a repeated tool call.
pub(crate) fn repeat_reminder_message_build(tool_name: &str, count: usize) -> StoredMessage {
    StoredMessage {
        id: crate::id::new_id("guard_repeat"),
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: format!(
                "[Guard] Tool `{tool_name}` repeated identically {count} times in a row. {REPEAT_TOOL_REMINDER}"
            ),
            cache_control: None,
        }],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Mirrors the shipped default of `[loop_guard] repeat_tool_threshold`.
    const REPEAT_TOOL_THRESHOLD: usize = 4;

    fn stored_message(id: &str, block: ContentBlock) -> StoredMessage {
        StoredMessage {
            id: id.to_string(),
            role: Role::User,
            content: vec![block],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        }
    }

    fn tool_use(name: &str, input: serde_json::Value) -> ContentBlock {
        ContentBlock::ToolUse {
            id: format!("t-{name}").into(),
            name: name.to_string(),
            input,
            thought_signature: None,
        }
    }

    #[test]
    fn transcript_detection_stays_silent_below_threshold() {
        let transcript = vec![
            stored_message("m0", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m1", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m2", tool_use("grep", json!({"query": "x"}))),
        ];
        // Newest is "grep"; no trailing run of 4 identical => silent.
        assert!(repeat_reminder_from_transcript(&transcript, REPEAT_TOOL_THRESHOLD).is_none());
    }

    #[test]
    fn transcript_detector_does_not_fire_below_threshold() {
        // Regression: a repeated run strictly below REPEAT_TOOL_THRESHOLD (3
        // committed identical calls) must NOT nudge. The caller commits the whole
        // batch before consulting the detector, so the newest committed ToolUse
        // is the candidate and must not be double-counted.
        let transcript = vec![
            stored_message("m0", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m1", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m2", tool_use("bash", json!({"command": "ls"}))),
        ];
        assert!(
            repeat_reminder_from_transcript(&transcript, REPEAT_TOOL_THRESHOLD).is_none(),
            "3 identical calls must stay below the threshold"
        );
    }

    #[test]
    fn transcript_detector_fires_exactly_at_threshold() {
        // Regression: exactly REPEAT_TOOL_THRESHOLD (4) committed identical
        // calls must nudge exactly once, reporting a count of 4 (not 5).
        let transcript = vec![
            stored_message("m0", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m1", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m2", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m3", tool_use("bash", json!({"command": "ls"}))),
        ];
        let reminder = repeat_reminder_from_transcript(&transcript, REPEAT_TOOL_THRESHOLD)
            .expect("4 identical calls must trigger the reminder");
        let text = match &reminder.content[0] {
            ContentBlock::Text { text, .. } => text,
            other => panic!("expected Text block, got {other:?}"),
        };
        assert!(
            text.contains("repeated identically 4 times"),
            "reported count must be exactly 4, got text: {text}"
        );
    }

    #[test]
    fn canonicalize_sorts_nested_keys() {
        let a = json!({"b": {"z": 1, "a": 2}, "c": [{"y": 1, "x": 2}]});
        let b = json!({"c": [{"x": 2, "y": 1}], "b": {"a": 2, "z": 1}});
        assert_eq!(tool_signature("t", &a), tool_signature("t", &b));
        // Different values must differ.
        let c = json!({"b": {"z": 1, "a": 3}, "c": [{"y": 1, "x": 2}]});
        assert_ne!(tool_signature("t", &a), tool_signature("t", &c));
    }

    /// A lower configured threshold fires earlier (tunable sensitivity).
    #[test]
    fn configurable_lower_threshold_fires_sooner() {
        let transcript = vec![
            stored_message("m0", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m1", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m2", tool_use("bash", json!({"command": "ls"}))),
        ];
        // Threshold 4 must stay silent for 3 identical calls.
        assert!(
            repeat_reminder_from_transcript(&transcript, 4).is_none(),
            "3 identical calls must stay silent at threshold 4"
        );
        // Threshold 2 must fire for 3 identical calls.
        assert!(
            repeat_reminder_from_transcript(&transcript, 2).is_some(),
            "3 identical calls must fire at threshold 2"
        );
    }

    /// A threshold of 0 disables the guard entirely.
    #[test]
    fn threshold_zero_disables_guard() {
        let transcript = vec![
            stored_message("m0", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m1", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m2", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m3", tool_use("bash", json!({"command": "ls"}))),
        ];
        assert!(
            repeat_reminder_from_transcript(&transcript, 0).is_none(),
            "threshold 0 must disable the repeat-tool reminder entirely"
        );
    }
}
