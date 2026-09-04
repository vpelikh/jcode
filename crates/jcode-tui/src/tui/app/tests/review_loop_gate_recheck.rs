// Tests for the N2 completion-gate re-check after the review loop converges.
//
// When the review loop converges and fixed files (so the work changed after the
// completion gates first passed), the gates are re-run exactly once against the
// post-fix state. A failing gate in that one re-run surfaces and stops — it never
// re-enters the review loop (no gates↔review ping-pong).
//
// The engine-level flag that decides *whether* to re-check is covered by
// `review_loop_tests` (convergence_with_fix_requests_one_gate_recheck /
// convergence_without_fix_does_not_request_gate_recheck). These tests pin the
// observable outcome: `finish_review_loop` surfaces a failure and leaves the
// loop finished, so nothing schedules another review round.

#[test]
fn gate_recheck_that_fails_surfaces_and_finishes_closed() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let session_id = app.session_id().to_string();

        // A completed todo with NO completion confidence, but a passing goal.
        // Ownership already holds; only the confidence gate trips, so the
        // surfaced reason is the confidence branch specifically.
        save_completed_todo(&session_id, None);
        crate::todo::save_goals(&session_id, &[passing_goal()]).expect("save goal");

        // A converged loop that also fixed files must request the re-check.
        let mut state = jcode_session_types::ReviewLoopState::new();
        state.finished = true;
        state.current_lens = Some(jcode_session_types::ReviewLens::Correctness);

        super::commands_review::finish_review_loop(&mut app, &mut state, true);

        // The loop is left finished (never re-enters review), with the reason
        // recording that the post-review gate re-check disagreed.
        assert!(state.finished);
        let reason = state.finish_reason.as_deref().unwrap_or_default();
        assert!(
            reason.starts_with("converged_gate_recheck_failed"),
            "expected gate-recheck-failed reason, got {reason:?}"
        );
        assert!(
            reason.ends_with("completion confidence needs re-validation"),
            "expected the confidence gate reason, got {reason:?}"
        );
        // The surfaced message tells the user to review the result themselves.
        assert!(
            app.display_messages().iter().any(|msg| {
                msg.content
                    .contains("Review fixed files, but the completion assessment now disagrees")
            }),
            "expected a surfacing message when the gate re-check fails"
        );
        // The persisted session loop is finished too, so nothing drives another
        // review round on a later turn-end.
        assert!(
            app.session
                .review_loop
                .as_ref()
                .map(|s| s.finished)
                .unwrap_or(false),
            "session review loop must be finished after a failed gate re-check"
        );
    });
}

#[test]
fn gate_recheck_that_fails_on_ownership_surfaces_ownership_reason() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let session_id = app.session_id().to_string();

        // A completed todo, but NO goal recorded for its group and no completion
        // confidence. Ownership's `completed_groups_have_sufficient_delivery`
        // needs a goal for every completed group, so it holds false here.
        save_completed_todo(&session_id, Some(100)); // valid confidence, but no goal
        // Intentionally no save_goals.

        let mut state = jcode_session_types::ReviewLoopState::new();
        state.finished = true;
        state.current_lens = Some(jcode_session_types::ReviewLens::Correctness);

        super::commands_review::finish_review_loop(&mut app, &mut state, true);

        assert!(state.finished);
        let reason = state.finish_reason.as_deref().unwrap_or_default();
        assert!(
            reason.starts_with("converged_gate_recheck_failed"),
            "expected gate-recheck-failed reason, got {reason:?}"
        );
        assert!(
            reason.ends_with("end-to-end delivery assessment no longer holds"),
            "expected the ownership gate reason, got {reason:?}"
        );
        assert!(
            app.display_messages().iter().any(|msg| {
                msg.content
                    .contains("completion assessment now disagrees")
            }),
            "expected the failure to be surfaced for the ownership gate"
        );
    });
}

