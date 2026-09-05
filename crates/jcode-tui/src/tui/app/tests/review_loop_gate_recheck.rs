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
            app.display_messages()
                .iter()
                .any(|msg| { msg.content.contains("completion assessment now disagrees") }),
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
            app.display_messages()
                .iter()
                .any(|msg| { msg.content.contains("completion assessment now disagrees") }),
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
            !app.display_messages()
                .iter()
                .any(|msg| { msg.content.contains("completion assessment now disagrees") }),
            "a passing gate re-check must not surface a failure"
        );
    });
}

/// A goal that passes every ownership sub-check except trade_off must fail the
/// review-loop gate re-check (not silently pass), since trade_off is folded
/// into delivery_state_passes.
#[test]
fn gate_recheck_fails_when_only_trade_off_is_missing() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let session_id = app.session_id().to_string();

        // A completed todo with valid confidence; the goal is complete on every
        // dimension except trade_off (explicitly None).
        save_completed_todo(&session_id, Some(100));
        let mut goal = passing_goal();
        goal.trade_off = None;
        crate::todo::save_goals(&session_id, &[goal]).expect("save goal");

        let mut state = jcode_session_types::ReviewLoopState::new();
        state.finished = true;
        state.current_lens = Some(jcode_session_types::ReviewLens::Correctness);

        super::commands_review::finish_review_loop(&mut app, &mut state, true);

        // The loop stays finished (no gates<->review ping-pong) but the missed
        // trade-off assessment must surface as a gate re-check failure.
        assert!(state.finished);
        let reason = state.finish_reason.as_deref().unwrap_or_default();
        assert!(
            reason.starts_with("converged_gate_recheck_failed"),
            "missing trade_off alone must fail the gate re-check, got {reason:?}"
        );
        assert!(
            app.display_messages()
                .iter()
                .any(|msg| msg.content.contains("completion assessment now disagrees")),
            "a trade-off-only failure must be surfaced to the user"
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
            !app.display_messages()
                .iter()
                .any(|msg| { msg.content.contains("completion assessment now disagrees") }),
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
            completion_confidence: completion_confidence
                .map(|score| crate::todo::ConfidenceState::from_legacy_score(score)),
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
        trade_off: Some(crate::todo::TradeOffState::SomeConsidered),
        trade_offs: Some("weighed alternatives".to_string()),
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
        // The end-of-loop digest must also reflect the disagreement in its
        // "Finish reason" line, not just the one-off surface message. A record
        // is present (drive_to_final_confirmation_lens seeds one), so the full
        // digest is rendered with the failed reason.
        assert!(
            app.display_messages().iter().any(|msg| {
                msg.content
                    .contains("Finish reason: converged_gate_recheck_failed")
            }),
            "expected the digest to record the gate-recheck failure reason"
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
            !app.display_messages()
                .iter()
                .any(|msg| { msg.content.contains("completion assessment now disagrees") }),
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
            !app.display_messages()
                .iter()
                .any(|msg| { msg.content.contains("completion assessment now disagrees") }),
            "a converged-without-fix review must not run the gate re-check"
        );
    });
}

