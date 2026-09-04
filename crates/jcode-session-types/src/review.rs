//! Review-rounds types: the machine-parseable reviewer report contract, the
//! per-lens definitions, the convergence/stall fingerprint, the persisted loop
//! state, and the per-session review record.
//!
//! These types live in `jcode-session-types` (a dependency of both `jcode-base`
//! for `Session` persistence and `jcode-tui` for the loop engine) so the
//! foundation layer can own them without pulling in any TUI crate.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// The six independent review lenses run in order after the completion gates
/// pass. Each lens is reviewed by its own independent, read-only spawned
/// reviewer so later lenses are not biased by earlier findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReviewLens {
    /// Does the changed code actually do what it claims?
    Correctness,
    /// Edge cases, error paths, panics, and unhandled inputs.
    EdgesErrors,
    /// Security: injection, authz, secret handling, unsafe APIs.
    Security,
    /// Performance: hot loops, allocations, blocking, N+1.
    Performance,
    /// Build/leftovers: warnings, unused, dead code, dev artifacts.
    BuildLeftovers,
    /// Requirement traceability: every explicit requirement has a check.
    RequirementTraceability,
}

impl ReviewLens {
    /// The canonical loop order. Convergence is only declared after a final
    /// confirmation pass over lenses whose scope touched later changes.
    pub const ALL: [ReviewLens; 6] = [
        ReviewLens::Correctness,
        ReviewLens::EdgesErrors,
        ReviewLens::Security,
        ReviewLens::Performance,
        ReviewLens::BuildLeftovers,
        ReviewLens::RequirementTraceability,
    ];

    /// Short machine name used in prompts, logs, and the report contract.
    pub fn name(&self) -> &'static str {
        match self {
            ReviewLens::Correctness => "correctness",
            ReviewLens::EdgesErrors => "edges_errors",
            ReviewLens::Security => "security",
            ReviewLens::Performance => "performance",
            ReviewLens::BuildLeftovers => "build_leftovers",
            ReviewLens::RequirementTraceability => "requirement_traceability",
        }
    }

    /// Human-facing label for digests and status notices.
    pub fn label(&self) -> &'static str {
        match self {
            ReviewLens::Correctness => "Correctness",
            ReviewLens::EdgesErrors => "Edges/Errors",
            ReviewLens::Security => "Security",
            ReviewLens::Performance => "Performance",
            ReviewLens::BuildLeftovers => "Build/Leftovers",
            ReviewLens::RequirementTraceability => "Requirement Traceability",
        }
    }

    /// Free-text focus used to assemble a per-lens reviewer prompt.
    pub fn focus(&self) -> &'static str {
        match self {
            ReviewLens::Correctness => {
                "Verify the changed code does what the task asked. Look for logic bugs, \
                 wrong control flow, incorrect assumptions, broken invariants, and \
                 regressions against existing behavior."
            }
            ReviewLens::EdgesErrors => {
                "Inspect edge cases and error handling in the changed code. Look for \
                 unhandled inputs, panics, swallowed errors, missing validation, and \
                 off-by-one or null/empty boundary mistakes."
            }
            ReviewLens::Security => {
                "Inspect the changed code for security issues: injection, missing \
                 authorization checks, unsafe deserialization, secret leakage, path \
                 traversal, and misuse of unsafe APIs."
            }
            ReviewLens::Performance => {
                "Inspect the changed code for performance problems: redundant work in hot \
                 paths, unbounded allocations, blocking calls where async is expected, \
                 N+1 patterns, and unnecessary copies."
            }
            ReviewLens::BuildLeftovers => {
                "Inspect the changed code for build hygiene: compiler warnings, unused \
                 imports/code, dead branches, leftover debug prints, and stray dev \
                 artifacts that should not ship."
            }
            ReviewLens::RequirementTraceability => {
                "Check that every explicit requirement of this batch is actually met by \
                 the changed code. Note anything in scope that is missing, partial, or \
                 only implied but not implemented, with a concrete check for each."
            }
        }
    }

    /// Parse a lens from its machine name (used by configs/tests).
    pub fn from_name(name: &str) -> Option<ReviewLens> {
        match name {
            "correctness" => Some(ReviewLens::Correctness),
            "edges_errors" => Some(ReviewLens::EdgesErrors),
            "security" => Some(ReviewLens::Security),
            "performance" => Some(ReviewLens::Performance),
            "build_leftovers" => Some(ReviewLens::BuildLeftovers),
            "requirement_traceability" => Some(ReviewLens::RequirementTraceability),
            _ => None,
        }
    }
}

