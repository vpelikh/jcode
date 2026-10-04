//! Desktop notification for completed long agent turns.
//!
//! When a turn finishes after a configurable duration (lower threshold when
//! the session has todos, since those indicate task-style work), the user gets
//! a desktop notification: session name + duration in the title, todo progress
//! as the subtitle, and the **full** final assistant message in the body.
//! Nothing is truncated for the durable surfaces: Notification Center
//! stores the whole body (so it survives expansion and search) and the chat
//! channels render it in full (chunked per backend). Two in-bubble transports
//! are bounded because they cannot carry an unbounded payload: the
//! terminal-native escape sequence (kitty OSC 99 / iTerm2 OSC 9) and the
//! `osascript` / `notify-send` banner fallback, which passes the text as a
//! process argument. By default it fires only while the terminal window is
//! unfocused.

use super::App;
use crate::todo::TodoItem;
#[cfg(any(target_os = "macos", test))]
use base64::Engine as _;

/// Character budget for a terminal-native notification payload (kitty OSC 99,
/// iTerm2 OSC 9). These carry the body inline in a single escape sequence, so a
/// multi-megabyte reply would emit a huge terminal write and may exceed the
/// terminal's own notification limit. The full text still goes to Notification
/// Center and the chat channels; only the in-terminal bubble is bounded.
#[cfg(any(target_os = "macos", test))]
const TERMINAL_NOTIFICATION_MAX_CHARS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TurnNotification {
    pub title: String,
    pub subtitle: Option<String>,
    pub body: String,
}

impl App {
    /// Send a desktop notification for a just-completed turn when warranted.
    /// Call at turn completion, after the final assistant message is committed.
    pub(super) fn maybe_notify_turn_complete(&self, duration_secs: Option<f32>) {
        if !self.runtime_mode_allows_turn_notifications() {
            return;
        }
        let cfg = &crate::config::config().notifications;
        if !cfg.turn_complete {
            return;
        }
        if cfg.turn_complete_only_when_unfocused && self.client_focused() {
            return;
        }
        let Some(duration) = duration_secs else {
            return;
        };

        let todos = self
            .active_client_session_id()
            .map(load_session_todos)
            .unwrap_or_default();
        let threshold = if todos.is_empty() {
            cfg.turn_complete_min_secs
        } else {
            cfg.turn_complete_todo_min_secs
                .min(cfg.turn_complete_min_secs)
        };
        if (duration as u64) < threshold.max(1) {
            return;
        }

        let notification = build_turn_notification(
            self.active_client_session_id()
                .and_then(crate::id::extract_session_name),
            duration,
            &todos,
            self.last_assistant_text_for_notification().as_deref(),
        );
        let sound = cfg.turn_complete_sound.trim();
        let sound = (!sound.is_empty()).then_some(sound);
        if !send_originating_terminal_notification(
            &notification,
            self.active_client_session_id().unwrap_or("unknown"),
            sound,
        ) {
            crate::notifications::send_desktop_notification_rich(
                &notification.title,
                notification.subtitle.as_deref(),
                &notification.body,
                sound,
            );
        }

        // Fan the same event out to every configured notification backend
        // (desktop, Telegram/Discord channels, ntfy, email). This makes the
        // Telegram control chat a notification center for all sessions: the
        // local OS banner fires as before, and remote channels mirror it.
        //
        // The detailed body carries the full assistant reply. Because ntfy
        // topics can be public, ntfy gets a short, non-sensitive safe body
        // instead of the reply text.
        {
            let session_id = self.active_client_session_id().unwrap_or("unknown");
            let dispatcher = crate::notifications::NotificationDispatcher::new();
            let mut body = String::new();
            if let Some(subtitle) = notification.subtitle.as_deref() {
                body.push_str(subtitle);
                body.push('\n');
            }
            body.push_str(&notification.body);
            let safe_body = notification
                .subtitle
                .as_deref()
                .unwrap_or("An agent turn finished. Open jcode for details.");
            dispatcher.dispatch_rich(
                &notification.title,
                safe_body,
                &body,
                crate::notifications::Priority::Default,
                Some(session_id),
            );
        }
    }

    fn runtime_mode_allows_turn_notifications(&self) -> bool {
        matches!(self.runtime_mode(), super::AppRuntimeMode::RemoteClient) && !self.is_replay
    }

    /// Final assistant text of the turn, used as the notification body.
    fn last_assistant_text_for_notification(&self) -> Option<String> {
        self.display_messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && !m.content.trim().is_empty())
            .map(|m| m.content.clone())
    }
}