// Regression: the auto review loop must also be *seeded* on the remote product
// TUI turn-completion path. The remote TUI (App::new_for_remote -> run_remote)
// completes turns through server_events.rs's `ServerEvent::Done` handler, which
// is the exact analogue of the local `finish_turn` completion path but previously
// never called `maybe_enter_review_loop` (the local path seeded it at the end of
// `run_turn_interactive` instead). Without seeding here the loop is never created
// and review rounds silently never run. The seed must live on the normal-completion
// (Done) arm only, NOT inside `schedule_turn_end_followups`, because that helper
// is also reached on interrupt and failed-retry paths where the work is incomplete.
// This pins that a remote, non-harness, non-replay app seeding happens exactly once
// when its current turn's ServerEvent::Done arrives.
#[test]
fn remote_done_seeds_review_loop_on_remote_product_path() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut remote = crate::tui::backend::RemoteConnection::dummy();

        // Model the real product TUI: a remote server-client (not the harness),
        // so the auto review loop is allowed to seed. Under the ordinary
        // TestHarness this would be skipped for determinism.
        app.is_remote = true;
        app.is_replay = false;
        app.runtime_mode = super::AppRuntimeMode::RemoteClient;
        app.autoreview_enabled = true;
        app.pending_queued_dispatch = false;
        app.improve_mode = None;
        app.session.review_loop = None;

        // A normal in-progress remote turn whose current message is the one
        // finishing. This is exactly the state the remote client is in when
        // ServerEvent::Done for the current message arrives.
        app.is_processing = true;
        app.status = crate::tui::app::ProcessingStatus::RunningTool("x".to_string());
        app.current_message_id = Some(42);
        app.stream_message_ended = true;

        assert!(
            app.session.review_loop.is_none(),
            "precondition: no review loop before the turn completes"
        );

        // The real remote turn-completion handler. It must seed the loop here,
        // exactly once (ServerEvent::Done for the normal completed turn), and
        // NOT on interrupts or failed-retry paths (which never reach Done).
        let _ = app.handle_server_event(
            crate::protocol::ServerEvent::Done { id: 42 },
            &mut remote,
        );

        assert!(
            app.session.review_loop.as_ref().is_some_and(|s| {
                s.current_lens.is_some() && s.record.is_some() && !s.finished
            }),
            "remote normal turn completion must seed (and keep alive) the review loop",
        );
        // Seeding happened for THIS completion only: a fresh session must not
        // pre-seed, and the guard prohibits restarting a finished loop later.
        let review_loop_count = app
            .session
            .review_loop
            .as_ref()
            .map(|s| s.record.as_ref().map(|r| r.rounds.len()).unwrap_or(0))
            .unwrap_or(0);
        assert_eq!(review_loop_count, 0, "seeded review loop must have no rounds yet");
    });
}

// Regression (R-A1): the review loop must SELF-DRIVE from the idle tick, not
// rely only on turn-end events. After a lens reviewer is spawned there is no
// further ServerEvent::Done (a spawned reviewer runs asynchronously in its own
// window; a CLEAN verdict produces no synthetic fix turn), so the loop used to
// stall right after the first spawn and the review rounds never actually ran. The
// local idle tick handler now polls the loop; this pins that a ready verdict is
// consumed and the loop advances to the next lens just from a tick.
#[test]
fn idle_tick_self_drives_review_loop_advance() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let parent_session_id = app.session_id().to_string();

        // Seed a live review loop at the first lens (Correctness).
        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
        assert_eq!(
            state.current_lens,
            Some(jcode_session_types::ReviewLens::Correctness)
        );

        // A real reviewer child session whose last message is a CLEAN verdict,
        // exactly as a completed lens reviewer leaves behind.
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
        // Ensure it is not considered "started" yet by the tick's polling guard.
        app.is_processing = false;
        app.pending_queued_dispatch = false;

        // The idle tick (local run loop) must poll the reviewer and advance to
        // the next lens without any turn-end event.
        let _redraw = crate::tui::app::local::handle_tick(&mut app);

        let advanced = app.session.review_loop.as_ref();
        assert!(advanced.is_some(), "review loop must remain present");
        // The CLEAN verdict for Correctness was consumed: the loop advanced to
        // EdgesErrors AND spawned (and set an active reviewer for) that lens.
        let advanced = advanced.unwrap();
        assert_eq!(
            advanced.current_lens,
            Some(jcode_session_types::ReviewLens::ALL[1]),
            "idle tick must consume the CLEAN verdict and advance to the next lens"
        );
        // The next lens reviewer was spawned (active_reviewer_id set for it).
        assert!(
            advanced.active_reviewer_id.is_some(),
            "advancing must spawn an active reviewer for the next lens"
        );
        // The parent status notice reflects the lens now under review (the
        // spawned reviewer runs in its own window; this is the parent-side
        // signal that the loop advanced).
        assert!(
            app.status_notice
                .as_ref()
                .is_some_and(|(n, _)| n.contains("reviewing") && n.contains("Edges/Errors")),
            "advancing must surface the new lens in the parent status notice, got {:?}",
            app.status_notice.as_ref().map(|(n, _)| n.as_str())
        );
    });
}

