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

/// Read the configured stale-todo threshold that the live loops use. Reads
/// `[loop_guard] todo_stale_threshold` (default 5). A value of `0` disables the
/// guard entirely.
pub(crate) fn stale_todo_threshold() -> usize {
    crate::config::config().loop_guard.todo_stale_threshold
}

/// Whether the todo guard should run at all for this session.
///
/// A session whose tool policy disables `todo` (e.g. a swarm worker, which is
/// explicitly barred from the shared plan) cannot act on a reminder, so asking
/// it to plan or refresh the list is pure noise. Skip the guard there.
pub(crate) fn todo_guard_applies(session_id: &str) -> bool {
    !crate::tool::session_tool_is_disabled(session_id, "todo")
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
                name: n, input: i, ..
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

/// Hidden reminder injected when the model keeps working without touching the
/// todo list. Wrapped in `<system-reminder>` so it stays out of the visible
/// transcript after resume, while still reaching the model on the next request.
pub const STALE_TODO_REMINDER: &str = concat!(
    "<system-reminder>You have made many tool calls without updating the todo",
    " list. The user watches todo progress live, so a stale list looks like",
    " stalled work. Mark the current item in_progress before continuing, mark",
    " finished items completed now (do not batch completions to the end), and",
    " add any newly discovered work. Update the todo tool before the next",
    " step.</system-reminder>"
);

/// Hidden reminder injected once per session when the model has done sustained
/// multi-step work but never created a todo list. Distinct text (and a distinct
/// marker) so the detector can tell "no plan yet" apart from "plan gone stale"
/// and avoid nagging a session that never plans.
pub const TODO_PLAN_REMINDER: &str = concat!(
    "<system-reminder>You have made many tool calls without a todo list. If this",
    " is multi-step work, create a short todo plan now and keep it current: the",
    " user watches todo progress live. Skip this only for a genuinely",
    " single-step task.</system-reminder>"
);

/// True when a tool call touches the todo list.
///
/// Uses the canonical resolver so every alias a provider or prompt may emit
/// (`todo`, `todos`, `todoread`, `todowrite`, the underscore variants, and the
/// `functions.` transport namespace) is recognized, instead of a hand-rolled
/// list that would silently miss one.
fn is_todo_tool_name(name: &str) -> bool {
    jcode_tool_types::resolve_tool_name(name) == "todo"
}

/// True when a `batch` call nests a todo subcall.
///
/// A todo update made through `batch` is stored as a single `batch` ToolUse
/// whose input lists the subcalls, not as a separate `todo` ToolUse, so the
/// counter must inspect the subcalls or it would inject a spurious reminder
/// right after the model updated its list via batch.
fn batch_input_touches_todo(input: &serde_json::Value) -> bool {
    input
        .get("tool_calls")
        .and_then(|value| value.as_array())
        .is_some_and(|calls| {
            calls.iter().any(|call| {
                call.get("tool")
                    .or_else(|| call.get("name"))
                    .and_then(|value| value.as_str())
                    .is_some_and(is_todo_tool_name)
            })
        })
}

/// Build the hidden stale-todo reminder message.
fn stale_todo_reminder_message_build() -> StoredMessage {
    StoredMessage {
        id: crate::id::new_id("guard_todo"),
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: STALE_TODO_REMINDER.to_string(),
            cache_control: None,
        }],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    }
}

/// Build the hidden "no todo plan yet" reminder message.
fn todo_plan_reminder_message_build() -> StoredMessage {
    StoredMessage {
        id: crate::id::new_id("guard_todo_plan"),
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: TODO_PLAN_REMINDER.to_string(),
            cache_control: None,
        }],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    }
}

/// True when `text` is one of this guard's own injected reminders. Both are reset
/// points, so the detector does not re-derive its count from material it already
/// reacted to.
fn is_todo_guard_reminder(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with(STALE_TODO_REMINDER) || text.starts_with(TODO_PLAN_REMINDER)
}

