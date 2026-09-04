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

        // A completed todo with NO completion confidence fails the completion
        // gate. (Give it a passing goal so it is purely the confidence gate that
        // trips, mirroring how `todo_confidence_summary` flags missing/missing
        // info as needs-validation.)
        crate::todo::save_todos(
            &session_id,
            &[crate::todo::TodoItem {
                group: None,
                id: "todo-1".to_string(),
                content: "Reviewed work".to_string(),
                status: "completed".to_string(),
                priority: "high".to_string(),
                blocked_by: Vec::new(),
                assigned_to: None,
                confidence: None,
                completion_confidence: None,
                confidence_history: Vec::new(),
            }],
        )
        .expect("save todos");

        // A converged loop that also fixed files must request the re-check.
        let mut state = jcode_session_types::ReviewLoopState::new();
        state.finished = true;
        state.needs_gate_recheck = true;
        state.current_lens = Some(jcode_session_types::ReviewLens::Correctness);

        super::commands_review::finish_review_loop(&mut app, &mut state);

        // The loop is left finished (never re-enters review), with the reason
        // recording that the post-review gate re-check disagreed.
        assert!(state.finished);
        let reason = state.finish_reason.as_deref().unwrap_or_default();
        assert!(
            reason.starts_with("converged_gate_recheck_failed"),
            "expected gate-recheck-failed reason, got {reason:?}"
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
fn gate_recheck_that_passes_finishes_cleanly() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let session_id = app.session_id().to_string();

        // A completed todo with valid completion confidence + a passing goal so
        // both gates hold.
        crate::todo::save_todos(
            &session_id,
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
                    crate::todo::ConfidenceState::from_legacy_score(90),
                    crate::todo::ConfidenceState::from_legacy_score(100),
                ],
            }],
        )
        .expect("save todos");
        crate::todo::save_goals(
            &session_id,
            &[crate::todo::TodoGoal {
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
            }],
        )
        .expect("save goal delivery state");

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
        state.needs_gate_recheck = true;
        state.current_lens = Some(jcode_session_types::ReviewLens::Correctness);

        super::commands_review::finish_review_loop(&mut app, &mut state);

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