//! Compass-query-first enforcement: redirect `agentgrep` grep calls to
//! `compass_query` and refuse a raw-grep fallback until it is attempted.
//!
//! This module is the single owner of the whole tier. It contains the pure
//! decision policy (`decide_enforcement`), the availability preconditions
//! (`CompassAvailability`), the input classifiers and guidance output builders
//! (`agentgrep_requests_raw_fallback`, `agentgrep_call_is_grep_mode`,
//! `compass_redirect_output`), the per-session pending-redirect state, the
//! lifecycle phase labels, and the `Registry` integration (`enforce_compass_first`
//! / `clear_compass_redirect_after_run`) so `Registry::execute` in `tool::mod`
//! stays a thin dispatch loop.
//!
//! Why it exists: the redirect tells the model to call `compass_query` first,
//! but a model can take the documented `allow_raw_fallback` escape hatch on the
//! *very next* turn without ever attempting compass. This module records, per
//! session, that a redirect was issued and refuses the raw-fallback bypass until
//! the session makes a genuine `compass_query` attempt (any execution, even a
//! "building, try again" fail-fast, clears the flag).
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
pub(crate) fn mark_redirect_pending(session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    if let Ok(mut map) = PENDING_REDIRECT.lock() {
        map.insert(session_id.to_string());
    }
}

/// Clear the pending-redirect mark for `session_id`. Called when the session
/// executes any `compass_query` call or is switched away.
pub(crate) fn clear_redirect_pending(session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    if let Ok(mut map) = PENDING_REDIRECT.lock() {
        map.remove(session_id);
    }
}