/// A single reviewer finding, in the machine-parseable contract form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Severity token, e.g. `HIGH`, `MEDIUM`, `LOW`, `INFO`.
    pub severity: String,
    /// File path the finding applies to (may be empty for repo-wide items).
    pub path: String,
    /// Concise issue text.
    pub text: String,
}

impl Finding {
    pub fn new(severity: impl Into<String>, path: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            severity: severity.into(),
            path: path.into(),
            text: text.into(),
        }
    }

    /// The stable fingerprint key for this finding: (severity, path). Two
    /// findings that share a key on the open-findings set count as the same
    /// outstanding issue, which is what the stall logic compares across a
    /// post-fix re-run.
    pub fn fingerprint_key(&self) -> (String, String) {
        (self.severity.to_lowercase(), self.path.clone())
    }
}

/// The verdict a reviewer returns, parsed from its final message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewReport {
    /// The lens is clean: nothing to fix in the changed code.
    Clean,
    /// The lens produced one or more findings to fix.
    Findings(Vec<Finding>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewReportParseError {
    /// No `VERDICT:` line was present at all.
    MissingVerdict,
    /// The `VERDICT:` token was neither `CLEAN` nor `FINDINGS`.
    UnknownVerdict(String),
    /// `VERDICT: FINDINGS` was declared but no `FINDING:` lines followed.
    FindingsWithoutItems,
}

impl std::fmt::Display for ReviewReportParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReviewReportParseError::MissingVerdict => {
                write!(f, "reviewer report missing VERDICT line")
            }
            ReviewReportParseError::UnknownVerdict(v) => {
                write!(f, "reviewer report unknown VERDICT: {v}")
            }
            ReviewReportParseError::FindingsWithoutItems => {
                write!(f, "reviewer declared FINDINGS but listed none")
            }
        }
    }
}

impl std::error::Error for ReviewReportParseError {}

impl ReviewReport {
    /// Parse a reviewer's final message into a `ReviewReport` using the report
    /// contract:
    ///
    /// ```text
    /// VERDICT: CLEAN | FINDINGS
    /// FINDING: <severity>|<file>|<issue text>
    /// ```
    ///
    /// The `VERDICT:` token is required. `CLEAN` may optionally be followed by
    /// explanatory prose. `FINDINGS` must be followed by at least one
    /// `FINDING:` line. `FINDING:` lines may carry trailing prose after the
    /// third pipe; only the first three fields are significant, and the path
    /// may be empty for repo-wide findings.
    pub fn parse(text: &str) -> Result<ReviewReport, ReviewReportParseError> {
        let mut verdict: Option<&str> = None;
        let mut findings: Vec<Finding> = Vec::new();

        for raw_line in text.lines() {
            let line = raw_line.trim();
            if let Some(rest) = line.strip_prefix("VERDICT:") {
                let token = rest.trim();
                // Allow "VERDICT: CLEAN (because ...)" style prose.
                let token = token.split_whitespace().next().unwrap_or("");
                verdict = Some(token);
                continue;
            }
            if let Some(rest) = line.strip_prefix("FINDING:") {
                let body = rest.trim();
                if body.is_empty() {
                    continue;
                }
                let mut parts = body.splitn(3, '|');
                let severity = parts.next().unwrap_or("").trim().to_string();
                let path = parts.next().unwrap_or("").trim().to_string();
                let issue = parts.next().unwrap_or("").trim().to_string();
                findings.push(Finding {
                    severity,
                    path,
                    text: issue,
                });
            }
        }

        match verdict {
            None => Err(ReviewReportParseError::MissingVerdict),
            Some("CLEAN") => Ok(ReviewReport::Clean),
            Some("FINDINGS") => {
                if findings.is_empty() {
                    Err(ReviewReportParseError::FindingsWithoutItems)
                } else {
                    Ok(ReviewReport::Findings(findings))
                }
            }
            Some(other) => Err(ReviewReportParseError::UnknownVerdict(other.to_string())),
        }
    }