// Regression (R-G1): on the REMOTE product TUI the review fix-turn (findings ->
// "fix them" prompt) must be *dispatched*, not just staged. The remote client
// does not consume `pending_turn` (only the local run() loop does), so the
// previous start_synthetic_user_turn path left the fix prompt unsent and the
// loop stalled after the first findings. This pins that, when a remote reviewer
// returns FINDINGS, the fix prompt is enqueued to `queued_messages`, which the
// remote run loop dispatches via process_remote_followups.
#[test]
fn remote_findings_enqueue_fix_turn_for_dispatch() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        // The real product TUI is a remote server-client.
        app.is_remote = true;
        app.is_replay = false;
        app.runtime_mode = super::AppRuntimeMode::RemoteClient;

        // Seed a live review loop at the first lens with an in-flight reviewer
        // whose session reports a FINDINGS verdict.
        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
        let mut reviewer = crate::session::Session::create(None, None);
        let reviewer_id = reviewer.id.clone();
        reviewer.add_message_with_display_role(
            crate::message::Role::User,
            vec![crate::message::ContentBlock::Text {
                text: "VERDICT: FINDINGS\nFINDING: HIGH|a.rs|Off-by-one bug".to_string(),
                cache_control: None,
            }],
            None,
        );
        reviewer.save().expect("save reviewer session");
        state.active_reviewer_id = Some(reviewer_id);
        app.session.review_loop = Some(state);

        let _followup = super::commands_review::step_review_loop(&mut app);

        // The fix prompt must be queued for the remote dispatch loop (NOT behind
        // the local-only pending_turn path).
        assert!(
            !app.queued_messages.is_empty(),
            "remote findings must enqueue the fix prompt for dispatch"
        );
        assert!(
            app.queued_messages.iter().any(|q| q.contains("Fix them")),
            "the queued fix prompt must ask to fix the findings, got {:?}",
            app.queued_messages
        );
        // The loop is still active and awaiting its post-fix re-check.
        let state = app.session.review_loop.as_ref().unwrap();
        assert!(!state.finished);
        assert!(
            state.awaiting_postfix_recheck,
            "must be awaiting the post-fix re-check"
        );
        // Round-E guard: the fix is flagged for immediate dispatch, so the idle
        // tick must NOT prematurely spawn the post-fix re-check reviewer (which
        // would review the pre-fix tree before the fix runs).
        assert!(
            app.pending_queued_dispatch,
            "remote fix turn must request dispatch (close the premature-spawn window)"
        );
        assert_eq!(
            state.current_lens,
            Some(jcode_session_types::ReviewLens::Correctness),
            "the loop must not advance to the next lens before the fix is dispatched"
        );
    });
}

// Regression (remote tick self-drive): the local idle tick self-drive is
// covered by idle_tick_self_drives_review_loop_advance, but the REMOTE run loop
// drives review via remote::handle_tick (remote.rs), which must equally consume
// a ready verdict and advance the loop. This pins that a remote reviewer's CLEAN
// verdict is polled and the loop advances to the next lens from a remote tick.
#[test]
fn remote_tick_self_drives_review_loop_advance() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut remote = crate::tui::backend::RemoteConnection::dummy();

        app.is_remote = true;

        // Seed a live review loop at the first lens with an in-flight reviewer
        // whose session holds a CLEAN verdict, exactly as a finished lens
        // reviewer leaves behind.
        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
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
        app.is_processing = false;
        app.pending_queued_dispatch = false;

        // The REMOTE idle tick must poll the reviewer and advance to the next
        // lens without any turn-end event.
        let _ = rt.block_on(crate::tui::app::remote::handle_tick(&mut app, &mut remote));

        let advanced = app.session.review_loop.as_ref().unwrap();
        assert_eq!(
            advanced.current_lens,
            Some(jcode_session_types::ReviewLens::ALL[1]),
            "remote idle tick must consume the CLEAN verdict and advance to the next lens"
        );
        assert!(
            advanced.active_reviewer_id.is_some(),
            "advancing must spawn an active reviewer for the next lens"
        );
        // Same parent-side progress signal as the local path: the spawned
        // next-lens reviewer is surfaced in the status notice.
        assert!(
            app.status_notice
                .as_ref()
                .is_some_and(|(n, _)| n.contains("reviewing") && n.contains("Edges/Errors")),
            "remote advance must surface the new lens in the parent status notice, got {:?}",
            app.status_notice.as_ref().map(|(n, _)| n.as_str())
        );
    });
}

