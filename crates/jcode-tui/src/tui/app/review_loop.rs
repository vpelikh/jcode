//! Review-loop engine: the post-completion review round state machine.
//!
//! This module is the orchestration brain. It is intentionally free of TUI/App
//! types so the whole state machine is unit-testable in isolation. The App glue
//! (`commands_review.rs` / `input.rs`) spawns the per-lens reviewer child
//! sessions, polls them for a `VERDICT`, and calls back into these functions.
//!
//! Loop shape (per the proposal at `docs/proposals/review-rounds.md`):
//!   1. After the completion gates pass, seed the loop on the session.
//!   2. For each lens in order: spawn an independent read-only reviewer, parse
//!      its `ReviewReport`, and either (CLEAN) advance to the next lens or
//!      (FINDINGS) queue a synthetic fix turn in the parent and re-run the same
//!      lens on the post-fix code (P6).
//!   3. Stall is bounded by `max_stalled_turns` and computed on the
//!      open-findings *set* across a post-fix re-run (a re-raised finding is not
//!      a new one).
//!   4. After all six lenses report CLEAN, run a final confirmation pass that
//!      re-runs each lens once against the final code state; only an all-CLEAN
//!      final pass counts as convergence.
//!   5. `can't-fix` findings never count as a stall; they stay open and surface
//!      in the digest.

use jcode_session_types::{
    findings_stalled, Finding, ReviewLens, ReviewLoopPhase, ReviewLoopState, ReviewRecord,
    ReviewReport, ReviewRound,
};

/// What the harness should do next after a `step`/`apply_verdict` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewLoopAction {
    /// No review loop is active (or it is finished). Do nothing.
    None,
    /// Spawn a fresh independent reviewer for this lens and poll it for a verdict.
    SpawnReviewer(ReviewLens),
    /// The reviewer reported findings; inject this synthetic fix turn into the
    /// parent session, then re-run the *same* lens on the post-fix code.
    QueueFixTurn(Vec<Finding>),
    /// All lenses + the final confirmation pass are clean. Emit the digest.
    Converged,
    /// The stall cap was hit without convergence. Emit the digest (stopped).
    Stalled,
}

/// Seed a review loop on a session that has just had its todos complete.
///
/// Idempotent while a loop is in progress: if a loop is already active it is a
/// no-op. If a *previous* loop has finished (manual `/review-loop start` after
/// a completed run, or a fresh completion triggering the auto loop again), the
/// finished state is cleared so a new loop can run from the first lens.
pub fn enter_review_loop(state: &mut ReviewLoopState) {
    if state.finished {
        *state = ReviewLoopState::new();
    }
    if state.current_lens.is_none() && !state.phase_is_confirmation() {
        state.current_lens = Some(ReviewLens::ALL[0]);
    }
    if state.record.is_none() {
        state.record = Some(ReviewRecord::default());
    }
}

/// True when the session currently has an active, unfinished review loop.
pub fn is_review_loop_active(state: &ReviewLoopState) -> bool {
    !state.finished && state.current_lens.is_some()
}

/// Decide the next harness action when there is no in-flight reviewer for the
/// current lens. Call this from turn-end followups when
/// `app.active_review_reviewer_id` is `None`.
pub fn next_action(state: &mut ReviewLoopState) -> ReviewLoopAction {
    if state.finished {
        return ReviewLoopAction::None;
    }
    // Seed the first lens if none is set.
    if state.current_lens.is_none() {
        state.current_lens = Some(ReviewLens::ALL[0]);
    }
    let lens = state.current_lens.expect("set above");
    // If we are mid re-check for the same lens, or starting fresh, spawn.
    ReviewLoopAction::SpawnReviewer(lens)
}

