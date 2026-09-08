//! Automatic command triggers.
//!
//! A plain user prompt (not a `/command`) can carry an intent that the product
//! should honor *before* forwarding the message to the agent. This module
//! turns such natural-language statements into an explicit command invocation
//! so the behavior stays testable and reusable, instead of each caller
//! hand-parsing prose.
//!
//! The design is deliberately generic. Each [`IntentRule`] pairs a stable id
//! with a matcher that, given a plain prompt, returns an optionally
//! parameterized [`IntentCommand`]. New triggers are added by appending to
//! [`INTENT_RULES`]; the submit path only knows "does any rule match, and what
//! command should run". The set of commands is an explicit enum so a trigger
//! can never call arbitrary text.

use super::commands::{WorktreeSpec, parse_worktree_spec};
use regex::Regex;
use std::sync::OnceLock;

/// A concrete command to run, derived from a natural-language prompt.
///
/// Only the parameterized commands the product can auto-invoke are represented
/// here. The set is small and explicit so a trigger can never call arbitrary
/// shell text. Adding a new auto-invocable command means adding a variant here
/// and (optionally) a matching rule below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum IntentCommand {
    /// Create a trimmed git worktree and move the session into it.
    ///
    /// Equivalent to `/worktree <name>` (see [`WorktreeSpec`]).
    NewWorktree(WorktreeSpec),
}

/// A natural-language rule that may trigger an intent command.
pub(super) struct IntentRule {
    /// Stable identifier for telemetry and debugging (e.g. `new_worktree`).
    pub id: &'static str,
    /// Human-readable label shown in notices (e.g. "new worktree").
    pub label: &'static str,
    /// Given a plain prompt, return the command to auto-invoke.
    pub match_fn: fn(&str) -> Option<IntentCommand>,
}

/// All automatic triggers, in priority order. The first rule whose matcher
/// fires wins. Add new triggers by appending to this list.
pub(super) const INTENT_RULES: &[IntentRule] = &[IntentRule {
    id: "new_worktree",
    label: "new worktree",
    match_fn: detect_new_worktree,
}];

/// Run intent matching across every registered rule, in priority order.
///
/// This is the single entry point the submit path calls. Returns the id, label,
/// and command for the first rule that fires, or `None`.
pub(super) fn detect_intent(prompt: &str) -> Option<(&'static str, &'static str, IntentCommand)> {
    INTENT_RULES
        .iter()
        .find_map(|rule| (rule.match_fn)(prompt).map(|command| (rule.id, rule.label, command)))
}

/// Precise success notice for an auto-triggered command.
///
/// `created_display` is the user-facing path of the worktree that was created
/// and into which the session was moved. This is shown only after a real
/// create + move, so the wording states both facts accurately.
pub(super) fn intent_notice(created_display: &str) -> String {
    format!("Created worktree {created_display} and moved this session into it.")
}

/// Match a plain prompt that asks to begin work in a new worktree, deriving
/// the worktree/feature name from the message.
///
/// Conservative on purpose: it only fires for explicit "make/create/start a
/// worktree" statements, so a prompt that merely mentions worktrees ("what is
/// a worktree?", "list my worktrees") is left as ordinary user input.
fn detect_new_worktree(prompt: &str) -> Option<IntentCommand> {
    let p = prompt.trim();
    if p.is_empty() {
        return None;
    }
    let lower = p.to_lowercase();

    // Require an explicit directive meaning "begin work in a (new) worktree".
    // A bare mention ("what is a worktree?") must not trigger.
    let directive = [
        "make a worktree",
        "make a new worktree",
        "make worktree",
        "create a worktree",
        "create a new worktree",
        "create worktree",
        "start a worktree",
        "start a new worktree",
        "set up a worktree",
        "set up a new worktree",
        "spin up a worktree",
        "spin up a new worktree",
        "new worktree",
        "in a new worktree",
        "do this in a new worktree",
        "do it in a new worktree",
        "work in a new worktree",
    ]
    .iter()
    .any(|d| lower.contains(d));
    if !directive {
        return None;
    }

    // Derive a single-segment feature name. Prefer an explicit quoted name,
    // a `called/named <x>` / `for <x>` subject, else a quiet no-trigger (the
    // user can run `/worktree` manually to name it deterministically).
    let name = extract_worktree_name(p)?;
    let spec = parse_worktree_spec(&name).ok()?;
    Some(IntentCommand::NewWorktree(spec))
}

/// Whether `name` is "identifier-like" enough to safely auto-derive a worktree
/// name from natural language.
///
/// A bare single lowercase English word (e.g. `analysis`, `ui`, `project`) is
/// ambiguous — it is usually part of a larger phrase ("for the analysis work"),
/// so auto-naming a worktree after it is a guess. We only auto-derive when the
/// subject is clearly a feature identifier: it contains a digit, a separator
/// (`-`, `_`, `.`), or an uppercase letter (camelCase / an acronym like `UI`).
/// Quoted names always qualify.
///
/// This is a *principled* precision rule rather than a denylist, so it cannot
/// drift as the codebase enumerates more generic words.
fn is_identifier_like(name: &str) -> bool {
    name.chars().any(|c| {
        c.is_ascii_digit() || c == '-' || c == '_' || c == '.' || c.is_uppercase()
    })
}