#[test]
fn gate_recheck_that_fails_on_confidence_spike_surfaces() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let session_id = app.session_id().to_string();

        // A completed todo with a valid completion confidence but a two-level
        // confidence *spike* in its history. `todo_confidence_summary` reports
        // this when `completion_confidence_needs_validation` is false, and the
        // re-check must still treat it as a confidence gate failure.
        save_spiking_completed_todo(&session_id);
        crate::todo::save_goals(&session_id, &[passing_goal()]).expect("save goal");

        let mut state = jcode_session_types::ReviewLoopState::new();
        state.finished = true;
        state.current_lens = Some(jcode_session_types::ReviewLens::Correctness);

        super::commands_review::finish_review_loop(&mut app, &mut state, true);

        assert!(state.finished);
        let reason = state.finish_reason.as_deref().unwrap_or_default();
        assert!(
            reason.starts_with("converged_gate_recheck_failed"),
            "expected gate-recheck-failed reason, got {reason:?}"
        );
        assert!(
            reason.ends_with("completion confidence needs re-validation"),
            "expected the confidence gate reason for a spike, got {reason:?}"
        );
        assert!(
            app.display_messages().iter().any(|msg| {
                msg.content
                    .contains("completion assessment now disagrees")
            }),
            "expected the spike failure to be surfaced"
        );
    });
}

#[test]
fn gate_recheck_that_passes_finishes_cleanly() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let session_id = app.session_id().to_string();

        // A completed todo with valid completion confidence + a passing goal so
        // both gates hold.
        save_completed_todo(&session_id, Some(100));
        crate::todo::save_goals(&session_id, &[passing_goal()]).expect("save goal");

        // Sanity: the plumbing used by finish_review_loop must actually see the
        // goal and todos we just persisted, or this test would be testing the
        // temp-home plumbing rather than the gate re-check branch.
        let loaded_goals = crate::todo::load_goals(&session_id).unwrap_or_default();
        assert!(
            !loaded_goals.is_empty(),
            "finish_review_loop's goal loader must find the saved goal"
        );

        let mut state = jcode_session_types::ReviewLoopState::new();
        state.finished = true;
        state.current_lens = Some(jcode_session_types::ReviewLens::Correctness);

        super::commands_review::finish_review_loop(&mut app, &mut state, true);

        assert!(state.finished);
        assert_eq!(state.finish_reason.as_deref(), Some("converged"));
        // No failure-surfacing message.
        assert!(
            !app.display_messages().iter().any(|msg| {
                msg.content
                    .contains("completion assessment now disagrees")
            }),
            "a passing gate re-check must not surface a failure"
        );
    });
}

#[test]
fn finish_without_gate_recheck_emits_digest_and_does_not_touch_gates() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();

        // No todos persisted at all, and the flag is false (converged without
        // touching files, or stalled). `finish_review_loop` must not evaluate
        // the gates or surface anything - it just emits the digest and marks
        // the loop done.
        let mut state = jcode_session_types::ReviewLoopState::new();
        state.finished = true;
        state.current_lens = Some(jcode_session_types::ReviewLens::Correctness);

        super::commands_review::finish_review_loop(&mut app, &mut state, false);

        assert!(state.finished);
        // No failure-surfacing message and no spurious gate evaluation.
        assert!(
            !app.display_messages().iter().any(|msg| {
                msg.content
                    .contains("completion assessment now disagrees")
            }),
            "a no-re-check finish must not surface a gate failure"
        );
        assert!(
            app.session
                .review_loop
                .as_ref()
                .map(|s| s.finished)
                .unwrap_or(false),
            "session review loop must be marked finished"
        );
        // The digest was still emitted (the end-of-loop summary). With a bare
        // state (no record) `build_digest` returns the "Review complete." form.
        assert!(
            app.display_messages()
                .iter()
                .any(|msg| msg.content.contains("Review complete.")),
            "the digest must still be emitted without a gate re-check"
        );
    });
}

/// Persist a single completed todo for the given session.
///
/// When completion confidence is present, the recorded history climbs one level
/// per step (`Validated` -> `Verified`), which is *not* a confidence spike, so
/// the confidence gate alone decides the outcome.
fn save_completed_todo(session_id: &str, completion_confidence: Option<u8>) {
    crate::todo::save_todos(
        session_id,
        &[crate::todo::TodoItem {
            group: None,
            id: "todo-1".to_string(),
            content: "Reviewed work".to_string(),
            status: "completed".to_string(),
            priority: "high".to_string(),
            blocked_by: Vec::new(),
            assigned_to: None,
            confidence: None,
            completion_confidence: completion_confidence.map(|score| {
                crate::todo::ConfidenceState::from_legacy_score(score)
            }),
            confidence_history: match completion_confidence {
                // Validated -> Verified: a single-level step, no spike.
                Some(_) => vec![
                    crate::todo::ConfidenceState::from_legacy_score(96),
                    crate::todo::ConfidenceState::from_legacy_score(100),
                ],
                None => Vec::new(),
            },
        }],
    )
    .expect("save todos");
}