/// Apply a reviewer verdict for the current lens and decide what happens next.
///
/// `max_stalled_turns == 0` means unlimited (never force-stop on stall).
pub fn apply_verdict(
    state: &mut ReviewLoopState,
    report: &ReviewReport,
    max_stalled_turns: u32,
) -> ReviewLoopAction {
    if state.finished {
        return ReviewLoopAction::None;
    }
    let lens = match state.current_lens {
        Some(l) => l,
        None => return next_action(state),
    };
    let record = state.record.get_or_insert_with(ReviewRecord::default);

    match report {
        ReviewReport::Clean => {
            record.rounds.push(ReviewRound {
                lens,
                findings: Vec::new(),
                clean: true,
                cant_fix: Vec::new(),
                files_touched: Vec::new(),
            });
            state.awaiting_postfix_recheck = false;
            state.stall_turns = 0;
            advance_lens(state)
        }
        ReviewReport::Findings(findings) => {
            let prev_open = record.open_findings();
            record.rounds.push(ReviewRound {
                lens,
                findings: findings.clone(),
                clean: false,
                cant_fix: Vec::new(),
                files_touched: Vec::new(),
            });
            let new_open = record.open_findings();

            // Stall is only counted on a post-fix re-run for the same lens (P6):
            // the open set neither shrank nor gained a new finding. A first-pass
            // findings report is always "productive" (the main session will fix).
            // Critically, a re-check whose fix actually changed files on disk is
            // *productive* repair work and must NEVER count toward the stall cap:
            // the cap is meant to bound churn (re-raising the same findings with no
            // progress), not legitimate fixes that happened to be partial or
            // adjacent. Only a file-touching re-check that still reports the same
            // open findings is a true stall.
            let is_stall = state.awaiting_postfix_recheck
                && !state.last_fix_touched_files
                && findings_stalled(&prev_open, &new_open);
            if is_stall {
                state.stall_turns = state.stall_turns.saturating_add(1);
                if max_stalled_turns != 0 && state.stall_turns >= max_stalled_turns {
                    state.finished = true;
                    state.finish_reason = Some("stall_cap".to_string());
                    // The still-open findings could not be resolved within the
                    // cap. Surface them in the digest as can't-fix so the user
                    // sees exactly what blocked convergence.
                    for f in &new_open {
                        state.add_cant_fix(vec![f.clone()]);
                    }
                    return ReviewLoopAction::Stalled;
                }
                // Stalled but under cap: re-spawn the reviewer for another re-check
                // (do not queue another fix turn, the fix did not resolve it).
                return ReviewLoopAction::SpawnReviewer(lens);
            } else {
                // Productive round (first pass, new findings, or a file-changing
                // fix that did not fully clear the set): reset the cap.
                state.stall_turns = 0;
            }

            // Queue the fix turn and mark that the next spawn for this lens is a
            // post-fix re-check.
            state.awaiting_postfix_recheck = true;
            ReviewLoopAction::QueueFixTurn(findings.clone())
        }
    }
}

/// Mark findings for the current lens as "can't fix" (blocked / out of scope).
/// These never count as a stall; the loop advances to the next lens with the
/// finding kept open in the digest.
pub fn mark_cant_fix(state: &mut ReviewLoopState, findings: Vec<Finding>) -> ReviewLoopAction {
    if state.finished {
        return ReviewLoopAction::None;
    }
    let lens = match state.current_lens {
        Some(l) => l,
        None => return next_action(state),
    };
    let record = state.record.get_or_insert_with(ReviewRecord::default);
    for f in findings {
        if !record.cant_fix.iter().any(|c| c.fingerprint_key() == f.fingerprint_key()) {
            record.cant_fix.push(f);
        }
    }
    record.rounds.push(ReviewRound {
        lens,
        findings: Vec::new(),
        clean: false,
        cant_fix: record.cant_fix.clone(),
        files_touched: Vec::new(),
    });
    state.awaiting_postfix_recheck = false;
    state.stall_turns = 0;
    advance_lens(state)
}

/// Record files the main session touched while fixing the current lens.
pub fn record_fix_files(state: &mut ReviewLoopState, files: Vec<String>) {
    let Some(record) = state.record.as_mut() else {
        return;
    };
    for f in files {
        if !record.files_touched.contains(&f) {
            record.files_touched.push(f);
        }
    }
}