/// Ask the terminal to create the notification when it has a native protocol.
///
/// On macOS, a notification emitted by `osascript` belongs to the helper
/// process, so clicking it cannot identify, much less focus, the terminal pane
/// that owns this session. Kitty retains that origin natively. The bundled
/// broker records the controlling tty and uses it to return Terminal.app and
/// iTerm2 users to the exact originating tab/session when one is exposed.
#[cfg(target_os = "macos")]
fn send_originating_terminal_notification(
    notification: &TurnNotification,
    session_id: &str,
    sound: Option<&str>,
) -> bool {
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term = std::env::var("TERM").unwrap_or_default();
    let is_kitty = term_program.eq_ignore_ascii_case("kitty") || term == "xterm-kitty";

    // Kitty's OSC 99 path is strictly better than a generic helper because the
    // terminal itself can focus the exact originating surface. All other macOS
    // terminals use the LSUIElement broker when installed; it carries durable
    // route metadata and can target Terminal.app/iTerm2 by tty on click.
    if !is_kitty
        && crate::notifications::send_macos_turn_notification(
            &notification.title,
            notification.subtitle.as_deref(),
            &notification.body,
            sound,
        )
    {
        return true;
    }

    let sequence = if is_kitty {
        kitty_notification_sequence(notification, session_id)
    } else if term_program.eq_ignore_ascii_case("iTerm.app") {
        iterm_notification_sequence(notification)
    } else {
        return false;
    };

    // Route through the serialized terminal writer. This runs on the TUI event
    // thread after a render, while the render writer thread drains frame bytes
    // asynchronously; a direct `io::stdout()` write here could otherwise
    // interleave with that stream and corrupt both (see write_serialized).
    // Report whether the bytes were handed off so a failed terminal write falls
    // through to the caller's desktop-notification fallback.
    crate::tui::terminal_writer::write_serialized(sequence.as_bytes())
}

#[cfg(not(target_os = "macos"))]
fn send_originating_terminal_notification(
    _notification: &TurnNotification,
    _session_id: &str,
    _sound: Option<&str>,
) -> bool {
    false
}

#[cfg(any(target_os = "macos", test))]
fn notification_text(notification: &TurnNotification) -> String {
    let full = match notification.subtitle.as_deref() {
        Some(subtitle) => format!("{}\n{}", subtitle, notification.body),
        None => notification.body.clone(),
    };
    // Terminal-native notifications (kitty OSC 99, iTerm2 OSC 9) render plain
    // text, so strip Markdown markers for display, then bound the payload: the
    // full body is preserved for Notification Center and the chat channels, but
    // the in-terminal bubble must not emit an unbounded escape sequence.
    terminal_payload_text(&crate::notifications::markdown_to_plain_text(&full))
}

