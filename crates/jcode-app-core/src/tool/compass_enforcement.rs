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
//! State is scoped to the owning session id and is never persisted. The set is
//! bounded in practice by the small overlap between a redirect and the compass
//! query that clears it, and the flag is also dropped when a session is switched
//! away, so a stale id cannot linger for a session the daemon has stopped using.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

/// Sessions with an outstanding, unsatisfied `compass_query` redirect.
///
/// The window is intentionally narrow: a redirect adds the session, any
/// `compass_query` execution (or a session switch-away) removes it. A set is
/// the right shape because there is at most one logical pending redirect per
/// session at a time, and the only transition that matters is absent/present.
static PENDING_REDIRECT: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Record that an `agentgrep` call for `session_id` was redirected to
/// `compass_query`. The mark is cleared by the next `compass_query` execution
/// for the session, whether that query succeeds or fails (see
/// [`Registry::execute`]); a redirect is also cleared when the session is
/// switched away (see `crate::agent` turn restore).
pub fn mark_redirect_pending(session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    if let Ok(mut map) = PENDING_REDIRECT.lock() {
        map.insert(session_id.to_string());
    }
}

/// Clear the pending-redirect mark for `session_id`. Called when the session
/// executes any `compass_query` call or is switched away.
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
        .map(|map| map.contains(session_id))
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

/// The decision of the `compass_query`-first enforcement tier for one
/// `agentgrep` call. `Intercept` carries which guidance was produced so the
/// caller can distinguish a redirect (which arms the pending flag) from a
/// raw-fallback block (which leaves it as-is).
#[derive(Debug)]
pub enum EnforcementDecision {
    /// Let `agentgrep` run normally (no interception).
    PassThrough,
    /// Intercept this call with the given guidance output.
    Intercept { redirect: bool, output: super::ToolOutput },
}

/// The conditions under which `compass_query` is authoritative enough to be
/// worth redirecting/blocking an `agentgrep` call. Resolved by
/// [`Registry::execute`] once (it holds the tools lock and reads session
/// policy), then passed in so the policy itself stays pure and testable.
#[derive(Clone, Copy)]
pub struct CompassAvailability {
    /// The operator enabled the enforcement tier.
    pub prefer_compass_query: bool,
    /// `compass_query` is registered for the containing registry.
    pub compass_registered: bool,
    /// `compass_query` is not disabled by the session tool policy.
    pub compass_not_disabled: bool,
    /// The session has a working directory for compass to search.
    pub has_working_dir: bool,
}

impl CompassAvailability {
    /// Whether `compass_query` is actually invokable for this session. Both the
    /// redirect and the raw-fallback block require this, so it is computed once
    /// rather than duplicated across the two inline branches.
    fn compass_invokable(&self) -> bool {
        self.compass_registered && self.compass_not_disabled && self.has_working_dir
    }
}

/// Decide the `compass_query`-first enforcement for one `agentgrep` call.
///
/// Pure: takes only the call input, the availability snapshot, and the session
/// id. Returns `PassThrough` when nothing should intercept the call, or the
/// specific guidance output (redirect vs blocked raw fallback) otherwise. The
/// caller is responsible for the side effects those decisions require (marking/
/// clearing the pending flag, telemetry, post-tool hooks).
pub fn decide_enforcement(
    input: &serde_json::Value,
    resolved_is_agentgrep: bool,
    availability: CompassAvailability,
    session_id: &str,
) -> EnforcementDecision {
    use super::agentgrep_call_is_grep_mode;
    use super::agentgrep_requests_raw_fallback;

    if !resolved_is_agentgrep
        || !availability.prefer_compass_query
        || !availability.compass_invokable()
    {
        return EnforcementDecision::PassThrough;
    }

    let grep_mode = agentgrep_call_is_grep_mode(input);

    // A plain full-text grep with compass available -> redirect to compass.
    if grep_mode && !agentgrep_requests_raw_fallback(input) {
        return EnforcementDecision::Intercept {
            redirect: true,
            output: super::compass_redirect_output(input),
        };
    }

    // A raw-fallback grep while a redirect is still outstanding -> refuse the
    // bypass (only meaningful for grep mode; find/outline/trace are distinct).
    if grep_mode
        && agentgrep_requests_raw_fallback(input)
        && redirect_pending(session_id)
    {
        return EnforcementDecision::Intercept {
            redirect: false,
            output: raw_fallback_blocked_output(),
        };
    }

    EnforcementDecision::PassThrough
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

    #[test]
    fn double_mark_is_idempotent_and_one_clear_releases() {
        // A set (not a counter): marking twice is the same as once, and a single
        // clear fully releases the session regardless of how many marks fired.
        let sid = "session_test_compass_enforce_2";
        mark_redirect_pending(sid);
        mark_redirect_pending(sid);
        assert!(redirect_pending(sid));
        clear_redirect_pending(sid);
        assert!(!redirect_pending(sid), "one clear must release a double-mark");
    }
}

#[cfg(test)]
mod decide_enforcement_tests {
    use super::*;

    fn avail(on: bool) -> CompassAvailability {
        CompassAvailability {
            prefer_compass_query: on,
            compass_registered: on,
            compass_not_disabled: on,
            has_working_dir: on,
        }
    }

    #[test]
    fn redirects_a_plain_grep_when_compass_available() {
        let d = decide_enforcement(&serde_json::json!({"query": "fn main"}), true, avail(true), "s");
        match d {
            EnforcementDecision::Intercept { redirect: true, .. } => {}
            other => panic!("expected redirect, got {other:?}"),
        }
    }

    #[test]
    fn passes_through_when_enforcement_off() {
        let d = decide_enforcement(&serde_json::json!({"query": "x"}), true, avail(false), "s");
        assert!(matches!(d, EnforcementDecision::PassThrough));
    }

    #[test]
    fn passes_through_non_grep_mode() {
        let d = decide_enforcement(&serde_json::json!({"mode": "find", "query": "x"}), true, avail(true), "s");
        assert!(matches!(d, EnforcementDecision::PassThrough), "find must not be redirected");
    }

    #[test]
    fn raw_fallback_not_redirected_without_pending() {
        let sid = "no-pending";
        clear_redirect_pending(sid);
        let d = decide_enforcement(
            &serde_json::json!({"query": "x", "allow_raw_fallback": true}),
            true,
            avail(true),
            sid,
        );
        assert!(matches!(d, EnforcementDecision::PassThrough), "raw fallback should run when no redirect pending");
    }

    #[test]
    fn raw_fallback_blocked_when_redirect_pending() {
        let sid = "pending";
        mark_redirect_pending(sid);
        let d = decide_enforcement(
            &serde_json::json!({"query": "x", "allow_raw_fallback": true}),
            true,
            avail(true),
            sid,
        );
        match d {
            EnforcementDecision::Intercept { redirect: false, .. } => {}
            other => panic!("expected block, got {other:?}"),
        }
        clear_redirect_pending(sid);
    }

    #[test]
    fn not_agentgrep_is_passthrough() {
        let d = decide_enforcement(&serde_json::json!({}), false, avail(true), "s");
        assert!(matches!(d, EnforcementDecision::PassThrough));
    }
}