fn advance_lens(state: &mut ReviewLoopState) -> ReviewLoopAction {
    let confirm = state.phase_is_confirmation();
    let idx = lens_index(state.current_lens);
    // Find the next lens in ALL order (first pass) or the next *unconfirmed*
    // lens (confirmation pass).
    if confirm {
        // Confirmation pass: walk ALL from the current index+1; if we reach the
        // end, the whole pass was clean -> converge.
        let next = ReviewLens::ALL
            .iter()
            .copied()
            .skip(idx + 1)
            .next();
        match next {
            Some(nl) => {
                state.current_lens = Some(nl);
                ReviewLoopAction::SpawnReviewer(nl)
            }
            None => {
                state.finished = true;
                state.finish_reason = Some("converged".to_string());
                ReviewLoopAction::Converged
            }
        }
    } else {
        let next = ReviewLens::ALL.iter().copied().skip(idx + 1).next();
        match next {
            Some(nl) => {
                state.current_lens = Some(nl);
                ReviewLoopAction::SpawnReviewer(nl)
            }
            None => {
                // First pass complete for all lenses. Enter confirmation pass:
                // re-run every lens once against the final code state. Only an
                // all-CLEAN confirmation pass counts as convergence.
                state.phase = ReviewLoopPhase::Confirmation;
                state.current_lens = Some(ReviewLens::ALL[0]);
                ReviewLoopAction::SpawnReviewer(ReviewLens::ALL[0])
            }
        }
    }
}

fn lens_index(lens: Option<ReviewLens>) -> usize {
    match lens {
        Some(l) => ReviewLens::ALL.iter().position(|x| *x == l).unwrap_or(0),
        None => 0,
    }
}

/// Build the end-of-loop digest summarizing rounds, findings fixed, can't-fix
/// items, and files touched.
pub fn build_digest(state: &ReviewLoopState) -> String {
    let record = match &state.record {
        Some(r) => r,
        None => return "Review complete.".to_string(),
    };
    let total_rounds = record.rounds.len();
    let findings_reported: usize = record.rounds.iter().map(|r| r.findings.len()).sum();
    let mut out = String::from("## Review rounds complete\n\n");
    out.push_str(&format!("- Lenses reviewed: {}\n", ReviewLens::ALL.len()));
    out.push_str(&format!("- Review/fix rounds: {}\n", total_rounds));
    out.push_str(&format!("- Findings reported: {}\n", findings_reported));
    out.push_str(&format!("- Files touched: {}\n", record.files_touched.len()));
    out.push_str(&format!(
        "- Last fix changed files: {}\n",
        state.last_fix_touched_files
    ));
    if !record.cant_fix.is_empty() {
        out.push_str("\n### Can't fix\n");
        for f in &record.cant_fix {
            out.push_str(&format!("- [{}] {}: {}\n", f.severity, f.path, f.text));
        }
    }
    out.push_str(&format!(
        "\nFinish reason: {}\n",
        state.finish_reason.as_deref().unwrap_or("unknown")
    ));
    out
}

#[cfg(test)]
mod review_loop_tests {
    use super::*;
    use jcode_session_types::ReviewReport;

    fn clean_state() -> ReviewLoopState {
        let mut s = ReviewLoopState::new();
        enter_review_loop(&mut s);
        s
    }

    fn findings(text: &str) -> ReviewReport {
        ReviewReport::Findings(vec![Finding::new("HIGH", "a.rs", text)])
    }

    #[test]
    fn enter_seeds_first_lens() {
        let mut s = ReviewLoopState::new();
        enter_review_loop(&mut s);
        assert_eq!(s.current_lens, Some(ReviewLens::Correctness));
        assert!(!s.finished);
    }

    #[test]
    fn enter_restarts_after_finished() {
        // A finished loop must be re-seedable: a manual `/review-loop start`
        // (or a fresh auto entry) after convergence should clear finished and
        // restart from the first lens, not be a silent no-op.
        let mut s = clean_state();
        // Drive to convergence.
        next_action(&mut s);
        apply_verdict(&mut s, &ReviewReport::Clean, 3);
        // Finish it off so it is `finished`.
        s.finish_with("converged");
        assert!(s.finished);

        enter_review_loop(&mut s);
        assert!(!s.finished);
        assert_eq!(s.current_lens, Some(ReviewLens::Correctness));
        assert!(s.record.is_some());
    }