/// Bound a terminal-native notification payload to a transport-safe size.
#[cfg(any(target_os = "macos", test))]
fn terminal_payload_text(text: &str) -> String {
    if text.chars().count() <= TERMINAL_NOTIFICATION_MAX_CHARS {
        return text.to_string();
    }
    let mut out: String = text
        .chars()
        .take(TERMINAL_NOTIFICATION_MAX_CHARS.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

#[cfg(any(target_os = "macos", test))]
fn osc_safe(text: &str) -> String {
    // Drop control characters that would terminate/corrupt the OSC payload, but
    // keep newlines so a multi-line body stays readable instead of collapsing
    // into one run when the body is no longer capped to a single line.
    text.chars()
        .filter(|ch| *ch == '\n' || !ch.is_control())
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn kitty_notification_id(session_id: &str) -> String {
    let safe: String = session_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '+' | '.'))
        .take(128)
        .collect();
    format!(
        "jcode-turn-{}",
        if safe.is_empty() { "unknown" } else { &safe }
    )
}

#[cfg(any(target_os = "macos", test))]
fn kitty_notification_sequence(notification: &TurnNotification, session_id: &str) -> String {
    // OSC 99 is Kitty's desktop-notification protocol. Notifications are tied
    // to the originating Kitty window, which is what makes click-to-focus work.
    // Base64 is required because the body can contain a newline and OSC 99's
    // unencoded form forbids every C0/C1 control character.
    let encoder = base64::engine::general_purpose::STANDARD;
    let title = encoder.encode(notification.title.as_bytes());
    let body = encoder.encode(notification_text(notification).as_bytes());
    let id = kitty_notification_id(session_id);
    // Request focus explicitly instead of relying on Kitty's current default.
    // This is the protocol guarantee that clicking the completed notification
    // returns to the exact window that emitted it.
    format!(
        "\x1b]99;i={id}:d=0:e=1:p=title;{title}\x1b\\\x1b]99;i={id}:d=1:e=1:p=body:a=focus;{body}\x1b\\"
    )
}

#[cfg(any(target_os = "macos", test))]
fn iterm_notification_sequence(notification: &TurnNotification) -> String {
    // iTerm2's OSC 9 notification is likewise associated with its source tab.
    let text = osc_safe(&format!(
        "{}: {}",
        notification.title,
        notification_text(notification)
    ));
    format!("\x1b]9;{text}\x07")
}

fn load_session_todos(session_id: &str) -> Vec<TodoItem> {
    crate::todo::load_todos(session_id).unwrap_or_default()
}

/// Build the notification. Kept free of `App` for testability.
///
/// Layout (macOS):
///   title:    jcode · <session> · done in <dur>
///   subtitle: <todo progress, e.g. "3/5 todos · 1 blocked">
///   body:     the todo work line ("✓ <just done> · → <in progress>" or a
///             blocker "⊘ <todo> needs <dep>") when todos exist, followed by
///             the **full** final assistant message. Nothing is trimmed: the
///             whole reply is delivered so Notification Center keeps it for
///             expansion/search and Telegram/Discord render it in full
///             (Telegram chunks at its own 4096-char limit when sending).
pub(super) fn build_turn_notification(
    session_name: Option<&str>,
    duration_secs: f32,
    todos: &[TodoItem],
    last_assistant_text: Option<&str>,
) -> TurnNotification {
    let mut title = String::from("jcode");
    if let Some(name) = session_name {
        title.push_str(" · ");
        title.push_str(name);
    }
    title.push_str(" · done in ");
    title.push_str(&format_duration_compact(duration_secs));

    let subtitle = todo_progress_line(todos);

    // Name the actual work when todos exist, then append the full assistant
    // message. Neither is truncated: the notification is a window onto the
    // real reply, not a one-line digest of it.
    let work_line = todo_work_line(todos);
    let full_text = last_assistant_text
        .map(full_assistant_text)
        .filter(|s| !s.is_empty());

    let mut body = String::new();
    if let Some(work) = work_line {
        body.push_str(&work);
    }
    if let Some(text) = full_text {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&text);
    }
    if body.is_empty() {
        body.push_str("Turn finished");
    }

    TurnNotification {
        title,
        subtitle,
        body,
    }
}

/// "3/5 todos" plus "· 1 blocked" when relevant; None when no todos exist.
fn todo_progress_line(todos: &[TodoItem]) -> Option<String> {
    if todos.is_empty() {
        return None;
    }
    let total = todos.len();
    let completed = todos.iter().filter(|t| t.status == "completed").count();
    let blocked = todos
        .iter()
        .filter(|t| t.status != "completed" && !t.blocked_by.is_empty())
        .count();
    let mut line = if completed == total {
        format!("✓ all {} todos", total)
    } else {
        format!("{}/{} todos", completed, total)
    };
    if blocked > 0 {
        line.push_str(&format!(" · {} blocked", blocked));
    }
    Some(line)
}