// Regression (multi-step, Round E): verify the full remote findings -> fix
// enqueue (R-G1) -> dispatch (R-E guard) -> re-check -> advance sequence does
// not corrupt loop state across successive step_review_loop calls.
#[test]
fn remote_findings_fix_then_recheck_advances() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.is_remote = true;
        app.is_replay = false;
        app.runtime_mode = super::AppRuntimeMode::RemoteClient;

        // Step 1: seed a loop with an in-flight reviewer that returns FINDINGS.
        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
        let mut reviewer = crate::session::Session::create(None, None);
        let reviewer_id = reviewer.id.clone();
        reviewer.add_message_with_display_role(
            crate::message::Role::User,
            vec![crate::message::ContentBlock::Text {
                text: "VERDICT: FINDINGS\nFINDING: MEDIUM|b.rs|Leak".to_string(),
                cache_control: None,
            }],
            None,
        );
        reviewer.save().expect("save reviewer session");
        state.active_reviewer_id = Some(reviewer_id);
        app.session.review_loop = Some(state);

        // Step 2: consume the FINDINGS verdict -> QueueFixTurn enqueues the fix.
        super::commands_review::step_review_loop(&mut app);
        assert_eq!(app.queued_messages.len(), 1);
        assert!(app.pending_queued_dispatch, "fix must be flagged for dispatch");
        let lens = app.session.review_loop.as_ref().unwrap();
        assert!(lens.awaiting_postfix_recheck, "must await the post-fix re-check");

        // Step 3: simulate the fix being dispatched and completing (R-G1 sends it,
        // the server runs it, and a Done arrives that clears processing + dispatch).
        app.queued_messages.clear();
        app.is_processing = true; // fix turn in flight
        app.pending_queued_dispatch = false;
        app.is_processing = false; // fix turn Done
        let recheck_lens = app.session.review_loop.as_ref().unwrap().current_lens.unwrap();

        // Step 4: the post-fix Done path re-spawns the re-check reviewer for the
        // SAME lens (active_reviewer_id was None after the fix).
        super::commands_review::step_review_loop(&mut app);
        let rechecker_id = app
            .session
            .review_loop
            .as_ref()
            .unwrap()
            .active_reviewer_id
            .clone()
            .expect("a fresh re-check reviewer must be spawned");
        assert_eq!(
            app.session.review_loop.as_ref().unwrap().current_lens,
            Some(recheck_lens),
            "re-check must stay on the same lens, not advance before it is clean"
        );

        // Step 5: the re-check reviewer reports CLEAN -> the loop records it and
        // advances to the next lens.
        let mut rechecker = crate::session::Session::load(&rechecker_id).expect("load re-checker");
        rechecker.add_message_with_display_role(
            crate::message::Role::User,
            vec![crate::message::ContentBlock::Text {
                text: "VERDICT: CLEAN".to_string(),
                cache_control: None,
            }],
            None,
        );
        rechecker.save().expect("save re-checker verdict");
        super::commands_review::step_review_loop(&mut app);

        let advanced = app.session.review_loop.as_ref().unwrap();
        assert!(!advanced.finished);
        assert_eq!(
            advanced.current_lens,
            Some(jcode_session_types::ReviewLens::ALL[1]),
            "after a fixed-then-clean lens the loop must advance to the next lens"
        );
    });
}