    #[test]
    fn clean_lens_advances_through_all_then_confirmation() {
        let mut s = clean_state();
        // First pass: 6 clean lenses -> enters confirmation pass.
        for _ in 0..6 {
            let lens = s.current_lens.unwrap();
            match next_action(&mut s) {
                ReviewLoopAction::SpawnReviewer(l) => assert_eq!(l, lens),
                other => panic!("expected spawn, got {other:?}"),
            }
            match apply_verdict(&mut s, &ReviewReport::Clean, 3) {
                ReviewLoopAction::SpawnReviewer(_) | ReviewLoopAction::Converged => {}
                other => panic!("expected spawn/converged, got {other:?}"),
            }
        }
        // After the 6th clean, the first-pass ends and confirmation pass begins.
        assert!(s.phase_is_confirmation());
        // Confirmation pass: 6 more clean lenses -> converge.
        for i in 0..6 {
            let lens = s.current_lens.unwrap();
            assert_eq!(
                next_action(&mut s),
                ReviewLoopAction::SpawnReviewer(lens)
            );
            match apply_verdict(&mut s, &ReviewReport::Clean, 3) {
                ReviewLoopAction::SpawnReviewer(_) if i < 5 => {}
                ReviewLoopAction::Converged if i == 5 => {}
                other => panic!("unexpected at confirmation step {i}: {other:?}"),
            }
        }
        assert!(s.finished);
        assert_eq!(s.finish_reason.as_deref(), Some("converged"));
    }

    #[test]
    fn stall_rechecks_without_queueing_another_fix_turn() {
        // Stalled (same open findings, no net shrink) but under the stall cap
        // must re-spawn the reviewer for another re-check, NOT queue a second
        // fix turn (the fix did not resolve the finding).
        let mut s = clean_state();
        next_action(&mut s);
        apply_verdict(&mut s, &findings("bug a"), 3);
        // Post-fix re-check re-raises the same finding -> stalled (cap=3).
        next_action(&mut s);
        match apply_verdict(&mut s, &findings("bug a"), 3) {
            ReviewLoopAction::SpawnReviewer(ReviewLens::Correctness) => {}
            other => panic!("expected re-spawn for stalled re-check, got {other:?}"),
        }
        // Still awaiting the post-fix re-check (no convergence yet).
        assert!(s.awaiting_postfix_recheck);
        assert_eq!(s.stall_turns, 1);
        assert!(!s.finished);
    }

    #[test]
    fn findings_queue_fix_then_recheck_then_clean() {
        let mut s = clean_state();
        assert_eq!(next_action(&mut s), ReviewLoopAction::SpawnReviewer(ReviewLens::Correctness));
        // Reviewer finds a bug.
        match apply_verdict(&mut s, &findings("off by one"), 3) {
            ReviewLoopAction::QueueFixTurn(fs) => {
                assert_eq!(fs.len(), 1);
                assert!(s.awaiting_postfix_recheck);
            }
            other => panic!("expected fix turn, got {other:?}"),
        }
        // Main session fixes; loop re-runs the same lens (post-fix re-check).
        assert_eq!(next_action(&mut s), ReviewLoopAction::SpawnReviewer(ReviewLens::Correctness));
        // Re-check is clean -> advance to next lens.
        match apply_verdict(&mut s, &ReviewReport::Clean, 3) {
            ReviewLoopAction::SpawnReviewer(ReviewLens::EdgesErrors) => {}
            other => panic!("expected edges lens, got {other:?}"),
        }
        assert_eq!(s.stall_turns, 0);
    }

    #[test]
    fn stall_cap_force_stops() {
        let mut s = clean_state();
        next_action(&mut s);
        apply_verdict(&mut s, &findings("bug a"), 2);
        // Re-check re-raises the same finding (no fix) -> stall 1.
        next_action(&mut s);
        apply_verdict(&mut s, &findings("bug a"), 2);
        // Re-check re-raises again -> stall 2 == cap -> Stalled.
        next_action(&mut s);
        match apply_verdict(&mut s, &findings("bug a"), 2) {
            ReviewLoopAction::Stalled => {}
            other => panic!("expected Stalled, got {other:?}"),
        }
        assert!(s.finished);
        assert_eq!(s.finish_reason.as_deref(), Some("stall_cap"));
    }