/// The model-visible text of a reminder built by this module. Both builders
/// produce exactly one text block; joining the text blocks keeps the caller
/// from accidentally sending a hardcoded constant that does not match the
/// reminder actually returned.
///
/// Test-only: the live loop must send [`reminder_display_summary`] to the client
/// (a clean, tag-free line) rather than this model-facing text, so the hidden
/// `<system-reminder>` wrapper never reaches the transcript or the UI.
#[cfg(test)]
pub(crate) fn reminder_text(message: &StoredMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Short, human-facing summary of a todo reminder, for the live client event.
///
/// The stored reminder is a hidden, model-addressed `<system-reminder>`; sending
/// its text to the UI (as the sibling repeat-tool guard does) would print the raw
/// XML wrapper and the model instructions. The client gets this tag-free notice
/// instead, so live display matches the reload behavior (the reminder stays out
/// of the visible transcript) instead of leaking internal markup.
pub(crate) fn reminder_display_summary(message: &StoredMessage) -> &'static str {
    if message.content.iter().any(|block| {
        matches!(block, ContentBlock::Text { text, .. }
            if text.trim_start().starts_with(TODO_PLAN_REMINDER))
    }) {
        TODO_PLAN_REMINDER_SUMMARY
    } else {
        STALE_TODO_REMINDER_SUMMARY
    }
}

/// Live notice shown when an incomplete list has gone stale.
pub const STALE_TODO_REMINDER_SUMMARY: &str =
    "📋 Todo list went stale after several steps; asked the model to refresh it.";

/// Live notice shown when sustained work is underway without any todo list.
pub const TODO_PLAN_REMINDER_SUMMARY: &str =
    "📋 Multi-step work without a todo list; suggested creating one.";

/// Count `ToolUse` blocks at the tail of the transcript, in reverse order, until
/// the first *reset point*: a `todo` tool call or an already-injected todo
/// reminder. This yields the number of tool calls made since the list was last
/// touched, so the reminder re-fires at most once per `threshold` calls rather
/// than every round. Exposed so callers can skip the (disk-backed) todo load
/// when the count has not reached the threshold yet.
pub(crate) fn tool_calls_since_todo_touch(messages: &[StoredMessage]) -> usize {
    let mut count = 0usize;
    for message in messages.iter().rev() {
        for block in message.content.iter().rev() {
            match block {
                ContentBlock::ToolUse { name, input, .. }
                    if is_todo_tool_name(name) || batch_input_touches_todo(input) =>
                {
                    return count;
                }
                ContentBlock::ToolUse { .. } => count += 1,
                ContentBlock::Text { text, .. } if is_todo_guard_reminder(text) => return count,
                _ => {}
            }
        }
    }
    count
}

/// Maximum stale-todo reminders injected within a single turn.
///
/// The detector resets on each injected reminder, so without a cap a very long
/// turn (dozens of tool calls) would re-inject the identical nudge every
/// `threshold` calls. One nudge plus one backstop is enough to steer the model;
/// further copies only add context noise.
pub const MAX_STALE_TODO_REMINDERS_PER_TURN: usize = 2;