    pub fn is_clean(&self) -> bool {
        matches!(self, ReviewReport::Clean)
    }

    /// The open findings of this report (empty when `Clean`).
    pub fn findings(&self) -> &[Finding] {
        match self {
            ReviewReport::Clean => &[],
            ReviewReport::Findings(items) => items,
        }
    }
}

/// Compare two open-findings sets by fingerprint key. Stall is declared only
/// when a post-fix re-run reports the *same still-open findings* (no net
/// shrink) and adds nothing new. Comparing raw round fingerprints is wrong
/// because a post-fix re-run usually changes the fingerprint anyway.
pub fn fingerprint_of(findings: &[Finding]) -> HashSet<(String, String)> {
    findings.iter().map(|f| f.fingerprint_key()).collect()
}

/// True when `next` has no net shrink and adds nothing new relative to `prev`
/// across a post-fix re-run. That is the definition of "stuck" used by the
/// stall counter: the open set did not shrink and no new open finding appeared.
pub fn findings_stalled(prev: &[Finding], next: &[Finding]) -> bool {
    let prev_fp = fingerprint_of(prev);
    let next_fp = fingerprint_of(next);
    // No new findings appeared...
    let added_new = next_fp.iter().any(|k| !prev_fp.contains(k));
    // ...and the open set did not shrink.
    let shrank = next_fp.len() < prev_fp.len();
    !added_new && !shrank
}

/// Which phase the review loop is in. The first pass runs all six lenses in
/// order; the confirmation pass re-runs each lens once against the final code
/// state (cross-lens fixes can invalidate an earlier lens's clean verdict).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewLoopPhase {
    /// Running the six lenses in order for the first time.
    Lenses,
    /// Re-running lenses once against the final code state.
    Confirmation,
}

impl Default for ReviewLoopPhase {
    fn default() -> Self {
        ReviewLoopPhase::Lenses
    }
}

/// Persisted review-loop progress on a session. Mirrors `SessionImproveMode`
/// in that it is an optional, serializable loop marker. The accumulated
/// findings and verdicts live in the embedded `ReviewRecord`; resuming reloads
/// both together so a resumed loop cannot re-raise findings it already cleared.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewLoopState {
    /// Lens currently under review (or the next one to (re-)enter). `None`
    /// means the loop has not seeded a lens yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_lens: Option<ReviewLens>,
    /// Consecutive rounds reporting no new findings without converging. Bounded
    /// by `AutoReviewConfig.max_stalled_turns`.
    #[serde(default)]
    pub stall_turns: u32,
    /// Whether the loop has converged or been force-stopped. Once true, no more
    /// review turns are scheduled.
    #[serde(default)]
    pub finished: bool,
    /// Reason the loop finished (converged, stall cap, budget), for the digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Accumulated review record (rounds, findings, verdicts, can't-fix items,
    /// files touched). Embedded so reload restores the open-findings set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<ReviewRecord>,
    /// Which phase the loop is in.
    #[serde(default)]
    pub phase: ReviewLoopPhase,
    /// When true, the active reviewer is the post-fix re-check for
    /// `current_lens` (findings were reported, a fix was attempted, and the
    /// lens is being re-run to confirm the fix). A re-check verdict is what the
    /// stall counter compares against the previous open-findings set.
    #[serde(default)]
    pub awaiting_postfix_recheck: bool,
    /// Session id of the in-flight reviewer child session for the current lens,
    /// if any. Persisted (not just in-memory) so a reloaded session can keep
    /// polling the same reviewer instead of spawning a duplicate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_reviewer_id: Option<String>,
    /// Session id of the single reviewer session used for the entire review loop.
    /// When set, all lens reviews reuse this same session instead of spawning new
    /// ones, creating a truly single-window review experience.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_session_id: Option<String>,
    /// Whether the most recent fix turn actually changed files on disk. A
    /// *productive* (file-changing) re-check never counts against the stall
    /// cap, even if the open-findings set did not shrink: the fix may have been
    /// partial or adjacent, and the cap is meant to bound churn, not legitimate
    /// repair work.
    #[serde(default)]
    pub last_fix_touched_files: bool,
    /// Working-tree signature captured when the last fix turn was queued, used
    /// to detect whether the fix actually changed files at re-check poll time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_baseline_tree: Option<String>,
    /// Set when the loop converged but the review fixed files, so the work
    /// changed after the completion gates first passed. When true, the harness
    /// must re-run the completion gates exactly once against the post-fix state
    /// before declaring done (N2). A failing gate in that one re-run surfaces
    /// and stops; it must NOT re-enter the review loop (no gates↔review
    /// ping-pong).
    #[serde(default)]
    pub needs_gate_recheck: bool,
}