// Regression (Round-E, dispatch-failure gap): when a review fix turn is queued
// for remote dispatch but the queued send FAILED and restored the message to
// `queued_messages` (see begin_remote_send error path in
// process_remote_followups), `pending_queued_dispatch` is false yet the fix is
// still unborn. An idle tick must NOT then spawn the post-fix re-check reviewer
// against the pre-fix tree. This pins that the tick self-drive refuses to poll
// the loop while any follow-up message is queued-but-undispatched, leaving the
// fix to be redelivered instead.
#[test]
fn remote_tick_does_not_spawn_premature_recheck_while_fix_queued_after_failed_dispatch() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut remote = crate::tui::backend::RemoteConnection::dummy();

        app.is_remote = true;
        app.is_replay = false;
        app.runtime_mode = super::AppRuntimeMode::RemoteClient;

        // A live loop mid post-fix re-check for the first lens (findings were
        // reported, a fix was queued, but the dispatch failed and restored the
        // fix prompt to queued_messages).
        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
        state.awaiting_postfix_recheck = true;
        state.active_reviewer_id = None;
        app.session.review_loop = Some(state);

        // Mimic the failed-dispatch-restore state: the fix is still in the
        // queue, pending_queued_dispatch has been consumed (false), and the
        // client is idle.
        app.queued_messages.push("The reviewer found the following issues. Fix them:\n\n[HIGH] a.rs: bug".to_string());
        app.pending_queued_dispatch = false;
        app.is_processing = false;

        // The idle tick must NOT spawn a premature re-check reviewer: a fix is
        // still waiting in the queue to be dispatched.
        let _ = rt.block_on(crate::tui::app::remote::handle_tick(&mut app, &mut remote));

        let state = app.session.review_loop.as_ref().unwrap();
        assert!(!state.finished, "loop must not finalize while a fix is pending");
        assert!(
            state.active_reviewer_id.is_none(),
            "must NOT spawn the post-fix re-check reviewer against the pre-fix tree while the fix is still queued"
        );
        assert_eq!(
            state.current_lens,
            Some(jcode_session_types::ReviewLens::Correctness),
            "the loop must stay on the fixing lens; the fix has not been dispatched yet"
        );
        // The tick's own queued-message dispatch (remote handle_tick) delivers
        // the restored fix instead of the review-poll spawning a premature
        // re-check: the fix turn is now in flight (is_processing true), so it
        // will complete and only then re-poll the re-check reviewer.
        assert!(
            app.is_processing,
            "the restored fix must be dispatched (turn in flight), not dropped"
        );
        assert!(
            app.queued_messages.is_empty(),
            "the fix prompt must have been dispatched, not left stranded in the queue"
        );
    });
}

// Regression (Round-E guard, local path): the local idle tick must equally
// refuse to spawn a post-fix re-check reviewer while a fix continuation is
// queued-but-undispatched. On local the fix turn is normally started via
// `start_synthetic_user_turn` (which sets is_processing), so this state is
// defensive; but if a queued message is present with an idle client
// (is_processing false, pending_queued_dispatch false, awaiting_postfix_recheck
// true, active_reviewer_id None), the tick must not spawn the re-check against
// the pre-fix tree.
#[test]
fn local_tick_does_not_spawn_premature_recheck_while_fix_queued() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();

        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
        state.awaiting_postfix_recheck = true;
        state.active_reviewer_id = None;
        app.session.review_loop = Some(state);

        app.queued_messages.push("The reviewer found the following issues. Fix them:\n\n[HIGH] a.rs: bug".to_string());
        app.pending_queued_dispatch = false;
        app.is_processing = false;

        let _ = crate::tui::app::local::handle_tick(&mut app);

        let state = app.session.review_loop.as_ref().unwrap();
        assert!(!state.finished, "loop must not finalize while a fix is pending");
        assert!(
            state.active_reviewer_id.is_none(),
            "local tick must NOT spawn a premature re-check reviewer while the fix is still queued"
        );
        assert_eq!(
            state.current_lens,
            Some(jcode_session_types::ReviewLens::Correctness),
            "the loop must stay on the fixing lens"
        );
    });
}

// Failure mode (reviewer gone): if the in-flight reviewer child session
// vanishes or becomes unloadable (deleted, corrupt on disk), step_review_loop
// must NOT poll forever or spawn a duplicate. It finalizes the loop cleanly
// with a terminal reason so it cannot be mistaken for still-active work.
#[test]
fn step_review_loop_finalizes_when_reviewer_session_is_gone() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();

        // Seed a live loop at the first lens whose active reviewer does not
        // exist as a loadable session (simulates the reviewer being deleted or
        // unloadable). Polling it yields PollResult::Gone.
        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
        state.active_reviewer_id = Some("session_reviewer_definitely_missing".to_string());
        // Exhaust the respawn budget so the very next Gone finalizes instead of
        // respawning another reviewer (a real spawn would launch a client).
        state.reviewer_respawn_count = 2;
        app.session.review_loop = Some(state);
        app.is_processing = false;
        app.pending_queued_dispatch = false;

        let followup = super::commands_review::step_review_loop(&mut app);

        // With the respawn budget exhausted, the loop finalizes; no further
        // follow-up is scheduled.
        assert!(!followup, "an exhausted-budget gone reviewer must not schedule work");
        let state = app.session.review_loop.as_ref().unwrap();
        assert!(state.finished, "loop must finalize when the reviewer is gone past the budget");
        assert_eq!(
            state.finish_reason.as_deref(),
            Some("reviewer_unavailable"),
            "the terminal reason must identify the unavailable reviewer"
        );
        assert!(
            state.active_reviewer_id.is_none(),
            "the stale reviewer id must be cleared"
        );
        assert!(
            app.display_messages().iter().any(|msg| {
                msg.content
                    .contains("the reviewer session kept being lost")
            }),
            "expected the exhausted-respawn notice to be surfaced"
        );
        assert!(
            app.status_notice.as_ref().is_some_and(|(n, _)| n.contains("reviewer gone")),
            "expected the reviewer-gone status notice"
        );
    });
}