/// Decide whether a todo reminder should fire for this turn.
///
/// Returns a hidden reminder message when the model has made at least
/// `threshold` tool calls since it last touched the todo list, provided `budget`
/// reminders remain for this turn. Two shapes:
///
/// - Incomplete todos exist but have gone stale: [`STALE_TODO_REMINDER`] asking
///   the model to update them.
/// - No todo list exists at all but sustained multi-step work is underway:
///   [`TODO_PLAN_REMINDER`], emitted at most once per session (any prior
///   reminder is a reset point), suggesting the model create a plan.
///
/// Pass `threshold == 0` to disable the guard or `budget == 0` once
/// [`MAX_STALE_TODO_REMINDERS_PER_TURN`] reminders have already fired.
pub fn stale_todo_reminder_from_transcript(
    messages: &[StoredMessage],
    todos: &[crate::todo::TodoItem],
    threshold: usize,
    budget: usize,
) -> Option<StoredMessage> {
    if threshold == 0 || budget == 0 {
        return None;
    }
    if tool_calls_since_todo_touch(messages) < threshold {
        return None;
    }
    let has_incomplete = todos.iter().any(|todo| {
        !crate::todo::todo_status_is_completed(&todo.status)
            && !crate::todo::todo_status_is_cancelled(&todo.status)
    });
    if !todos.is_empty() && !has_incomplete {
        // A list exists and is fully done: nothing to keep fresh.
        return None;
    }
    if todos.is_empty() {
        // A session that genuinely never plans (single-step work) must not be
        // nagged: emit the plan suggestion at most once for the whole session.
        if messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(block, ContentBlock::Text { text, .. }
                    if text.trim_start().starts_with(TODO_PLAN_REMINDER))
            })
        }) {
            return None;
        }
        Some(todo_plan_reminder_message_build())
    } else {
        Some(stale_todo_reminder_message_build())
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

    fn todo(id: &str, status: &str) -> crate::todo::TodoItem {
        crate::todo::TodoItem {
            id: id.to_string(),
            content: format!("task {id}"),
            status: status.to_string(),
            priority: "high".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn stale_todo_silent_below_threshold() {
        let transcript = vec![
            stored_message("m0", tool_use("bash", json!({"command": "ls"}))),
            stored_message("m1", tool_use("read", json!({"file_path": "a"}))),
        ];
        let todos = vec![todo("1", "in_progress")];
        assert!(
            stale_todo_reminder_from_transcript(&transcript, &todos, 3, 1).is_none(),
            "fewer than threshold tool calls must stay silent"
        );
    }

    #[test]
    fn todo_touch_aliases_reset_the_counter() {
        // Every alias the canonical resolver maps to the todo tool must reset
        // the count, including the `functions.` transport namespace and the
        // `todos` alias.
        for alias in [
            "todo",
            "todos",
            "todowrite",
            "todo_write",
            "todoread",
            "todo_read",
            "functions.todo",
        ] {
            let mut transcript: Vec<_> = (0..4)
                .map(|i| {
                    stored_message(&format!("m{i}"), tool_use("bash", json!({"command": "ls"})))
                })
                .collect();
            transcript.push(stored_message(
                "touch",
                tool_use(alias, json!({"todos": []})),
            ));
            let todos = vec![todo("1", "in_progress")];
            assert!(
                stale_todo_reminder_from_transcript(&transcript, &todos, 5, 1).is_none(),
                "`{alias}` must reset the touch counter so fewer than 5 calls remain"
            );
        }
    }

    #[test]
    fn stale_todo_fires_at_threshold_with_incomplete_work() {
        let transcript: Vec<_> = (0..5)
            .map(|i| stored_message(&format!("m{i}"), tool_use("bash", json!({"command": "ls"}))))
            .collect();
        let todos = vec![todo("1", "completed"), todo("2", "pending")];
        assert!(
            stale_todo_reminder_from_transcript(&transcript, &todos, 5, 1).is_some(),
            "5 tool calls with an incomplete todo must fire at threshold 5"
        );
    }

    #[test]
    fn stale_todo_resets_on_todo_touch() {
        // Four calls, then a todo update, then two more: only the trailing two
        // count since the list was last touched.
        let mut transcript: Vec<_> = (0..4)
            .map(|i| stored_message(&format!("m{i}"), tool_use("bash", json!({"command": "ls"}))))
            .collect();
        transcript.push(stored_message(
            "todo",
            tool_use("todo", json!({"todos": []})),
        ));
        transcript.push(stored_message(
            "m4",
            tool_use("bash", json!({"command": "ls"})),
        ));
        transcript.push(stored_message(
            "m5",
            tool_use("bash", json!({"command": "ls"})),
        ));
        let todos = vec![todo("1", "in_progress")];
        let threshold = 5;
        assert!(
            stale_todo_reminder_from_transcript(&transcript, &todos, threshold, 1).is_none(),
            "a todo touch must reset the count so only 2 calls count since it"
        );
    }

    #[test]
    fn stale_todo_resets_on_prior_reminder() {
        // A previously injected stale-todo reminder is itself a reset point, so
        // the guard re-fires at most once per threshold window.
        let mut transcript: Vec<_> = (0..5)
            .map(|i| stored_message(&format!("m{i}"), tool_use("bash", json!({"command": "ls"}))))
            .collect();
        transcript.push(stored_message(
            "reminder",
            ContentBlock::Text {
                text: STALE_TODO_REMINDER.to_string(),
                cache_control: None,
            },
        ));
        transcript.push(stored_message(
            "m5",
            tool_use("bash", json!({"command": "ls"})),
        ));
        let todos = vec![todo("1", "in_progress")];
        assert!(
            stale_todo_reminder_from_transcript(&transcript, &todos, 5, 1).is_none(),
            "the prior reminder must reset the count"
        );
    }

    #[test]
    fn stale_todo_suppressed_when_all_done() {
        let transcript: Vec<_> = (0..5)
            .map(|i| stored_message(&format!("m{i}"), tool_use("bash", json!({"command": "ls"}))))
            .collect();
        let todos = vec![
            todo("1", "completed"),
            todo("2", "completed"),
            todo("3", "cancelled"),
        ];
        assert!(
            stale_todo_reminder_from_transcript(&transcript, &todos, 5, 1).is_none(),
            "a finished list needs no live progress reminder"
        );
    }

    #[test]
    fn stale_todo_suppressed_without_todos_until_plan_reminder_fires() {
        // With no todo list at all, the guard suggests creating a plan rather
        // than staying silent, because the observed failure is a model that
        // never plans and then stamps work at the end.
        let transcript: Vec<_> = (0..5)
            .map(|i| stored_message(&format!("m{i}"), tool_use("bash", json!({"command": "ls"}))))
            .collect();
        let reminder = stale_todo_reminder_from_transcript(&transcript, &[], 5, 1)
            .expect("a sustained no-todo session should get a plan suggestion");
        let text = match reminder.content.first() {
            Some(ContentBlock::Text { text, .. }) => text,
            other => panic!("expected text block, got {other:?}"),
        };
        assert!(text.contains("without a todo list"));
    }

    #[test]
    fn plan_reminder_fires_at_most_once_per_session() {
        let mut transcript: Vec<_> = (0..5)
            .map(|i| stored_message(&format!("m{i}"), tool_use("bash", json!({"command": "ls"}))))
            .collect();
        assert!(
            stale_todo_reminder_from_transcript(&transcript, &[], 5, 1).is_some(),
            "first window should suggest a plan"
        );
        // Simulate the injected reminder, then more work with still no todos.
        transcript.push(stored_message(
            "plan-reminder",
            ContentBlock::Text {
                text: TODO_PLAN_REMINDER.to_string(),
                cache_control: None,
            },
        ));
        for i in 0..5 {
            transcript.push(stored_message(
                &format!("more{i}"),
                tool_use("bash", json!({"command": "ls"})),
            ));
        }
        assert!(
            stale_todo_reminder_from_transcript(&transcript, &[], 5, 1).is_none(),
            "a session that never plans must not be nagged more than once"
        );
    }

    #[test]
    fn stale_todo_threshold_zero_disables() {
        let transcript: Vec<_> = (0..9)
            .map(|i| stored_message(&format!("m{i}"), tool_use("bash", json!({"command": "ls"}))))
            .collect();
        let todos = vec![todo("1", "in_progress")];
        assert!(
            stale_todo_reminder_from_transcript(&transcript, &todos, 0, 1).is_none(),
            "threshold 0 must disable the stale-todo guard entirely"
        );
    }

    #[test]
    fn stale_todo_zero_budget_suppresses_repeat() {
        // Once the per-turn budget is spent, no further reminder fires even
        // though the call count still exceeds the threshold.
        let transcript: Vec<_> = (0..9)
            .map(|i| stored_message(&format!("m{i}"), tool_use("bash", json!({"command": "ls"}))))
            .collect();
        let todos = vec![todo("1", "in_progress")];
        assert!(
            stale_todo_reminder_from_transcript(&transcript, &todos, 5, 0).is_none(),
            "a spent budget must suppress further reminders"
        );
    }

    #[test]
    fn stale_todo_reminder_is_hidden_system_reminder() {
        let transcript: Vec<_> = (0..5)
            .map(|i| stored_message(&format!("m{i}"), tool_use("bash", json!({"command": "ls"}))))
            .collect();
        let todos = vec![todo("1", "in_progress")];
        let reminder =
            stale_todo_reminder_from_transcript(&transcript, &todos, 5, 1).expect("should fire");
        assert_eq!(reminder.role, Role::User);
        assert!(reminder.display_role.is_none());
        let text = match reminder.content.first() {
            Some(ContentBlock::Text { text, .. }) => text,
            other => panic!("expected text block, got {other:?}"),
        };
        assert!(
            text.starts_with("<system-reminder>") && text.ends_with("</system-reminder>"),
            "the reminder must stay hidden behind the system-reminder marker"
        );
        assert!(text.contains("todo"));
    }

    #[test]
    fn batch_nested_todo_resets_the_counter() {
        // A todo update issued through `batch` is stored as a single `batch`
        // ToolUse whose input lists the subcalls. It must still reset the touch
        // counter, or the guard would fire right after a batch todo update.
        let mut transcript: Vec<_> = (0..4)
            .map(|i| stored_message(&format!("m{i}"), tool_use("bash", json!({"command": "ls"}))))
            .collect();
        transcript.push(stored_message(
            "batch",
            tool_use(
                "batch",
                json!({"tool_calls": [
                    {"tool": "read", "file_path": "a"},
                    {"tool": "todos", "todos": []}
                ]}),
            ),
        ));
        let todos = vec![todo("1", "in_progress")];
        assert!(
            stale_todo_reminder_from_transcript(&transcript, &todos, 5, 1).is_none(),
            "a batch-nested todo update must reset the touch counter"
        );
    }

    #[test]
    fn batch_without_todo_does_not_reset_the_counter() {
        let transcript: Vec<_> = (0..5)
            .map(|i| {
                stored_message(
                    &format!("m{i}"),
                    tool_use(
                        "batch",
                        json!({"tool_calls": [{"tool": "read", "file_path": "a"}]}),
                    ),
                )
            })
            .collect();
        let todos = vec![todo("1", "in_progress")];
        assert!(
            stale_todo_reminder_from_transcript(&transcript, &todos, 5, 1).is_some(),
            "a batch with no todo subcall must not reset the counter"
        );
    }

    #[test]
    fn legacy_completion_statuses_count_as_done() {
        // Persisted sessions can carry legacy synonyms (`done`, `finished`,
        // `complete`, `canceled`). The guard must read them through the
        // canonical helpers, not raw string equality, or a fully-done legacy
        // list would look incomplete and get nudged.
        for status in ["done", "finished", "complete", "canceled"] {
            let transcript: Vec<_> = (0..5)
                .map(|i| {
                    stored_message(&format!("m{i}"), tool_use("bash", json!({"command": "ls"})))
                })
                .collect();
            let todos = vec![todo("1", status)];
            assert!(
                stale_todo_reminder_from_transcript(&transcript, &todos, 5, 1).is_none(),
                "legacy status `{status}` must count as done"
            );
        }
    }

    #[test]
    fn todo_guard_skips_sessions_without_the_todo_tool() {
        // A swarm worker is barred from the todo tool, so a reminder would be
        // unactionable. The gate must return false when the session policy
        // disables `todo`.
        let session = format!("guard-worker-{}", crate::id::new_id("t"));
        crate::tool::set_session_tool_policy(
            &session,
            None,
            std::collections::HashSet::from(["todo".to_string()]),
        );
        assert!(
            !todo_guard_applies(&session),
            "todo_guard_applies must be false when the policy disables todo"
        );
        crate::tool::clear_session_tool_policy(&session);

        // A session with no restrictive policy keeps the guard.
        assert!(
            todo_guard_applies(&session),
            "todo_guard_applies must default to true without a disabling policy"
        );
    }

    #[test]
    fn reminder_text_matches_the_built_reminder_for_both_shapes() {
        // The stale-list shape.
        let stale = stale_todo_reminder_message_build();
        assert_eq!(reminder_text(&stale), STALE_TODO_REMINDER);
        // The no-plan shape.
        let plan = todo_plan_reminder_message_build();
        assert_eq!(reminder_text(&plan), TODO_PLAN_REMINDER);
    }

    #[test]
    fn display_summary_is_tag_free_and_distinguishes_shapes() {
        // The live client event must not carry the hidden wrapper or the model
        // instructions; it sends a clean notice. Distinct per shape so the UI can
        // tell "refresh the list" from "create a plan".
        let stale = stale_todo_reminder_message_build();
        let stale_summary = reminder_display_summary(&stale);
        assert_eq!(stale_summary, STALE_TODO_REMINDER_SUMMARY);
        assert!(!stale_summary.contains("<system-reminder>"));
        assert!(!stale_summary.contains("</system-reminder>"));

        let plan = todo_plan_reminder_message_build();
        let plan_summary = reminder_display_summary(&plan);
        assert_eq!(plan_summary, TODO_PLAN_REMINDER_SUMMARY);
        assert!(!plan_summary.contains("<system-reminder>"));
        assert_ne!(stale_summary, plan_summary);
    }
}
