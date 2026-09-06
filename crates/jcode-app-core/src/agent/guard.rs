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
//! serialized input) has already appeared `REPEAT_TOOL_THRESHOLD - 1` times in a
//! row immediately before it, returns a short prompt-visible nudge asking the
//! model to change its approach or finish.

use jcode_message_types::{ContentBlock, Role};
use jcode_session_types::StoredMessage;

/// How many *consecutive identical* tool calls trip the reminder. A candidate
/// plus `REPEAT_TOOL_THRESHOLD - 1` prior identical calls in a row fires it.
pub const REPEAT_TOOL_THRESHOLD: usize = 4;

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

/// Decide whether dispatching `candidate` would exceed the repeat threshold.
///
/// `transcript` is the session's committed messages, which include any identical
/// calls already committed this same batch. `batch_prior_identical` is an extra
/// count for identical calls in the *current* response dispatched earlier in this
/// batch but not yet present in `transcript`; callers that commit every call
/// before consulting the detector should pass `0`.
///
/// Returns the number of consecutive identical occurrences *after* adding the
/// candidate when the threshold trips, otherwise `None`.
pub fn check_repeat_tool(
    transcript: &[StoredMessage],
    candidate: (&str, &serde_json::Value),
    batch_prior_identical: usize,
) -> Option<usize> {
    let (name, input) = candidate;
    let prior = consecutive_identical_tail(transcript, name, input) + batch_prior_identical;
    let total = prior + 1;
    (total >= REPEAT_TOOL_THRESHOLD).then_some(total)
}

/// Inspect a fully-committed transcript and, if the *most recent* tool call has
/// a trailing run of at least `REPEAT_TOOL_THRESHOLD` identical occurrences,
/// return the model-visible reminder. This is the simplest wiring for loops that
/// commit the whole batch (and its results) before continuing, because by then
/// the newest `ToolUse` is exactly the candidate.
pub fn repeat_reminder_from_transcript(messages: &[StoredMessage]) -> Option<StoredMessage> {
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
    check_repeat_tool(messages, (name, input), 1).map(|count| {
        repeat_reminder_message_build(name, count)
    })
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
            id: format!("t-{name}"),
            name: name.to_string(),
            input,
            thought_signature: None,
        }
    }

    #[test]
    fn below_threshold_is_ok() {
        let transcript = vec![
            stored_message("m0", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m1", tool_use("grep", json!({"query": "foo", "path": "src"}))),
        ];
        assert_eq!(
            check_repeat_tool(&transcript, (&"bash", &json!({"command": "ls"})), 0),
            None
        );
        // A second identical call is still below the threshold (only 2 total).
        let transcript2 = vec![
            stored_message("m0", tool_use("bash", json!({"command": "git status"}))),
            stored_message("m1", tool_use("bash", json!({"command": "git status"}))),
        ];
        assert_eq!(
            check_repeat_tool(&transcript2, (&"bash", &json!({"command": "git status"})), 0),
            None
        );
    }

    #[test]
    fn three_prior_identical_trips() {
        // 3 identical committed + 1 candidate = 4 >= threshold.
        let transcript = vec![
            stored_message("m0", tool_use("bash", json!({"command": "git status"}))),
            stored_message("m1", tool_use("bash", json!({"command": "git status"}))),
            stored_message("m2", tool_use("bash", json!({"command": "git status"}))),
        ];
        assert_eq!(
            check_repeat_tool(&transcript, (&"bash", &json!({"command": "git status"})), 0),
            Some(4)
        );
    }

    #[test]
    fn batch_prior_identical_counts() {
        // 2 committed + 2 more in the same batch = candidate is the 5th.
        let transcript = vec![
            stored_message("m0", tool_use("bash", json!({"command": "x"}))),
            stored_message("m1", tool_use("bash", json!({"command": "x"}))),
        ];
        assert_eq!(check_repeat_tool(&transcript, (&"bash", &json!({"command": "x"})), 2), Some(5));
    }

    #[test]
    fn different_args_break_the_run() {
        let transcript = vec![
            stored_message("m0", tool_use("bash", json!({"command": "git status"}))),
            stored_message("m1", tool_use("bash", json!({"command": "git commit"}))),
        ];
        // A different-args call ("git commit") is the trailing message, so the
        // consecutive identical run for "git status" is broken at 0. Even with
        // 1 "identical" batch_prior, the candidate is only the 1st trailing
        // match plus 1 batch => below threshold, so no repeat.
        assert_eq!(
            check_repeat_tool(&transcript, (&"bash", &json!({"command": "git status"})), 0),
            None
        );
        // A fresh candidate with different args is never a repeat.
        assert_eq!(
            check_repeat_tool(&transcript, (&"bash", &json!({"command": "git log"})), 0),
            None
        );
    }

    #[test]
    fn json_key_order_is_insensitive() {
        let transcript = vec![
            stored_message(
                "m0",
                tool_use("grep", json!({"query": "foo", "path": "src"})),
            ),
            stored_message(
                "m1",
                tool_use("grep", json!({"query": "foo", "path": "src"})),
            ),
        ];
        // Same content, different key order => still a consecutive run of 2.
        // Candidate itself is the 3rd; plus one in-batch prior accounted by the
        // caller (batch_prior_identical=1) => total 4.
        assert_eq!(
            check_repeat_tool(
                &transcript,
                (&"grep", &json!({"path": "src", "query": "foo"})),
                1
            ),
            Some(4)
        );
    }

    #[test]
    fn interleaved_text_does_not_reset_run() {
        let transcript = vec![
            stored_message("m0", tool_use("bash", json!({"command": "y"}))),
            stored_message(
                "m1",
                ContentBlock::Text {
                    text: "still working".to_string(),
                    cache_control: None,
                },
            ),
        ];
        // m1 text doesn't reset; m0 same call => 1 prior. + candidate + 2 batch = 4.
        assert_eq!(
            check_repeat_tool(&transcript, (&"bash", &json!({"command": "y"})), 2),
            Some(4)
        );
    }

    #[test]
    fn transcript_detector_fires_on_repeat_run() {
        // 3 committed identical calls + the newest (last) call counts once more.
        let transcript = vec![
            stored_message("m0", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m1", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m2", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m3", tool_use("bash", json!({"command": "ls"}))),
        ];
        let reminder = repeat_reminder_from_transcript(&transcript);
        let Some(reminder) = reminder else {
            panic!("expected a reminder for a 4x identical run");
        };
        assert_eq!(reminder.role, Role::User);
        let text = match &reminder.content[0] {
            ContentBlock::Text { text, .. } => text,
            other => panic!("expected Text block, got {other:?}"),
        };
        assert!(
            text.contains("repeated identically"),
            "unexpected text: {text}"
        );
        assert!(text.contains(REPEAT_TOOL_REMINDER));
    }

    #[test]
    fn transcript_detection_stays_silent_below_threshold() {
        let transcript = vec![
            stored_message("m0", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m1", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m2", tool_use("grep", json!({"query": "x"}))),
        ];
        // Newest is "grep"; no trailing run of 4 identical => silent.
        assert!(repeat_reminder_from_transcript(&transcript).is_none());
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
}