/// Extract a concise worktree/feature name from a prompt, or `None`.
///
/// Order of preference:
///  1. Quoted name: `"foo-bar"` or `'foo'`.
///  2. `called <x>` / `named <x>` → the following token.
///  3. `for <x>` → the following token (the subject).
///
/// The name must be a single safe path segment that is unambiguously
/// identifier-like (quoted, or contains a digit/separator/uppercase); otherwise
/// the trigger stays silent and defers to a manual `/worktree`.
fn extract_worktree_name(prompt: &str) -> Option<String> {
    static QUOTED: OnceLock<Regex> = OnceLock::new();
    let quoted = QUOTED.get_or_init(|| {
        Regex::new(r#"["']([A-Za-z0-9][A-Za-z0-9._-]{0,63})["']"#).expect("quoted name regex")
    });
    if let Some(caps) = quoted.captures(prompt) {
        return caps.get(1).map(|m| m.as_str().to_string());
    }

    static SUBJECT: OnceLock<Regex> = OnceLock::new();
    let subject = SUBJECT.get_or_init(|| {
        Regex::new(
            r"(?:called|named|for)\s+(?:a\s+|an\s+|the\s+)?([A-Za-z0-9][A-Za-z0-9._-]{0,63})",
        )
        .expect("subject name regex")
    });
    subject
        .captures(prompt)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .filter(|name| is_identifier_like(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn does_not_trigger_on_plain_mentions() {
        for prompt in [
            "what is a worktree?",
            "list my worktrees",
            "explain how worktrees work",
            "how many worktrees do I have",
        ] {
            let got = detect_intent(prompt);
            assert!(got.is_none(), "{prompt:?} should not trigger, got {got:?}");
        }
    }

    #[test]
    fn does_not_trigger_without_a_worktree_token() {
        for prompt in [
            "create geometry for the widget",
            "make a branch for this",
            "spin up a server",
        ] {
            let got = detect_intent(prompt);
            assert!(got.is_none(), "{prompt:?} should not trigger, got {got:?}");
        }
    }

    #[test]
    fn triggers_on_explicit_worktree_request_with_quoted_name() {
        let (id, label, got) =
            detect_intent("make a worktree called \"panel-settings\" and do the work")
                .expect("should trigger");
        assert_eq!(id, "new_worktree");
        assert_eq!(label, "new worktree");
        let IntentCommand::NewWorktree(spec) = got;
        assert_eq!(spec.name, "panel-settings");
    }

    #[test]
    fn triggers_on_create_new_worktree_with_name_in_quotes() {
        let (_, _, got) =
            detect_intent("create a new worktree for \"server-split\" and do it there")
                .expect("should trigger");
        let IntentCommand::NewWorktree(spec) = got;
        assert_eq!(spec.name, "server-split");
    }

    #[test]
    fn named_subject_derives_the_worktree_name() {
        let (_, _, got) =
            detect_intent("please make a new worktree for the panel-settings feature")
                .expect("should trigger");
        let IntentCommand::NewWorktree(spec) = got;
        assert_eq!(spec.name, "panel-settings");
    }

    #[test]
    fn empty_or_trivial_prompts_do_not_trigger() {
        assert!(detect_intent("").is_none());
        assert!(detect_intent("   ").is_none());
        assert!(detect_intent("hi").is_none());
    }

    #[test]
    fn directive_without_a_derivable_name_does_not_trigger() {
        // An explicit worktree directive with no quoted/named/for name must
        // stay silent (no best-guess name), leaving the user to /worktree.
        for prompt in [
            "make a new worktree for my project work",
            "create a worktree please",
            "work in a new worktree",
        ] {
            let got = detect_intent(prompt);
            assert!(got.is_none(), "{prompt:?} should not trigger, got {got:?}");
        }
    }

    #[test]
    fn generic_subject_words_do_not_become_worktree_names() {
        // "create a worktree for the new feature UI" must not yield a worktree
        // named "new" — a generic adjective is a poor auto-derived name, so it
        // stays silent and defers to /worktree.
        for prompt in [
            "create a worktree for the new feature UI",
            "make a worktree for the next milestone",
            "set up a worktree for the upcoming feature",
        ] {
            let got = detect_intent(prompt);
            assert!(got.is_none(), "{prompt:?} should not trigger, got {got:?}");
        }
    }

    #[test]
    fn bare_lowercase_subject_defer_to_manual_worktree() {
        // A subject that is a single all-lowercase English word is ambiguous
        // ("for the analysis work" reads the subject as "analysis"), so it must
        // not auto-derive a name. The user runs /worktree to name it precisely.
        for prompt in [
            "create a worktree for the analysis",
            "create a worktree for panel",
            "make a worktree for project",
        ] {
            let got = detect_intent(prompt);
            assert!(got.is_none(), "{prompt:?} should not trigger, got {got:?}");
        }
    }

    #[test]
    fn identifier_like_subjects_derive_a_name() {
        // A subject that is clearly an identifier (hyphen, digit, or camelCase)
        // is a safe auto-derived worktree name.
        for (prompt, expected) in [
            ("create a worktree for panel-settings", "panel-settings"),
            ("make a worktree for server2", "server2"),
            ("create a worktree for the APIClient work", "APIClient"),
        ] {
            let (_, _, got) = detect_intent(prompt).expect("should trigger: {prompt}");
            let IntentCommand::NewWorktree(spec) = got;
            assert_eq!(spec.name, expected, "for prompt {prompt:?}");
        }
    }
}