// Failure mode (reviewer lost, respawn): a single transient reviewer loss must
// NOT abort the whole lens loop. step_review_loop respawns the same lens up to
// the bounded budget, clears the stale id, and keeps the loop active.
#[test]
fn step_review_loop_respawns_lost_reviewer_within_budget() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();

        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
        state.active_reviewer_id = Some("session_reviewer_definitely_missing".to_string());
        // The respawn budget still has headroom (0 < 2): the Gone must respawn.
        state.reviewer_respawn_count = 0;
        app.session.review_loop = Some(state);
        app.is_processing = false;
        app.pending_queued_dispatch = false;

        let followup = super::commands_review::step_review_loop(&mut app);

        // A respawn schedules a fresh reviewer (follow-up true), keeps the loop
        // active on the same lens, and bumps the budget counter.
        assert!(followup, "a within-budget gone reviewer must respawn and schedule work");
        let state = app.session.review_loop.as_ref().unwrap();
        assert!(!state.finished, "the loop must stay active after respawning");
        assert_eq!(state.reviewer_respawn_count, 1, "respawn budget must bump");
        assert_eq!(
            state.current_lens,
            Some(jcode_session_types::ReviewLens::Correctness),
            "respawn must keep the same lens"
        );
        assert!(
            state.active_reviewer_id.is_some(),
            "a fresh reviewer session must be spawned for the lost lens"
        );
        assert!(
            app.status_notice.as_ref().is_some_and(|(n, _)| n.contains("respawning")),
            "expected the respawn status notice"
        );
    });
}

// Regression (trade-off #3): the idle self-drive must NOT do a full
// `Session::load` behind a pending reviewer on every idle tick. The first idle
// poll runs, but an immediate re-poll is debounced (returns false without
// stepping the loop). This is exercised twice to cover the per-App debounce.
#[test]
fn idle_self_drive_debounces_rapid_repeats() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();

        // A live loop with a reviewer still pending (no verdict yet).
        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
        state.active_reviewer_id = Some("session_reviewer_pending".to_string());
        app.session.review_loop = Some(state);
        app.is_processing = false;
        app.pending_queued_dispatch = false;
        app.last_review_loop_idle_poll = None;

        // First idle poll runs (nothing cached yet).
        let first = super::commands_review::maybe_poll_review_loop_from_idle(&mut app);
        assert!(first, "first idle poll must run");

        // An immediate second idle poll within the debounce window is suppressed
        // so we do not reload the reviewer session again this tick burst.
        let second = super::commands_review::maybe_poll_review_loop_from_idle(&mut app);
        assert!(!second, "second idle poll within the debounce window must be suppressed");

        // A fresh App (its own debounce clock) polls immediately.
        let mut app2 = create_test_app();
        let mut state2 = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state2);
        state2.active_reviewer_id = Some("creator_reviewer_pending".to_string());
        app2.session.review_loop = Some(state2);
        app2.is_processing = false;
        app2.pending_queued_dispatch = false;
        app2.last_review_loop_idle_poll = None;
        assert!(
            super::commands_review::maybe_poll_review_loop_from_idle(&mut app2),
            "a fresh App's first poll is independent of app1's debounce clock"
        );
    });
}