impl Default for ReviewLoopState {
    fn default() -> Self {
        Self {
            current_lens: None,
            stall_turns: 0,
            finished: false,
            finish_reason: None,
            record: None,
            phase: ReviewLoopPhase::Lenses,
            awaiting_postfix_recheck: false,
            active_reviewer_id: None,
            reviewer_session_id: None,
            last_fix_touched_files: false,
            fix_baseline_tree: None,
            needs_gate_recheck: false,
        }
    }
}

impl ReviewLoopState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the loop is currently in the final confirmation pass (re-running
    /// lenses against the final code state).
    pub fn phase_is_confirmation(&self) -> bool {
        self.phase == ReviewLoopPhase::Confirmation
    }

    /// Record that the loop finished with the given reason.
    pub fn finish_with(&mut self, reason: &str) {
        self.finished = true;
        self.finish_reason = Some(reason.to_string());
    }

    /// Append findings to the record's can't-fix list (deduped by fingerprint
    /// key) without advancing the lens. Used to surface still-open findings when
    /// the loop force-stops on the stall cap.
    pub fn add_cant_fix(&mut self, findings: Vec<Finding>) {
        let record = self.record.get_or_insert_with(ReviewRecord::default);
        for f in findings {
            if !record.cant_fix.iter().any(|c| c.fingerprint_key() == f.fingerprint_key()) {
                record.cant_fix.push(f);
            }
        }
    }
}

/// One review/fix round for a single lens.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewRound {
    pub lens: ReviewLens,
    /// Findings reported by the reviewer for this round.
    pub findings: Vec<Finding>,
    /// Whether the round converged (reviewer reported CLEAN).
    pub clean: bool,
    /// Open findings that the main session could not fix this round.
    pub cant_fix: Vec<Finding>,
    /// Files touched while fixing this round.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_touched: Vec<String>,
}

/// Per-session review record. Persisted as part of `ReviewLoopState` so a
/// resumed loop restores the full open-findings set.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReviewRecord {
    /// Every review/fix round, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rounds: Vec<ReviewRound>,
    /// Findings the main session reported it could not fix (blocked, missing
    /// creds, out of scope). These never count as a stall.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cant_fix: Vec<Finding>,
    /// Union of all files touched across fix rounds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_touched: Vec<String>,
    /// Final digest text emitted on convergence/force-stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