/// Whether `session_id` has an outstanding redirect to `compass_query` that has
/// not yet been satisfied by a `compass_query` attempt.
pub(crate) fn redirect_pending(session_id: &str) -> bool {
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
pub(crate) fn raw_fallback_blocked_output() -> super::ToolOutput {
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
pub(crate) enum EnforcementDecision {
    /// Let `agentgrep` run normally (no interception).
    PassThrough,
    /// Intercept this call with the given guidance output.
    Intercept { redirect: bool, output: super::ToolOutput },
}

/// The conditions under which `compass_query` is authoritative enough to be
/// worth redirecting/blocking an `agentgrep` call. Built once by
/// [`Registry::enforce_compass_first`] (which holds the tools lock and reads
/// session policy), then passed into the pure `decide_enforcement` so the
/// preconditions are computed in one place rather than duplicated across
/// decision branches.
#[derive(Clone, Copy)]
pub(crate) struct CompassAvailability {
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

/// The input key that disables the "redirect agentgrep to compass_query"
/// enforcement for a single call. Mirrored in the agentgrep schema.
pub(crate) const AGENTGREP_RAW_FALLBACK_KEY: &str = "allow_raw_fallback";

/// Whether an agentgrep call explicitly opted out of `compass_query`-first
/// enforcement. This is the caller's documented escape hatch for searches that
/// Compass cannot serve: building outputs, logs, and files outside the indexed
/// tree (see the redirected message and the agentgrep schema).
pub(crate) fn agentgrep_requests_raw_fallback(input: &serde_json::Value) -> bool {
    match input.get(AGENTGREP_RAW_FALLBACK_KEY) {
        Some(serde_json::Value::Bool(opted_out)) => *opted_out,
        Some(serde_json::Value::String(raw)) => raw.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Whether an agentgrep call is a full-text/pattern grep search (mode "grep"
/// or omitted, which defaults to grep). Filename (`find`), single-file
/// (`outline`), and relationship (`trace`) lookups are distinct operations that
/// Compass's semantic search does not replace, so enforcement targets only the
/// grep mode.
pub(crate) fn agentgrep_call_is_grep_mode(input: &serde_json::Value) -> bool {
    match input.get("mode").and_then(|v| v.as_str()) {
        Some(m) => m.eq_ignore_ascii_case("grep"),
        None => true,
    }
}

/// The redirecting output returned when an `agentgrep` call is intercepted by
/// the `compass_query`-first code-enforcement tier. It explains why grep did
/// not run, directs the model to `compass_query`, and gives the explicit,
/// self-documenting escape hatch (retry with `allow_raw_fallback`) for searches
/// that genuinely need raw grep.
pub(crate) fn compass_redirect_output(input: &serde_json::Value) -> super::ToolOutput {
    let query = input
        .get("query")
        .or_else(|| input.get("pattern")) // legacy grep-alias calls pass `pattern`
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let query_text = if query.trim().is_empty() {
        String::new()
    } else {
        format!(" (query: {})", truncate_middle(query, 200))
    };
    // Preserve explicit search filters so the follow-up compass_query stays
    // confined to the same subset the grep call was targeting.
    //
    // Only `path` maps cleanly onto `compass_query`'s `path` filter (a file or
    // directory substring). `glob` is a filename pattern that has no direct
    // compass_query equivalent, so it is surfaced separately as a narrowing
    // hint rather than as a `path` value the model would blindly re-use.
    let path = input
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    let glob = input
        .get("glob")
        .or_else(|| input.get("include")) // legacy grep-alias calls pass `include`
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    let path_text = path
        .map(|s| format!(", keeping the search path `{}`", truncate_middle(s, 120)))
        .unwrap_or_default();
    let glob_text = glob
        .map(|s| format!(", and only files matching `{}`", truncate_middle(s, 120)))
        .unwrap_or_default();
    super::ToolOutput::new(format!(
        "⚠️ `agentgrep` was intercepted before running: `compass_query` is available \
         for this workspace and must be attempted before raw grep.\n\n\
         Do not repeat this `agentgrep` call unchanged. Instead call `compass_query` \
         with the same intent (natural language query + optional `path`{path_text}) to search the \
         code graph first{query_text}{glob_text}. The first call may build the index for this \
         workspace; that is expected.\n\n\
         Only if `compass_query` genuinely cannot answer (for example you need to search \
         files outside the indexed tree, build outputs, or logs; or the index fails to \
         build) retry `agentgrep` with `\"allow_raw_fallback\": true` to force raw grep \
         for this one call."
    ))
    .with_title("agentgrep redirected to compass_query")
    .with_metadata(serde_json::json!({
        "redirected_to": "compass_query",
        "reason": "compass-first enforcement",
    }))
}

/// Whether the operator has `compass_query`-first enforcement enabled for an
/// `agentgrep` call. Reads the `tools.prefer_compass_query` config value.
///
/// This must be called *before* the registry's tools read lock is taken:
/// `config()` can trigger a reload (disk read + listener dispatch), and doing
/// that while holding the lock could deadlock if a reload listener re-entered
/// the tool registry. Keeping the read here (rather than in
/// `enforce_compass_first`, which runs under the lock) preserves that
/// property and also avoids paying a config read on every unrelated tool call.
pub(crate) fn prefer_compass_query_for(resolved_name: &str) -> bool {
    resolved_name == "agentgrep"
        && crate::config::config().tools.prefer_compass_query
}

pub(crate) fn truncate_middle(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    // Below 3 chars the ellipsis itself cannot fit, so fall back to a plain
    // prefix to preserve the invariant that the result is at most `max` chars.
    if max < 3 {
        return s.chars().take(max).collect();
    }
    let half = (max.saturating_sub(3)) / 2;
    let mut prefix: Vec<char> = s.chars().take(half).collect();
    let mut suffix: Vec<char> = s.chars().rev().take(half).collect();
    suffix.reverse();
    let mut out: String = prefix.drain(..).collect();
    out.push_str("...");
    out.push_str(&suffix.iter().collect::<String>());
    out
}

/// TOOL_LIFECYCLE `phase` value recorded when an `agentgrep` grep is redirected
/// to `compass_query`.
pub(crate) const REDIRECT_PHASE: &str = "redirected_to_compass";
/// TOOL_LIFECYCLE `phase` value recorded when a raw-fallback grep is refused
/// because a `compass_query` redirect is still outstanding for the session.
pub(crate) const BLOCKED_PHASE: &str = "raw_fallback_blocked_pending_compass";

/// Decide the `compass_query`-first enforcement for one `agentgrep` call.
///
/// Takes the call input, the availability snapshot, and the session id, and
/// consults the session's pending-redirect flag (process-global, keyed by
/// session) to decide whether a raw-fallback grep is blocked. Returns
/// `PassThrough` when nothing should intercept the call, or the specific
/// guidance output (redirect vs blocked raw fallback) otherwise. The caller is
/// responsible for the side effects those decisions require (marking/clearing
/// the pending flag, telemetry, post-tool hooks).
pub(crate) fn decide_enforcement(
    input: &serde_json::Value,
    availability: CompassAvailability,
    session_id: &str,
) -> EnforcementDecision {
    if !availability.prefer_compass_query || !availability.compass_invokable() {
        return EnforcementDecision::PassThrough;
    }

    let grep_mode = agentgrep_call_is_grep_mode(input);

    // A plain full-text grep with compass available -> redirect to compass.
    if grep_mode && !agentgrep_requests_raw_fallback(input) {
        return EnforcementDecision::Intercept {
            redirect: true,
            output: compass_redirect_output(input),
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

/// Registry integration for the compass-query-first enforcement tier.
///
/// Kept here (an inherent impl from a submodule) so `Registry::execute` in
/// `tool::mod` stays a thin dispatch loop with no inline compass specifics; the
/// whole interception policy + side effects live in this module.
impl super::Registry {
    /// Apply the compass-query-first enforcement tier to the current tool call.
    ///
    /// For an `agentgrep` call, decides whether to redirect it to `compass_query`
    /// (a plain grep with Compass invokable) or refuse a raw-fallback bypass
    /// (retried before any compass attempt). When it intercepts, this applies the
    /// side effects (mark the pending flag for a redirect, telemetry, post-tool
    /// hook, lifecycle log) and returns `Some(output)` for `execute` to
    /// short-circuit with. Returns `None` to run the tool normally. Takes the
    /// tools read guard by value so it can drop it before observable work.
    pub(crate) fn enforce_compass_first(
        &self,
        tools_guard: tokio::sync::RwLockReadGuard<
            '_,
            std::collections::HashMap<String, std::sync::Arc<dyn super::Tool>>,
        >,
        name: &str,
        resolved_name: &str,
        input: &serde_json::Value,
        prefer_compass_query: bool,
        ctx: &super::ToolContext,
    ) -> Option<super::ToolOutput> {
        // The short-circuit only applies to agentgrep; other tools run normally.
        if resolved_name != "agentgrep" {
            return None;
        }
        let availability = CompassAvailability {
            prefer_compass_query,
            compass_registered: tools_guard.contains_key("compass_query"),
            compass_not_disabled: !super::session_tool_is_disabled(
                &ctx.session_id,
                "compass_query",
            ),
            has_working_dir: ctx.working_dir.is_some(),
        };
        let (redirect, output) = match decide_enforcement(input, availability, &ctx.session_id) {
            EnforcementDecision::PassThrough => {
                return None;
            }
            EnforcementDecision::Intercept { redirect, output } => (redirect, output),
        };
        // A redirect arms the per-session pending flag so a later raw-fallback
        // grep (before any compass attempt) is refused by the block decision.
        drop(tools_guard);
        if redirect {
            mark_redirect_pending(&ctx.session_id);
        }
        let phase = if redirect { REDIRECT_PHASE } else { BLOCKED_PHASE };
        crate::telemetry::record_tool_execution(resolved_name, input, true, 0);
        Self::fire_post_tool_hook(resolved_name, ctx, &Ok(output.clone()), 0);
        crate::logging::event_info(
            "TOOL_LIFECYCLE",
            Self::tool_lifecycle_fields(phase, name, resolved_name, input, ctx),
        );
        Some(output)
    }

    /// Release any outstanding compass-redirect after a real `compass_query`
    /// attempt. A genuine attempt satisfies the redirect whether it returned a
    /// warm result or a "building, try again" hint, letting a
    /// genuinely-unindexable project fall back to raw grep after one real
    /// compass call.
    pub(crate) fn clear_compass_redirect_after_run(
        &self,
        resolved_name: &str,
        ctx: &super::ToolContext,
    ) {
        if resolved_name == "compass_query" {
            clear_redirect_pending(&ctx.session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefer_compass_query_for_is_false_for_non_agentgrep() {
        // Enforcement only ever applies to agentgrep; for any other tool the flag
        // is false regardless of the configured value. The `&&` short-circuit in
        // the helper also means no config() read happens for non-agentgrep calls
        // (the hot-path perf guard).
        assert!(!prefer_compass_query_for("read"));
        assert!(!prefer_compass_query_for("bash"));
        assert!(!prefer_compass_query_for(""));
    }

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

    #[test]
    fn raw_fallback_blocked_output_guides_toward_compass() {
        // Pin the model-visible guidance for a blocked raw fallback: it must
        // name compass_query, state that the block clears after an attempt, and
        // carry the distinguishing title/metadata so callers can tell it from a
        // redirect.
        let out = raw_fallback_blocked_output();
        assert_eq!(
            out.title.as_deref(),
            Some("agentgrep raw fallback refused until compass_query attempted")
        );
        assert!(
            out.output.contains("compass_query"),
            "block must direct the model to compass_query, got: {}",
            out.output
        );
        assert!(
            out.output.contains("Once `compass_query` has been attempted"),
            "block must state the restriction clears after a compass attempt, got: {}",
            out.output
        );
        assert_eq!(
            out.metadata.as_ref().and_then(|m| m.get("reason")),
            Some(&serde_json::json!("compass-redirect-pending")),
            "block must carry the compass-redirect-pending reason"
        );
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
        let d = decide_enforcement(&serde_json::json!({"query": "fn main"}), avail(true), "s");
        match d {
            EnforcementDecision::Intercept { redirect: true, .. } => {}
            other => panic!("expected redirect, got {other:?}"),
        }
    }

    #[test]
    fn passes_through_when_enforcement_off() {
        let d = decide_enforcement(&serde_json::json!({"query": "x"}), avail(false), "s");
        assert!(matches!(d, EnforcementDecision::PassThrough));
    }

    #[test]
    fn passes_through_non_grep_mode() {
        let d = decide_enforcement(&serde_json::json!({"mode": "find", "query": "x"}), avail(true), "s");
        assert!(matches!(d, EnforcementDecision::PassThrough), "find must not be redirected");
    }

    #[test]
    fn raw_fallback_not_redirected_without_pending() {
        let sid = "no-pending";
        clear_redirect_pending(sid);
        let d = decide_enforcement(
            &serde_json::json!({"query": "x", "allow_raw_fallback": true}),
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
    fn empty_input_redirects_as_grep_when_compass_available() {
        // Omitted `mode` defaults to grep, so an otherwise-empty agentgrep input
        // is a full-text grep and is redirected to compass.
        let d = decide_enforcement(&serde_json::json!({}), avail(true), "s");
        match d {
            EnforcementDecision::Intercept { redirect: true, .. } => {}
            other => panic!("expected redirect for empty grep input, got {other:?}"),
        }
    }
}