// Regression (trade-off #4): the Round-E guard must block only on the review's
// OWN unborn fix, not on an unrelated interleave message. Setting an unrelated
// message and a ready CLEAN verdict must still advance the loop (the old
// `has_queued_followups()` guard would have stalled it).
#[test]
fn unrelated_interleave_does_not_stall_review_loop_advance() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut remote = crate::tui::backend::RemoteConnection::dummy();

        app.is_remote = true;

        // Live loop at Correctness with a reviewer that already emitted CLEAN.
        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
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
        app.is_processing = false;
        app.pending_queued_dispatch = false;
        app.last_review_loop_idle_poll = None;

        // An unrelated interleave message is staged. The old guard
        // (`has_queued_followups()`) would treat this as "queued work" and block
        // the loop; the narrow `review_fix_pending` guard does not.
        app.interleave_message = Some("user background note".to_string());

        let _ = rt.block_on(crate::tui::app::remote::handle_tick(&mut app, &mut remote));

        let state = app.session.review_loop.as_ref().unwrap();
        assert_eq!(
            state.current_lens,
            Some(jcode_session_types::ReviewLens::ALL[1]),
            "an unrelated interleave message must not stall the review loop advance"
        );
        assert!(
            state.active_reviewer_id.is_some(),
            "the loop must still spawn the next lens reviewer despite the interleave"
        );
    });
}

// Regression (Round-E, turn-end gap): the premature post-fix re-check guard must
// also hold on the TURN-END path (schedule_turn_end_followups), not just the
// idle self-drive. If a review fix is queued-but-undispatched (awaiting_postfix_recheck
// true, active_reviewer_id None), a turn-end must NOT step the loop and spawn
// the re-check reviewer against the pre-fix tree.
#[test]
fn turn_end_followups_does_not_spawn_premature_recheck_while_fix_queued() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();

        // Live loop mid post-fix re-check for the first lens, with the fix turn
        // still queued-but-undispatched.
        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
        state.awaiting_postfix_recheck = true;
        state.active_reviewer_id = None;
        app.session.review_loop = Some(state);

        app.queued_messages.push("The reviewer found the following issues. Fix them:\n\n[HIGH] a.rs: bug".to_string());
        app.pending_queued_dispatch = false;
        app.is_processing = false;

        // A turn-end fires (e.g. some other event completed).
        let scheduled = app.schedule_turn_end_followups();

        // The turn-end must NOT spawn a premature re-check reviewer: it owns the
        // continuation (returns true) while the fix waits to be dispatched.
        assert!(scheduled, "turn-end should treat the pending fix as owned work");
        let state = app.session.review_loop.as_ref().unwrap();
        assert!(!state.finished, "loop must not finalize while the fix is pending");
        assert!(
            state.active_reviewer_id.is_none(),
            "turn-end must NOT spawn the re-check reviewer against the pre-fix tree"
        );
        assert_eq!(
            state.current_lens,
            Some(jcode_session_types::ReviewLens::Correctness),
            "the loop must stay on the fixing lens"
        );
        assert!(
            !app.queued_messages.is_empty(),
            "the queued fix prompt must remain for dispatch"
        );
    });
}

// Regression (stop cancels pending fix): stopping a review loop must cancel any
// review fix turn that is queued-but-undispatched (the remote path stages the
// fix into queued_messages). Otherwise the "fix them" prompt would still be
// dispatched after the loop was stopped.
#[test]
fn review_loop_stop_cancels_queued_fix() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.is_remote = true;
        app.is_replay = false;
        app.runtime_mode = super::AppRuntimeMode::RemoteClient;

        // Start a loop, then simulate a queued-but-undispatched fix as the remote
        // path would leave it (awaiting_postfix_recheck + a fix prompt staged).
        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
        state.awaiting_postfix_recheck = true;
        state.active_reviewer_id = None;
        app.session.review_loop = Some(state);
        app.queued_messages.push("The reviewer found the following issues. Fix them:\n\n[HIGH] a.rs: bug".to_string());
        app.pending_queued_dispatch = true;

        // Stop the loop.
        app.input = "/review-loop stop".to_string();
        app.submit_input();

        let state = app.session.review_loop.as_ref().unwrap();
        assert!(state.finished, "loop must be stopped");
        assert_eq!(state.finish_reason.as_deref(), Some("user_stopped"));
        assert!(
            app.queued_messages.is_empty(),
            "stopping the loop must cancel its queued fix, not dispatch it later"
        );
        assert!(
            !app.pending_queued_dispatch,
            "stopping the loop must clear the pending-dispatch flag"
        );
    });
}