impl ReviewRecord {
    /// The currently open findings: the findings from the *latest* non-clean
    /// round for each lens, minus findings marked can't-fix, minus lenses whose
    /// latest round was clean. This is the set used to compute the stall
    /// fingerprint.
    ///
    /// We deliberately take the most recent round's findings per lens rather
    /// than the union of all rounds. A post-fix re-check reflects the
    /// post-fix state of the code: if a re-check reported `{A, B}` and the
    /// following re-check (after a fix) reports only `{A}`, the open set must
    /// shrink to `{A}`. Unioning across rounds would wrongly keep `{A, B}`
    /// open forever and treat a legitimate partial fix as a stall.
    pub fn open_findings(&self) -> Vec<Finding> {
        use std::collections::HashMap;
        // The latest non-clean round's findings per lens. Tracks the latest
        // *non-clean* round index per lens; a later clean round clears it.
        let mut latest_open_by_lens: HashMap<ReviewLens, Vec<Finding>> = HashMap::new();
        let mut latest_nonclean_index: HashMap<ReviewLens, usize> = HashMap::new();
        let mut latest_clean_index: HashMap<ReviewLens, usize> = HashMap::new();
        let mut cant_fix: Vec<Finding> = Vec::new();
        let mut cant_fix_keys: HashSet<(String, String)> = HashSet::new();

        for f in &self.cant_fix {
            let key = f.fingerprint_key();
            if !cant_fix_keys.contains(&key) {
                cant_fix_keys.insert(key);
                cant_fix.push(f.clone());
            }
        }

        for (i, round) in self.rounds.iter().enumerate() {
            if round.clean {
                // A clean round for a lens clears its open findings if it is the
                // most recent round for that lens. Can't-fix findings are
                // tracked separately and survive via `cant_fix`.
                latest_clean_index.insert(round.lens, i);
                latest_nonclean_index.remove(&round.lens);
                latest_open_by_lens.remove(&round.lens);
            } else {
                latest_nonclean_index.insert(round.lens, i);
                let slot = latest_open_by_lens.entry(round.lens).or_default();
                slot.clear();
                for f in &round.findings {
                    let key = f.fingerprint_key();
                    if cant_fix_keys.contains(&key) {
                        continue;
                    }
                    if !slot.iter().any(|o| o.fingerprint_key() == key) {
                        slot.push(f.clone());
                    }
                }
            }
            for f in &round.cant_fix {
                let key = f.fingerprint_key();
                if !cant_fix_keys.contains(&key) {
                    cant_fix_keys.insert(key);
                    cant_fix.push(f.clone());
                }
            }
        }

        // Drop any lens whose latest round was a clean one (it advanced past
        // the open findings we just recorded).
        for lens in latest_clean_index.keys() {
            if let Some(&ci) = latest_clean_index.get(lens) {
                if let Some(&ni) = latest_nonclean_index.get(lens) {
                    if ci > ni {
                        latest_open_by_lens.remove(lens);
                    }
                }
            }
        }

        let mut open: Vec<Finding> = latest_open_by_lens.into_values().flatten().collect();
        open.extend(cant_fix);
        open
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;

    #[test]
    fn parse_clean() {
        let report = ReviewReport::parse("Some prose\nVERDICT: CLEAN\nlooks good to me").unwrap();
        assert_eq!(report, ReviewReport::Clean);
        assert!(report.is_clean());
    }

    #[test]
    fn parse_findings() {
        let text = concat!(
            "VERDICT: FINDINGS\n",
            "FINDING: HIGH|src/foo.rs|off-by-one in loop bound\n",
            "FINDING: LOW|src/bar.rs|unused import\n",
        );
        let report = ReviewReport::parse(text).unwrap();
        match report {
            ReviewReport::Findings(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].severity, "HIGH");
                assert_eq!(items[0].path, "src/foo.rs");
                assert_eq!(items[0].text, "off-by-one in loop bound");
                assert_eq!(items[1].severity, "LOW");
            }
            _ => panic!("expected findings"),
        }
    }

    #[test]
    fn parse_tolerates_trailing_prose_after_pipe() {
        let text = "VERDICT: FINDINGS\nFINDING: MEDIUM|src/x.rs|leak | add drop\n";
        let report = ReviewReport::parse(text).unwrap();
        match report {
            ReviewReport::Findings(items) => {
                assert_eq!(items[0].text, "leak | add drop");
            }
            _ => panic!("expected findings"),
        }
    }

    #[test]
    fn parse_errors() {
        assert!(matches!(
            ReviewReport::parse("no verdict here"),
            Err(ReviewReportParseError::MissingVerdict)
        ));
        assert!(matches!(
            ReviewReport::parse("VERDICT: MAYBE"),
            Err(ReviewReportParseError::UnknownVerdict(_))
        ));
        assert!(matches!(
            ReviewReport::parse("VERDICT: FINDINGS\nnothing"),
            Err(ReviewReportParseError::FindingsWithoutItems)
        ));
    }

    #[test]
    fn fingerprint_and_stall() {
        let prev = vec![Finding::new("HIGH", "a.rs", "x")];
        // Same open finding, no shrink, nothing new -> stalled.
        let same = vec![Finding::new("HIGH", "a.rs", "x")];
        assert!(findings_stalled(&prev, &same));
        // New finding added (even if old still open) -> not stalled.
        let added = vec![Finding::new("HIGH", "a.rs", "x"), Finding::new("LOW", "b.rs", "y")];
        assert!(!findings_stalled(&prev, &added));
        // Open set shrank -> not stalled.
        let shrank: Vec<Finding> = vec![];
        assert!(!findings_stalled(&prev, &shrank));
    }

    #[test]
    fn lens_roundtrip() {
        for lens in ReviewLens::ALL {
            assert_eq!(ReviewLens::from_name(lens.name()), Some(lens));
        }
        assert_eq!(ReviewLens::from_name("nope"), None);
    }

    #[test]
    fn open_findings_clears_on_clean_and_respects_cant_fix() {
        let mut record = ReviewRecord::default();
        record.rounds.push(ReviewRound {
            lens: ReviewLens::Correctness,
            findings: vec![Finding::new("HIGH", "a.rs", "bug")],
            clean: false,
            cant_fix: vec![],
            files_touched: vec![],
        });
        // Open after one non-clean round.
        assert_eq!(record.open_findings().len(), 1);

        // A clean round for the same lens clears it.
        record.rounds.push(ReviewRound {
            lens: ReviewLens::Correctness,
            findings: vec![],
            clean: true,
            cant_fix: vec![],
            files_touched: vec![],
        });
        assert_eq!(record.open_findings().len(), 0);

        // A can't-fix finding stays open even if lens later cleans.
        record.rounds.push(ReviewRound {
            lens: ReviewLens::Security,
            findings: vec![Finding::new("HIGH", "sec.rs", "authz")],
            clean: false,
            cant_fix: vec![Finding::new("HIGH", "sec.rs", "authz")],
            files_touched: vec![],
        });
        record.rounds.push(ReviewRound {
            lens: ReviewLens::Security,
            findings: vec![],
            clean: true,
            cant_fix: vec![],
            files_touched: vec![],
        });
        // authz is in cant_fix, so it must remain open.
        assert_eq!(record.open_findings().len(), 1);
        assert_eq!(record.open_findings()[0].path, "sec.rs");
    }

    #[test]
    fn open_findings_uses_latest_nonclean_round() {
        // The open_findings set must reflect the *latest* non-clean round per lens,
        // not the union of all rounds. This is critical for stall detection: a
        // post-fix re-check must be able to shrink the open set when a fix
        // resolves some findings.
        let mut record = ReviewRecord::default();

        // First non-clean round: two findings.
        record.rounds.push(ReviewRound {
            lens: ReviewLens::Correctness,
            findings: vec![
                Finding::new("HIGH", "a.rs", "bug a"),
                Finding::new("LOW", "b.rs", "bug b"),
            ],
            clean: false,
            cant_fix: vec![],
            files_touched: vec![],
        });
        // Open set: both findings.
        assert_eq!(record.open_findings().len(), 2);

        // Second non-clean round: only bug a remains (partial fix).
        record.rounds.push(ReviewRound {
            lens: ReviewLens::Correctness,
            findings: vec![Finding::new("HIGH", "a.rs", "bug a")],
            clean: false,
            cant_fix: vec![],
            files_touched: vec![],
        });
        // Open set: only bug a remains (bug b fixed).
        assert_eq!(record.open_findings().len(), 1);
        assert_eq!(record.open_findings()[0].text, "bug a");

        // Clean round for the lens clears open findings.
        record.rounds.push(ReviewRound {
            lens: ReviewLens::Correctness,
            findings: vec![],
            clean: true,
            cant_fix: vec![],
            files_touched: vec![],
        });
        assert_eq!(record.open_findings().len(), 0);
    }

    #[test]
    fn loop_state_roundtrips_empty() {
        let json = serde_json::to_string(&ReviewLoopState::default()).unwrap();
        let back: ReviewLoopState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ReviewLoopState::default());
    }

    #[test]
    fn productive_fix_fields_roundtrip() {
        let mut state = ReviewLoopState::default();
        state.last_fix_touched_files = true;
        state.fix_baseline_tree = Some(" M src/foo.rs".to_string());
        let json = serde_json::to_string(&state).unwrap();
        let back: ReviewLoopState = serde_json::from_str(&json).unwrap();
        assert!(back.last_fix_touched_files);
        assert_eq!(back.fix_baseline_tree.as_deref(), Some(" M src/foo.rs"));
    }
}