    #[test]
    fn productive_file_changing_recheck_never_counts_toward_stall_cap() {
        // A re-check whose fix actually changed files on disk is productive
        // repair work: even if the open-findings set did not shrink, it must
        // not increment the stall counter. The loop should queue another fix
        // turn (try again) rather than force-stopping at the cap.
        let mut s = clean_state();
        next_action(&mut s);
        apply_verdict(&mut s, &findings("bug a"), 2);
        // Simulate the fix turn having touched files (positive baseline diff).
        s.last_fix_touched_files = true;
        // Post-fix re-check re-raises the same finding, but files changed.
        next_action(&mut s);
        match apply_verdict(&mut s, &findings("bug a"), 2) {
            ReviewLoopAction::QueueFixTurn(_) => {}
            other => panic!("expected another fix turn, got {other:?}"),
        }
        // Stall counter must remain at zero for a productive round.
        assert_eq!(s.stall_turns, 0);
        assert!(!s.finished);
        // Even after repeatedly re-raising with file changes, never force-stops.
        for _ in 0..5 {
            s.last_fix_touched_files = true;
            next_action(&mut s);
            match apply_verdict(&mut s, &findings("bug a"), 2) {
                ReviewLoopAction::QueueFixTurn(_) => {}
                other => panic!("expected another fix turn, got {other:?}"),
            }
        }
        assert_eq!(s.stall_turns, 0);
        assert!(!s.finished);
    }

    #[test]
    fn stall_cap_only_counts_when_no_file_change() {
        // Without a file change (last_fix_touched_files = false), the same
        // re-raised finding still counts as a stall and force-stops at cap.
        let mut s = clean_state();
        s.last_fix_touched_files = false;
        next_action(&mut s);
        apply_verdict(&mut s, &findings("bug a"), 1);
        next_action(&mut s);
        match apply_verdict(&mut s, &findings("bug a"), 1) {
            ReviewLoopAction::Stalled => {}
            other => panic!("expected Stalled, got {other:?}"),
        }
        assert_eq!(s.finish_reason.as_deref(), Some("stall_cap"));
    }

    #[test]
    fn stall_cap_unlimited_when_zero() {
        let mut s = clean_state();
        next_action(&mut s);
        apply_verdict(&mut s, &findings("bug"), 0);
        next_action(&mut s);
        apply_verdict(&mut s, &findings("bug"), 0);
        next_action(&mut s);
        apply_verdict(&mut s, &findings("bug"), 0);
        // max_stalled_turns == 0 means never force-stop.
        assert!(!s.finished);
    }

    #[test]
    fn stall_cap_surfaces_open_findings_as_cant_fix() {
        let mut s = clean_state();
        next_action(&mut s);
        apply_verdict(&mut s, &findings("hard bug"), 2);
        // Post-fix re-check re-raises the same finding (no fix) -> stall 1.
        next_action(&mut s);
        apply_verdict(&mut s, &findings("hard bug"), 2);
        // Second stall -> cap hit -> Stalled, and the open finding is surfaced.
        next_action(&mut s);
        match apply_verdict(&mut s, &findings("hard bug"), 2) {
            ReviewLoopAction::Stalled => {}
            other => panic!("expected Stalled, got {other:?}"),
        }
        assert!(s.finished);
        let rec = s.record.as_ref().unwrap();
        assert_eq!(rec.cant_fix.len(), 1);
        assert_eq!(rec.cant_fix[0].text, "hard bug");
    }

    #[test]
    fn cant_fix_advances_without_stall() {
        let mut s = clean_state();
        next_action(&mut s);
        apply_verdict(&mut s, &findings("hard bug"), 3);
        next_action(&mut s);
        // Main session cannot fix -> mark can't-fix and advance.
        match mark_cant_fix(&mut s, vec![Finding::new("HIGH", "a.rs", "hard bug")]) {
            ReviewLoopAction::SpawnReviewer(ReviewLens::EdgesErrors) => {}
            other => panic!("expected advance to edges lens, got {other:?}"),
        }
        assert_eq!(s.stall_turns, 0);
        // The can't-fix finding is in the record's open set.
        let rec = s.record.as_ref().unwrap();
        assert_eq!(rec.cant_fix.len(), 1);
        assert_eq!(rec.open_findings().len(), 1);
    }

    #[test]
    fn digest_reports_rounds_and_cant_fix() {
        let mut s = clean_state();
        next_action(&mut s);
        apply_verdict(&mut s, &findings("x"), 3);
        record_fix_files(&mut s, vec!["a.rs".to_string(), "b.rs".to_string()]);
        mark_cant_fix(&mut s, vec![Finding::new("HIGH", "a.rs", "x")]);
        let digest = build_digest(&s);
        assert!(digest.contains("Review/fix rounds"));
        assert!(digest.contains("Can't fix"));
        assert!(digest.contains("a.rs"));
    }
}