// Regression (improve clears pending review fix): starting improve/refactor must
// clear an active review loop AND cancel any review fix queued-but-undispatched
// (mirroring /review-loop stop), so a stranded "fix them" prompt from the review
// loop is not dispatched after the loop is replaced.
#[test]
fn clear_review_loop_on_improve_cancels_queued_fix() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.is_remote = true;
        app.is_replay = false;
        app.runtime_mode = super::AppRuntimeMode::RemoteClient;

        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
        state.awaiting_postfix_recheck = true;
        state.active_reviewer_id = None;
        app.session.review_loop = Some(state);
        app.queued_messages.push("The reviewer found the following issues. Fix them:\n\n[HIGH] a.rs: bug".to_string());
        app.pending_queued_dispatch = true;

        super::commands_review::clear_review_loop_on_improve(&mut app);

        assert!(
            app.session.review_loop.is_none(),
            "improve must clear the active review loop"
        );
        assert!(
            app.queued_messages.is_empty(),
            "improve must cancel the review's queued fix, not dispatch it later"
        );
        assert!(
            !app.pending_queued_dispatch,
            "improve must clear the pending-dispatch flag"
        );
    });
}

// Regression (pending_queued_dispatch blocks idle loop step): while a queued
// follow-up (poke / gate continuation) is pending dispatch, the idle self-drive
// must NOT also step the review loop, or it would spawn a reviewer concurrently
// with the dispatch (double-scheduling). This is distinct from review_fix_pending:
// the loop here is NOT awaiting a fix, yet a poke can be queued in a loop gap.
#[test]
fn idle_poll_does_not_step_loop_while_non_fix_dispatch_pending() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();

        // A live loop, first lens, no in-flight reviewer yet, awaiting nothing.
        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
        state.active_reviewer_id = None;
        state.awaiting_postfix_recheck = false;
        app.session.review_loop = Some(state);
        app.is_processing = false;
        app.last_review_loop_idle_poll = None;

        // A poke/gate continuation is queued and pending dispatch (NOT the fix).
        app.pending_queued_dispatch = true;
        app.queued_messages.clear();
        app.queued_messages.push("Continue working on the todos.".to_string());

        let polled = super::commands_review::maybe_poll_review_loop_from_idle(&mut app);

        assert!(
            !polled,
            "idle poll must not step the loop while a non-fix queued dispatch is pending"
        );
        let state = app.session.review_loop.as_ref().unwrap();
        assert!(
            state.active_reviewer_id.is_none(),
            "the loop must not have spawned a reviewer while a queued dispatch is pending"
        );
    });
}

// Regression (respawn then verdict): a lost reviewer is respawned (bounded), and
// the respawned reviewer's CLEAN verdict is honored — the loop advances to the
// next lens exactly as if the original had produced it. Pins that respawning does
// not corrupt the loop's ability to make progress from the respawned reviewer.
#[test]
fn loop_advances_after_respawned_reviewer_reports_clean() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();

        // Live loop at Correctness with a reviewer that is lost.
        let mut state = jcode_session_types::ReviewLoopState::new();
        super::review_loop::enter_review_loop(&mut state);
        state.active_reviewer_id = Some("session_reviewer_definitely_missing".to_string());
        state.reviewer_respawn_count = 0;
        app.session.review_loop = Some(state);
        app.is_processing = false;
        app.pending_queued_dispatch = false;

        // First step: the lost reviewer triggers a respawn (budget 0 -> 1).
        let followup = super::commands_review::step_review_loop(&mut app);
        assert!(followup, "one-shot respawn must schedule a fresh reviewer");
        let respawned_id = app
            .session
            .review_loop
            .as_ref()
            .unwrap()
            .active_reviewer_id
            .clone()
            .expect("a respawned reviewer session must be set");

        // The respawned reviewer reports CLEAN.
        let mut reviewer = crate::session::Session::load(&respawned_id).expect("load respawned");
        reviewer.add_message_with_display_role(
            crate::message::Role::User,
            vec![crate::message::ContentBlock::Text {
                text: "VERDICT: CLEAN".to_string(),
                cache_control: None,
            }],
            None,
        );
        reviewer.save().expect("save respawned verdict");

        // Stepping again consumes the CLEAN verdict and advances to the next lens.
        super::commands_review::step_review_loop(&mut app);
        let state = app.session.review_loop.as_ref().unwrap();
        assert!(!state.finished, "a clean respawned verdict must keep the loop running");
        assert_eq!(
            state.current_lens,
            Some(jcode_session_types::ReviewLens::ALL[1]),
            "the respawned reviewer's clean verdict must advance to the next lens"
        );
    });
}