/// A completed todo whose recorded confidence history jumps two levels
/// (`Plausible` -> `Verified`), which the spike detector flags.
fn save_spiking_completed_todo(session_id: &str) {
    crate::todo::save_todos(
        session_id,
        &[crate::todo::TodoItem {
            group: None,
            id: "todo-1".to_string(),
            content: "Reviewed work".to_string(),
            status: "completed".to_string(),
            priority: "high".to_string(),
            blocked_by: Vec::new(),
            assigned_to: None,
            confidence: None,
            completion_confidence: Some(crate::todo::ConfidenceState::from_legacy_score(100)),
            confidence_history: vec![
                crate::todo::ConfidenceState::from_legacy_score(90), // Plausible
                crate::todo::ConfidenceState::from_legacy_score(100), // Verified (2-level jump)
            ],
        }],
    )
    .expect("save todos");
}

/// A goal that passes every ownership sub-check for a completed `group: None`
/// group.
fn passing_goal() -> crate::todo::TodoGoal {
    crate::todo::TodoGoal {
        group: None,
        delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
        autonomy: Some(crate::todo::Autonomy::NecessaryFollowthrough),
        iteration_maturity: Some(crate::todo::IterationMaturity::OutcomeReached),
        closed_feedback_loop: Some(crate::todo::FeedbackLoopState::Closed),
        feedback_loop: Some("verify completed work".to_string()),
        feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::Representative),
        feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::MainPaths),
        feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
        ..Default::default()
    }
}

// --- Integration through the real turn-end boundary ---
//
// The two tests above call `finish_review_loop` directly. These next two drive
// the *producer*: `step_review_loop`, which is what `schedule_turn_end_followups`
// invokes every turn while the loop is active. They exercise the real path —
// poll the persisted reviewer child session, parse its `VERDICT`, run the engine
// through `apply_verdict`/`advance_lens`, converge, derive whether the review
// changed files, and then evaluate the completion gates — rather than handing a
// finished state straight to the digest/emit stage.

/// Drive the review engine to one step short of convergence: all lenses clean on
/// the first pass, then all but the last lens clean on the confirmation pass,
/// leaving `current_lens` at the final confirmation lens with `record` recording
/// the files a fix touched. This is the exact state the real loop is in right
/// before the reviewer returns the last CLEAN verdict.
fn drive_to_final_confirmation_lens(
    touched_files: Vec<String>,
) -> jcode_session_types::ReviewLoopState {
    use super::review_loop::ReviewLoopAction;
    let mut state = jcode_session_types::ReviewLoopState::new();
    super::review_loop::enter_review_loop(&mut state);
    for file in touched_files {
        super::review_loop::record_fix_files(&mut state, vec![file]);
    }
    // First pass: 6 clean lenses -> confirmation pass.
    for _ in 0..6 {
        let lens = state.current_lens.unwrap();
        assert_eq!(
            super::review_loop::next_action(&mut state),
            ReviewLoopAction::SpawnReviewer(lens)
        );
        super::review_loop::apply_verdict(&mut state, &jcode_session_types::ReviewReport::Clean, 3);
    }
    assert!(state.phase_is_confirmation());
    // Confirmation pass: clean the first 5 lenses, staying one short of the 6th.
    for _ in 0..5 {
        let lens = state.current_lens.unwrap();
        assert_eq!(
            super::review_loop::next_action(&mut state),
            ReviewLoopAction::SpawnReviewer(lens)
        );
        super::review_loop::apply_verdict(&mut state, &jcode_session_types::ReviewReport::Clean, 3);
    }
    assert!(!state.finished);
    state
}

