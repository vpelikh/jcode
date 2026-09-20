//! Per-session bookkeeping for the `compass_query`-first enforcement tier.
//!
//! The redirect in [`Registry::execute`] turns a full-text `agentgrep` grep call
//! into a message telling the model to call `compass_query` first. That message
//! also documents an escape hatch (`allow_raw_fallback`) for searches raw grep
//! genuinely needs to serve. As observed in production, a model can take the
//! escape hatch on the *very next* turn without ever attempting `compass_query`,
//! effectively bypassing the enforcement the tool tier exists to provide.
//!
//! This module records, per session, the moment a redirect was issued and waits
//! for that session to make a genuine `compass_query` attempt before allowing the
//! raw-grep fallback again. Until the redirect is satisfied, a retried `agentgrep`
//! that asks for the raw fallback is itself refused with a coercive message
//! pointing back at `compass_query`. The flag is cleared by any executed
//! `compass_query` call, whether it returns a warm result or a "building, try
//! again" hint, so the model is never stuck if Compass itself fails.
//!
//! State is scoped to the owning session id and is never persisted. Entries are
//! reference-counted so nothing leaks if the same session id is torn down and
//! re-used by the daemon.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// Map of session_id -> count of outstanding unsatisified compass redirects.
/// The window is intentionally narrow: a redirect sets the count, any
/// `compass_query` execution clears it. Because redirects happen at most one
/// per logical agent turn and are satisfied by the next compass query, the
/// count only ever briefly exceeds zero; it is kept as a counter so a spurious
/// double-redirect cannot leave the session permanently wedged by a stale
/// decrement on the wrong occurence.
static PENDING_REDIRECT: LazyLock<Mutex<HashMap<String, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Record that an `agentgrep` call for `session_id` was redirected to
/// `compass_query`. Returns a guard that clears the mark when the next
/// `compass_query` attempt (or an explicit acknowledgement) releases it.
pub fn mark_redirect_pending(session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    if let Ok(mut map) = PENDING_REDIRECT.lock() {
        *map.entry(session_id.to_string()).or_insert(0) = map
            .get(session_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
    }
}

/// Clear the pending-redirect mark for `session_id`. Called when the session
/// executes any `compass_query` call.
pub fn clear_redirect_pending(session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    if let Ok(mut map) = PENDING_REDIRECT.lock() {
        map.remove(session_id);
    }
}

/// Whether `session_id` has an outstanding redirect to `compass_query` that has
/// not yet been satisfied by a `compass_query` attempt.
pub fn redirect_pending(session_id: &str) -> bool {
    if session_id.is_empty() {
        return false;
    }
    PENDING_REDIRECT
        .lock()
        .map(|map| map.get(session_id).copied().unwrap_or(0) > 0)
        .unwrap_or(false)
}

/// The coercive output returned when an `agentgrep` call asks for the raw
/// fallback while a `compass_query` redirect is still outstanding for the
/// session. Refusing the bypass here guarantees the model genuinely attempts
/// `compass_query` before falling back, closing the prod-observed hole where a
/// model retried `agentgrep` with `allow_raw_fallback` on the turn after a
/// redirect and never called `compass_query` at all.
pub fn raw_fallback_blocked_output() -> super::ToolOutput {
    super::ToolOutput::new(concat!(
        "✋ `agentgrep` with `allow_raw_fallback` was refused: you were just ",
        "directed to `compass_query` but have not attempted it yet.\n\n",
        "Call `compass_query` first (same intent + optional `path`) before falling ",
        "back to raw grep. Once `compass_query` has been attempted, this ",
        "restriction clears and `agentgrep` with `allow_raw_fallback` is allowed again.",
    ))
    .with_title("agentgrep raw fallback refused until compass_query attempted")
    .with_metadata(serde_json::json!({
        "reason": "compass-redirect-pending",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_pending_lifecycle() {
        let sid = "session_test_compass_enforce_1";
        assert!(!redirect_pending(sid));
        mark_redirect_pending(sid);
        assert!(redirect_pending(sid));
        clear_redirect_pending(sid);
        assert!(!redirect_pending(sid));
    }

    #[test]
    fn empty_ids_are_ignored() {
        assert!(!redirect_pending(""));
        mark_redirect_pending("");
        assert!(!redirect_pending(""));
        clear_redirect_pending("");
    }

    #[test]
    fn redirects_are_scoped_per_session() {
        let a = "session_test_compass_enforce_a";
        let b = "session_test_compass_enforce_b";
        mark_redirect_pending(a);
        assert!(redirect_pending(a));
        assert!(!redirect_pending(b));
        clear_redirect_pending(a);
        assert!(!redirect_pending(a));
    }
}