/// Names the salient todo work for the body: a blocker if one is the reason the
/// turn stopped, otherwise the most recently completed item and what's next.
/// Returns None when there are no todos (caller then uses only the assistant text).
fn todo_work_line(todos: &[TodoItem]) -> Option<String> {
    if todos.is_empty() {
        return None;
    }

    // A blocked, not-yet-done todo is the most actionable thing to surface.
    if let Some(blocked) = todos
        .iter()
        .find(|t| t.status != "completed" && !t.blocked_by.is_empty())
    {
        let dep = blocked
            .blocked_by
            .iter()
            .find_map(|id| resolve_todo_title(todos, id))
            .unwrap_or_else(|| blocked.blocked_by.join(", "));
        return Some(format!(
            "⊘ {} needs {}",
            todo_label(&blocked.content),
            todo_label(&dep)
        ));
    }

    let in_progress = todos
        .iter()
        .find(|t| t.status == "in_progress" || t.status == "in-progress");
    let last_done = todos.iter().rev().find(|t| t.status == "completed");

    let mut parts = Vec::new();
    if let Some(done) = last_done {
        let mut seg = format!("✓ {}", todo_label(&done.content));
        if let Some(conf) = done.completion_confidence
            && conf == crate::todo::ConfidenceState::Speculative
        {
            seg.push_str(&format!(" (low conf: {})", conf.as_str()));
        }
        parts.push(seg);
    }
    if let Some(next) = in_progress {
        parts.push(format!("→ {}", todo_label(&next.content)));
    } else if last_done.is_none() {
        // Nothing completed and nothing in progress: name the next pending item.
        if let Some(pending) = todos.iter().find(|t| t.status == "pending") {
            parts.push(format!("→ {}", todo_label(&pending.content)));
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn resolve_todo_title(todos: &[TodoItem], id: &str) -> Option<String> {
    todos.iter().find(|t| t.id == id).map(|t| t.content.clone())
}

/// A todo title for inline display in the notification body. Inline markdown
/// noise (list markers, `**`, backticks) is stripped for readability, but the
/// text itself is never truncated.
fn todo_label(s: &str) -> String {
    strip_markdown_inline(s.trim())
}

/// The full final assistant message, with only surrounding whitespace removed.
/// The whole reply is preserved so nothing is lost to a banner-sized cap.
fn full_assistant_text(text: &str) -> String {
    text.trim().to_string()
}

fn strip_markdown_inline(line: &str) -> String {
    let line = line.trim_start_matches('#').trim_start();
    // List/quote markers.
    let line = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("> "))
        .unwrap_or(line);
    line.replace("**", "").replace('`', "")
}

fn format_duration_compact(secs: f32) -> String {
    let secs = secs.max(0.0) as u64;
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{}m", m)
        } else {
            format!("{}m {}s", m, s)
        }
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{}h", h)
        } else {
            format!("{}h {}m", h, m)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(status: &str, blocked: bool) -> TodoItem {
        todo_named("x", status, &[]).tap(|t| {
            if blocked {
                t.blocked_by = vec!["other".to_string()];
            }
        })
    }

    fn todo_named(content: &str, status: &str, blocked_by: &[&str]) -> TodoItem {
        TodoItem {
            content: content.to_string(),
            status: status.to_string(),
            priority: "medium".to_string(),
            id: content.to_string(),
            group: None,
            confidence: None,
            completion_confidence: None,
            confidence_history: Vec::new(),
            blocked_by: blocked_by.iter().map(|s| s.to_string()).collect(),
            assigned_to: None,
        }
    }

    trait Tap: Sized {
        fn tap(mut self, f: impl FnOnce(&mut Self)) -> Self {
            f(&mut self);
            self
        }
    }
    impl Tap for TodoItem {}

    #[test]
    fn title_includes_session_and_compact_duration() {
        let n = build_turn_notification(Some("fox"), 754.0, &[], Some("All done."));
        assert_eq!(n.title, "jcode · fox · done in 12m 34s");
        assert_eq!(n.subtitle, None);
        assert_eq!(n.body, "All done.");
    }

    #[test]
    fn subtitle_holds_progress_and_body_names_the_work() {
        let todos = vec![
            todo_named("wire up parser", "completed", &[]),
            todo_named("handle reconnect", "in_progress", &[]),
        ];
        let n = build_turn_notification(None, 200.0, &todos, Some("Fixed the parser bug."));
        assert_eq!(n.title, "jcode · done in 3m 20s");
        assert_eq!(n.subtitle.as_deref(), Some("1/2 todos"));
        // Names the todo work first, then carries the full assistant reply.
        assert_eq!(
            n.body,
            "✓ wire up parser · → handle reconnect\n\nFixed the parser bug."
        );
    }

    #[test]
    fn body_names_blocker_and_its_dependency() {
        let todos = vec![
            todo_named("run migration", "pending", &[]),
            todo_named("deploy", "pending", &["run migration"]),
        ];
        let n = build_turn_notification(None, 200.0, &todos, None);
        assert_eq!(n.subtitle.as_deref(), Some("0/2 todos · 1 blocked"));
        assert_eq!(n.body, "⊘ deploy needs run migration");
    }

    #[test]
    fn low_confidence_completion_is_flagged() {
        let mut done = todo_named("risky refactor", "completed", &[]);
        done.completion_confidence = Some(crate::todo::ConfidenceState::from_legacy_score(35));
        let n = build_turn_notification(None, 200.0, &[done], None);
        assert_eq!(n.subtitle.as_deref(), Some("✓ all 1 todos"));
        assert_eq!(n.body, "✓ risky refactor (low conf: speculative)");
    }

    #[test]
    fn all_complete_celebrated_in_subtitle() {
        let done = vec![
            todo_named("a", "completed", &[]),
            todo_named("b", "completed", &[]),
        ];
        let n = build_turn_notification(None, 200.0, &done, None);
        assert_eq!(n.subtitle.as_deref(), Some("✓ all 2 todos"));
        assert_eq!(n.body, "✓ b");
    }

    #[test]
    fn full_assistant_text_used_when_no_todos() {
        let n = build_turn_notification(None, 200.0, &[], Some("Fixed the parser bug."));
        assert_eq!(n.subtitle, None);
        assert_eq!(n.body, "Fixed the parser bug.");
    }

    #[test]
    fn body_keeps_the_full_reply_untrimmed() {
        // A multi-line reply well past the old 120-char snippet cap must survive
        // verbatim; only the surrounding whitespace is trimmed.
        let long: String = (0..40)
            .map(|i| format!("line {i} of a longer answer"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("\n\n{long}\n\n");
        let n = build_turn_notification(None, 200.0, &[], Some(&text));
        assert_eq!(n.body, long);
        assert!(n.body.chars().count() > 120, "body must not be capped");
    }

    #[test]
    fn todo_labels_are_not_truncated() {
        let long_title = "x".repeat(200);
        let todos = vec![todo_named(&long_title, "in_progress", &[])];
        let n = build_turn_notification(None, 200.0, &todos, None);
        assert_eq!(n.body, format!("→ {long_title}"));
    }

    #[test]
    fn empty_inputs_fall_back_to_minimal_body() {
        let n = build_turn_notification(None, 65.0, &[], None);
        assert_eq!(n.title, "jcode · done in 1m 5s");
        assert_eq!(n.subtitle, None);
        assert_eq!(n.body, "Turn finished");
    }

    #[test]
    fn still_counts_blocked_in_subtitle() {
        let blocked = vec![todo("completed", false), todo("pending", true)];
        let n = build_turn_notification(None, 200.0, &blocked, None);
        assert_eq!(n.subtitle.as_deref(), Some("1/2 todos · 1 blocked"));
    }

    #[test]
    fn duration_formats_hours() {
        assert_eq!(format_duration_compact(59.0), "59s");
        assert_eq!(format_duration_compact(60.0), "1m");
        assert_eq!(format_duration_compact(3600.0), "1h");
        assert_eq!(format_duration_compact(3725.0), "1h 2m");
    }

    #[test]
    fn kitty_notification_is_one_completed_clickable_message() {
        let n = TurnNotification {
            title: "jcode · fox".to_string(),
            subtitle: Some("2/3 todos".to_string()),
            body: "Finished parser".to_string(),
        };
        assert_eq!(
            kitty_notification_sequence(&n, "session:fox/123"),
            "\x1b]99;i=jcode-turn-sessionfox123:d=0:e=1:p=title;amNvZGUgwrcgZm94\x1b\\\x1b]99;i=jcode-turn-sessionfox123:d=1:e=1:p=body:a=focus;Mi8zIHRvZG9zCkZpbmlzaGVkIHBhcnNlcg==\x1b\\"
        );
    }

    #[test]
    fn terminal_notification_payload_strips_osc_terminators() {
        let n = TurnNotification {
            title: "unsafe\x1b] title".to_string(),
            subtitle: None,
            body: "body\x07text".to_string(),
        };
        let sequence = iterm_notification_sequence(&n);
        assert_eq!(sequence.matches('\x07').count(), 1);
        assert!(!sequence.contains("\x1b] title"));
    }

    #[test]
    fn notification_text_bounds_a_long_terminal_payload() {
        let body = "a".repeat(5000);
        let n = TurnNotification {
            title: "jcode · fox".to_string(),
            subtitle: None,
            body: body.clone(),
        };
        // `notification_text` feeds the terminal escape sequence only; verify the
        // payload is bounded (the durable/desktop body stays full elsewhere).
        let text = notification_text(&n);
        assert_eq!(text.chars().count(), TERMINAL_NOTIFICATION_MAX_CHARS);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn build_path_keeps_full_body_while_terminal_payload_bounds() {
        let long = "x".repeat(TERMINAL_NOTIFICATION_MAX_CHARS + 500);
        assert_eq!(
            terminal_payload_text(&long).chars().count(),
            TERMINAL_NOTIFICATION_MAX_CHARS
        );
        // The build path keeps the full body; bounding happens only at the
        // transport sinks (terminal escape sequence, OS banner argv).
        let n = build_turn_notification(None, 200.0, &[], Some(&long));
        assert_eq!(n.body, long);
    }
}