#[test]
fn step_review_loop_converges_with_fix_and_gate_recheck_surfaces_on_failure() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let parent_session_id = app.session_id().to_string();

        // A completed todo with no completion confidence, but a passing goal.
        // Only the confidence gate trips when the post-review re-check runs.
        save_completed_todo(&parent_session_id, None);
        crate::todo::save_goals(&parent_session_id, &[passing_goal()]).expect("save goal");

        // Move the engine to the final confirmation lens. The recorded touched
        // file means the review's fix changed the work under review.
        let mut state = drive_to_final_confirmation_lens(vec!["fixed.rs".to_string()]);

        // A real reviewer child session whose last message is the final CLEAN
        // verdict. `poll_loop_reviewer` loads and parses this.
        let mut reviewer = crate::session::Session::create(None, None);
        let reviewer_id = reviewer.id.clone();
        reviewer.add_message_with_display_role(
            crate::message::Role::User,
            vec![crate::message::ContentBlock::Text {
                text: "Reviewing finished work.\nVERDICT: CLEAN".to_string(),
                cache_control: None,
            }],
            None,
        );
        reviewer.save().expect("save reviewer session");

        state.active_reviewer_id = Some(reviewer_id);
        app.session.review_loop = Some(state);

        // The real turn-end integration point.
        let followup = super::commands_review::step_review_loop(&mut app);

        // step_review_loop emits the digest + failure surfacing and stops; it
        // does not schedule another review round.
        assert!(!followup);
        let state = app.session.review_loop.as_ref().unwrap();
        assert!(state.finished);
        let reason = state.finish_reason.as_deref().unwrap_or_default();
        assert!(
            reason.starts_with("converged_gate_recheck_failed"),
            "expected gate-recheck-failed reason through step_review_loop, got {reason:?}"
        );
        assert!(
            reason.ends_with("completion confidence needs re-validation"),
            "expected the confidence gate reason through step_review_loop, got {reason:?}"
        );
        // The loop is finished above, so the one-shot re-check cannot re-trigger.
        assert!(
            app.display_messages().iter().any(|msg| {
                msg.content
                    .contains("Review fixed files, but the completion assessment now disagrees")
            }),
            "expected the failure to be surfaced through the real turn-end path"
        );
    });
}

#[test]
fn step_review_loop_converges_with_fix_and_gate_recheck_passes_cleanly() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let parent_session_id = app.session_id().to_string();

        // Completed todo with valid confidence + a fully passing goal so both
        // post-review gates hold.
        save_completed_todo(&parent_session_id, Some(100));
        crate::todo::save_goals(&parent_session_id, &[passing_goal()]).expect("save goal");

        let mut state = drive_to_final_confirmation_lens(vec!["fixed.rs".to_string()]);

        let mut reviewer = crate::session::Session::create(None, None);
        let reviewer_id = reviewer.id.clone();
        reviewer.add_message_with_display_role(
            crate::message::Role::User,
            vec![crate::message::ContentBlock::Text {
                text: "VERDICT: CLEAN".to_string(),
                cache_control: None,
            }],
            None,
        );
        reviewer.save().expect("save reviewer session");

        state.active_reviewer_id = Some(reviewer_id);
        app.session.review_loop = Some(state);

        let followup = super::commands_review::step_review_loop(&mut app);

        assert!(!followup);
        let state = app.session.review_loop.as_ref().unwrap();
        assert!(state.finished);
        assert_eq!(state.finish_reason.as_deref(), Some("converged"));
        assert!(
            !app.display_messages().iter().any(|msg| {
                msg.content
                    .contains("completion assessment now disagrees")
            }),
            "a passing gate re-check must not surface a failure through step_review_loop"
        );
    });
}

#[test]
fn step_review_loop_converged_without_fix_never_runs_gate_recheck() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let parent_session_id = app.session_id().to_string();

        // No todos persisted at all. The review converges WITHOUT touching any
        // files (empty touched_files), so `review_touched_files` is false and
        // the completion gates must NOT be re-run.
        let mut state = drive_to_final_confirmation_lens(vec![]);

        let mut reviewer = crate::session::Session::create(None, None);
        let reviewer_id = reviewer.id.clone();
        reviewer.add_message_with_display_role(
            crate::message::Role::User,
            vec![crate::message::ContentBlock::Text {
                text: "VERDICT: CLEAN".to_string(),
                cache_control: None,
            }],
            None,
        );
        reviewer.save().expect("save reviewer session");

        state.active_reviewer_id = Some(reviewer_id);
        app.session.review_loop = Some(state);

        let followup = super::commands_review::step_review_loop(&mut app);

        assert!(!followup);
        let state = app.session.review_loop.as_ref().unwrap();
        assert!(state.finished);
        assert_eq!(state.finish_reason.as_deref(), Some("converged"));
        // No gate re-check ran, so no failure surfaced and the digest is present.
        assert!(
            !app.display_messages().iter().any(|msg| {
                msg.content
                    .contains("completion assessment now disagrees")
            }),
            "a converged-without-fix review must not run the gate re-check"
        );
    });
}