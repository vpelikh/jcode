// Regression tests for issue #391: a queued message must survive a reload or
// disconnect that races the turn-end dispatch, staying queued until the turn
// naturally completes instead of silently disappearing.
//
// The drop happened when a queued follow-up had already been dequeued into an
// in-flight send. That in-flight shape lives only in
// `rate_limit_pending_message` as `is_system && !auto_retry`, which has no
// retry path: the tick resend requires a `rate_limit_reset` timestamp and the
// disconnect resend requires `auto_retry`. Both the disconnect handler and the
// reload snapshot must therefore fold it back into the queue.

#[test]
fn test_disconnect_recovers_inflight_queued_continuation_to_queue() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    // A queued follow-up was dequeued and handed to begin_remote_send: it now
    // lives only in rate_limit_pending_message with the queued-continuation
    // shape (is_system, no auto-retry, no scheduled reset).
    app.is_processing = true;
    app.status = ProcessingStatus::Streaming;
    app.current_message_id = Some(12);
    app.rate_limit_pending_message = Some(PendingRemoteMessage {
        content: "queued follow-up in flight".to_string(),
        images: vec![],
        is_system: true,
        system_reminder: Some("hidden reminder".to_string()),
        auto_retry: false,
        retry_attempts: 0,
        retry_at: None,
    });
    app.rate_limit_reset = None;

    let mut state = remote::RemoteRunState::default();
    remote::handle_disconnect(&mut app, &mut state, None);

    // The in-flight continuation must be back on the queue, not dropped.
    assert_eq!(app.queued_messages(), &["queued follow-up in flight"]);
    assert_eq!(app.hidden_queued_system_messages, vec!["hidden reminder"]);
    assert!(
        app.rate_limit_pending_message.is_none(),
        "recovered continuation must not linger as an unreachable pending message"
    );
}

#[test]
fn test_disconnect_still_clears_pending_for_non_queued_shapes() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    // A plain user message pending (not the queued-continuation shape) keeps
    // the old behavior when it cannot schedule a retry: exhausted attempts
    // clear it rather than re-queueing as a system continuation.
    app.is_processing = true;
    app.rate_limit_pending_message = Some(PendingRemoteMessage {
        content: "user retry message".to_string(),
        images: vec![],
        is_system: false,
        system_reminder: None,
        auto_retry: true,
        retry_attempts: u8::MAX,
        retry_at: None,
    });

    let mut state = remote::RemoteRunState::default();
    remote::handle_disconnect(&mut app, &mut state, None);

    assert!(
        app.queued_messages().is_empty(),
        "non-continuation pending shapes must not be converted into queued messages"
    );
}

#[test]
fn test_save_input_for_reload_persists_inflight_queued_continuation() {
    let mut app = create_test_app();
    let session_id = format!("test-391-inflight-{}", std::process::id());

    // Simulate the reload racing the dispatch: one message still queued, one
    // already dequeued into the in-flight pending slot.
    app.queued_messages.push("still queued".to_string());
    app.rate_limit_pending_message = Some(PendingRemoteMessage {
        content: "dispatched but unfinished".to_string(),
        images: vec![],
        is_system: true,
        system_reminder: Some("hidden reminder".to_string()),
        auto_retry: false,
        retry_attempts: 0,
        retry_at: None,
    });
    app.rate_limit_reset = None;

    app.save_input_for_reload(&session_id);

    let restored = App::restore_input_for_reload(&session_id).expect("reload state should exist");
    assert_eq!(
        restored.queued_messages,
        vec!["dispatched but unfinished", "still queued"],
        "the in-flight continuation must be persisted at the front of the queue"
    );
    assert_eq!(
        restored.hidden_queued_system_messages,
        vec!["hidden reminder"]
    );
    assert!(
        restored.rate_limit_pending_message.is_none(),
        "the continuation must not also restore as an unreachable pending message"
    );
}

#[test]
fn test_reload_preserves_completed_confidence_spike_challenge() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let reload_session_id = format!("test-confidence-spike-reload-{}", std::process::id());
        app.todo_confidence_spike_challenged = true;
        app.save_input_for_reload(&reload_session_id);

        let restored = App::restore_input_for_reload(&reload_session_id)
            .expect("confidence challenge state should survive reload");
        let mut reloaded_app = create_test_app();
        reloaded_app.apply_restored_reload_input(restored);
        assert!(reloaded_app.todo_confidence_spike_challenged);

        crate::todo::save_todos(
            &reloaded_app.session.id,
            &[crate::todo::TodoItem {
                id: "todo-1".to_string(),
                content: "Validate release result".to_string(),
                status: "completed".to_string(),
                priority: "high".to_string(),
                confidence: Some(crate::todo::ConfidenceState::from_legacy_score(100)),
                completion_confidence: Some(crate::todo::ConfidenceState::from_legacy_score(100)),
                confidence_history: vec![
                    crate::todo::ConfidenceState::from_legacy_score(70),
                    crate::todo::ConfidenceState::from_legacy_score(100),
                ],
                ..Default::default()
            }],
        )
        .expect("save completed todo");

        crate::todo::save_goals(
            &reloaded_app.session.id,
            &[crate::todo::TodoGoal {
                delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
                autonomy: Some(crate::todo::Autonomy::NecessaryFollowthrough),
                iteration_maturity: Some(crate::todo::IterationMaturity::OutcomeReached),
                feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::Representative),
                feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::MainPaths),
                feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
                trade_off: Some(crate::todo::TradeOffState::SomeConsidered),
                ..Default::default()
            }],
        )
        .expect("save passing goal");

        // Pin the default so the clean-cycle finish disarms rather than
        // re-arming; this test is about the spike-challenge flag, not the
        // default-on re-arm behavior.
        reloaded_app.auto_poke_default_on = false;
        // With all todos complete, validation passing, and the spike already
        // challenged, the clean finish queues one final-response continuation
        // (so the call returns true), disarms auto-poke (default off), and
        // keeps the challenge latched so the unchanged spike is not re-raised.
        assert!(reloaded_app.schedule_auto_poke_followup_if_needed());
        assert!(!reloaded_app.auto_poke_incomplete_todos);
        assert!(reloaded_app.todo_confidence_spike_challenged);
        assert!(reloaded_app.hidden_queued_system_messages.is_empty());
    });
}

#[test]
fn test_completion_gate_nudges_stop_after_budget_exhausted() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.auto_poke_incomplete_todos = true;

        // A completed todo with confidence below the gate threshold keeps the
        // completion gate failing on every check.
        crate::todo::save_todos(
            &app.session.id,
            &[crate::todo::TodoItem {
                id: "todo-1".to_string(),
                content: "Ship the fix".to_string(),
                status: "completed".to_string(),
                priority: "high".to_string(),
                confidence: Some(crate::todo::ConfidenceState::from_legacy_score(50)),
                completion_confidence: Some(crate::todo::ConfidenceState::from_legacy_score(50)),
                confidence_history: vec![crate::todo::ConfidenceState::from_legacy_score(50)],
                ..Default::default()
            }],
        )
        .expect("save low-confidence completed todo");

        // Each scheduled nudge consumes budget. Simulate the dispatch loop by
        // clearing the queued state between iterations (as if the turn ran and
        // the model made no todo progress).
        for attempt in 0..App::TODO_COMPLETION_GATE_MAX_ATTEMPTS {
            assert!(
                app.schedule_auto_poke_followup_if_needed(),
                "attempt {attempt} should still schedule a gate nudge"
            );
            app.queued_messages.clear();
            app.pending_queued_dispatch = false;
        }

        // Budget exhausted: the gate must stop scheduling and disarm auto-poke
        // instead of looping forever (observed live as one API call per ~5s).
        assert!(
            !app.schedule_auto_poke_followup_if_needed(),
            "exhausted gate must not schedule another nudge"
        );
        assert!(!app.auto_poke_incomplete_todos);
        assert!(!app.pending_queued_dispatch);
        assert!(app.queued_messages.is_empty());
        assert!(app.hidden_queued_system_messages.is_empty());
        assert_eq!(app.todo_confidence_gate_attempts, 0);
    });
}

#[test]
fn low_ownership_is_gated_after_the_completed_todo_was_saved() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.auto_poke_incomplete_todos = true;

        crate::todo::save_todos(
            &app.session.id,
            &[crate::todo::TodoItem {
                id: "todo-1".to_string(),
                content: "Ship the complete workflow".to_string(),
                status: "completed".to_string(),
                priority: "high".to_string(),
                group: Some("release".to_string()),
                confidence: Some(crate::todo::ConfidenceState::from_legacy_score(100)),
                completion_confidence: Some(crate::todo::ConfidenceState::from_legacy_score(100)),
                confidence_history: vec![crate::todo::ConfidenceState::from_legacy_score(100)],
                ..Default::default()
            }],
        )
        .expect("save completed todo");

        crate::todo::save_goals(
            &app.session.id,
            &[crate::todo::TodoGoal {
                group: Some("release".to_string()),
                delivery_state: Some(crate::todo::DeliveryState::Integrated),
                closed_feedback_loop: Some(crate::todo::FeedbackLoopState::from_legacy_score(100)),
                feedback_loop: Some("run the end-to-end release check".to_string()),
                feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::Representative),
                feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::MainPaths),
                feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
                autonomy: Some(crate::todo::Autonomy::NecessaryFollowthrough),
                iteration_maturity: Some(crate::todo::IterationMaturity::OutcomeReached),
                ..Default::default()
            }],
        )
        .expect("save low-ownership goal");

        assert!(app.schedule_auto_poke_followup_if_needed());
        assert!(app.pending_queued_dispatch);
        assert_eq!(app.queued_messages.len(), 1);
        assert!(app.queued_messages[0].contains("complete workflow"));
        // The goal is missing trade_off too, so the same queued ownership
        // continuation must surface the trade-off nudge through the real
        // turn-end gate path (not just the base unit test).
        assert!(
            app.queued_messages[0]
                .contains("consider at least one credible alternative"),
            "trade-off gate should be surfaced in the queued continuation"
        );

        let saved = crate::todo::load_todos(&app.session.id).expect("load saved todo");
        assert_eq!(saved[0].status, "completed");
    });
}

#[test]
fn remote_ownership_gate_reads_the_remote_goal_assessment() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.auto_poke_incomplete_todos = true;
        app.is_remote = true;
        let remote_session_id = format!("remote-ownership-{}", std::process::id());
        app.remote_session_id = Some(remote_session_id.clone());

        crate::todo::save_todos(
            &remote_session_id,
            &[crate::todo::TodoItem {
                id: "todo-1".to_string(),
                content: "Ship the complete workflow".to_string(),
                status: "completed".to_string(),
                priority: "high".to_string(),
                group: Some("release".to_string()),
                confidence: Some(crate::todo::ConfidenceState::Verified),
                completion_confidence: Some(crate::todo::ConfidenceState::Verified),
                confidence_history: vec![crate::todo::ConfidenceState::Verified],
                ..Default::default()
            }],
        )
        .expect("save remote completed todo");
        crate::todo::save_goals(
            &remote_session_id,
            &[crate::todo::TodoGoal {
                group: Some("release".to_string()),
                delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
                autonomy: Some(crate::todo::Autonomy::NecessaryFollowthrough),
                iteration_maturity: Some(crate::todo::IterationMaturity::OutcomeReached),
                feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::Representative),
                feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::MainPaths),
                feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
                trade_off: Some(crate::todo::TradeOffState::SomeConsidered),
                ..Default::default()
            }],
        )
        .expect("save remote goal assessment");

        // The remote session's goal assessment clears the ownership gate, so
        // no ownership continuation is queued. Post-1bd235b5f the clean
        // completion path still returns true because it requests the concise
        // final-response turn ("All todos done"), not a poke.
        assert!(app.schedule_auto_poke_followup_if_needed());
        assert_eq!(
            app.queued_messages,
            vec![crate::todo::TODO_FINAL_RESPONSE_CONTINUATION_MESSAGE.to_string()],
            "a passing remote goal yields the final-response turn, not a poke"
        );
    });
}

#[test]
fn test_save_input_for_reload_removes_stale_file_when_state_is_empty() {
    let mut app = create_test_app();
    let session_id = format!("test-391-stale-{}", std::process::id());

    // First reload snapshot holds a queued message.
    app.queued_messages.push("old queued".to_string());
    app.save_input_for_reload(&session_id);

    let path = crate::storage::jcode_dir()
        .expect("jcode dir")
        .join(format!("client-input-{}", session_id));
    assert!(path.exists(), "first save should write the reload file");

    // An empty save while the file is FRESH must preserve it: another client
    // attached to the same session may have just saved its own queued
    // messages during the same reload handoff.
    app.queued_messages.clear();
    app.save_input_for_reload(&session_id);
    assert!(
        path.exists(),
        "an empty save must not delete a fresh reload file (multi-client safety)"
    );

    // Backdate the file past the staleness window; now an empty save must
    // remove it so a long-stale queue cannot resurrect on a later restore.
    #[cfg(unix)]
    {
        let stale_age_secs = 400; // > 300s staleness cutoff
        let target = std::time::SystemTime::now() - Duration::from_secs(stale_age_secs);
        let since_epoch = target
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("clock before epoch");
        let times = [
            libc::timespec {
                tv_sec: since_epoch.as_secs() as libc::time_t,
                tv_nsec: 0,
            },
            libc::timespec {
                tv_sec: since_epoch.as_secs() as libc::time_t,
                tv_nsec: 0,
            },
        ];
        let c_path = std::ffi::CString::new(path.to_str().expect("utf8 path")).expect("c path");
        let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(rc, 0, "backdating the reload file mtime should succeed");

        app.save_input_for_reload(&session_id);
        assert!(
            !path.exists(),
            "an empty save must remove a stale reload file so old queued messages cannot resurrect"
        );
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::remove_file(&path);
    }
}

/// Repeated provider guardrail refusals must trip the circuit breaker and
/// disarm auto-poke instead of re-sending the refused request forever
/// (observed live: `[guardrail] ... refusal` alternating with
/// `Auto-poking: N incomplete todos` every ~7s, one refused API call each).
#[test]
fn test_repeated_guardrail_refusals_stop_auto_poke_loop() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let mut remote = crate::tui::backend::RemoteConnection::dummy();
        remote.mark_history_loaded();

        app.is_remote = true;
        app.auto_poke_incomplete_todos = true;

        crate::todo::save_todos(
            &app.session.id,
            &[crate::todo::TodoItem {
                id: "todo-1".to_string(),
                content: "Never-finishing task".to_string(),
                status: "in_progress".to_string(),
                priority: "high".to_string(),
                ..Default::default()
            }],
        )
        .expect("save incomplete todo");

        let run_refused_turn = |app: &mut App, remote: &mut _, id: u64| {
            app.is_processing = true;
            app.status = ProcessingStatus::Streaming;
            app.current_message_id = Some(id);
            app.handle_server_event(
                crate::protocol::ServerEvent::ProviderGuardrail {
                    stop_reason: Some("refusal".to_string()),
                    message: "Provider guardrail stopped the response (stop_reason: refusal)."
                        .to_string(),
                },
                remote,
            );
            app.handle_server_event(crate::protocol::ServerEvent::Done { id }, remote);
            // Simulate the queued poke actually dispatching before the next turn.
            app.queued_messages.clear();
            app.hidden_queued_system_messages.clear();
            app.pending_queued_dispatch = false;
        };

        // First refusal: still under budget, auto-poke may schedule again.
        run_refused_turn(&mut app, &mut remote, 1);
        assert!(
            app.auto_poke_incomplete_todos,
            "one refusal alone must not disarm auto-poke"
        );
        assert_eq!(app.consecutive_guardrail_stops, 1);

        // Second consecutive refusal: circuit breaker must trip.
        run_refused_turn(&mut app, &mut remote, 2);
        assert!(
            !app.auto_poke_incomplete_todos,
            "repeated refusals must disarm auto-poke"
        );
        assert!(
            app.queued_messages.is_empty(),
            "no poke follow-up may stay queued after the breaker trips"
        );
        assert!(
            app.display_messages()
                .iter()
                .any(|m| m.role == "system" && m.content.contains("refused")),
            "the user should be told why auto-poke stopped"
        );

        // A successful turn resets the streak once auto-poke is re-armed.
        commands::activate_auto_poke(&mut app);
        assert_eq!(app.consecutive_guardrail_stops, 0);
        app.is_processing = true;
        app.status = ProcessingStatus::Streaming;
        app.current_message_id = Some(3);
        app.handle_server_event(crate::protocol::ServerEvent::Done { id: 3 }, &mut remote);
        assert_eq!(app.consecutive_guardrail_stops, 0);
        assert!(
            app.auto_poke_incomplete_todos,
            "a clean turn must keep auto-poke armed"
        );
    });
}

/// The deferred quality review must fire at turn end, and must re-arm when the
/// auto-poke cycle finishes. Without the re-arm a session could only ever
/// deliver one digest, so later work would silently lose the review.
#[test]
fn test_gate_digest_is_delivered_at_turn_end_and_rearms_next_cycle() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.auto_poke_incomplete_todos = true;

        // Completed work with validated confidence, so the completion gate is
        // satisfied and the digest is the only thing left to say.
        crate::todo::save_todos(
            &app.session.id,
            &[crate::todo::TodoItem {
                id: "todo-1".to_string(),
                content: "Speed up the parser".to_string(),
                status: "completed".to_string(),
                priority: "high".to_string(),
                confidence: Some(crate::todo::ConfidenceState::from_legacy_score(100)),
                completion_confidence: Some(crate::todo::ConfidenceState::from_legacy_score(100)),
                confidence_history: vec![
                    crate::todo::ConfidenceState::from_legacy_score(100),
                    crate::todo::ConfidenceState::from_legacy_score(100),
                ],
                ..Default::default()
            }],
        )
        .expect("save completed todo");

        crate::todo::save_goals(
            &app.session.id,
            &[crate::todo::TodoGoal {
                delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
                autonomy: Some(crate::todo::Autonomy::NecessaryFollowthrough),
                iteration_maturity: Some(crate::todo::IterationMaturity::OutcomeReached),
                feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::Representative),
                feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::MainPaths),
                feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
                trade_off: Some(crate::todo::TradeOffState::SomeConsidered),
                ..Default::default()
            }],
        )
        .expect("save passing goal");

        // A point that was flagged during the turn and never resolved.
        crate::todo::append_gate_observations(
            &app.session.id,
            &[crate::todo::GateObservation {
                kind: crate::todo::GateObservationKind::IntentUnderstanding,
                group: None,
                state: Some(
                    crate::todo::IntentUnderstanding::from_legacy_score(70)
                        .as_str()
                        .to_string(),
                ),
            }],
        )
        .expect("record observation");

        assert!(
            app.schedule_auto_poke_followup_if_needed(),
            "an unresolved review point should schedule the digest"
        );
        let digest = app
            .queued_messages
            .last()
            .expect("digest should be queued")
            .clone();
        assert!(digest.starts_with(crate::todo::TODO_GATE_DIGEST_PREFIX));
        assert!(app.todo_gate_digest_delivered);
        // Consumed, so the same points cannot be raised twice.
        assert!(
            crate::todo::load_gate_observations(&app.session.id)
                .expect("reload")
                .is_empty()
        );

        // Simulate the turn running, then the cycle completing.
        app.queued_messages.clear();
        app.pending_queued_dispatch = false;
        // With the digest consumed and no other outstanding point, the clean
        // cycle closes by queuing the concise final response.
        assert!(
            app.schedule_auto_poke_followup_if_needed(),
            "after the digest, the cycle should queue the final response"
        );
        assert_eq!(
            app.queued_messages,
            vec![crate::todo::TODO_FINAL_RESPONSE_CONTINUATION_MESSAGE.to_string()]
        );
        assert!(
            !app.todo_gate_digest_delivered,
            "a finished cycle must re-arm the review for later work"
        );
    });
}

// Regression: auto-poke is on by default (`features.auto_poke`), but the
// scheduler used to disarm it permanently the first time a turn ended with no
// todo list at all, and again whenever a cycle completed. In practice that
// meant the default-on feature switched itself off within the first few turns
// of every session and never poked again, which is exactly what the logs
// showed (`AUTO_POKE_DECISION action=disarm reason=no_todos` on nearly every
// session, followed by no pokes for the rest of the day).
#[test]
fn auto_poke_stays_armed_when_a_turn_has_no_todos() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.auto_poke_incomplete_todos = true;
        app.auto_poke_default_on = true;

        assert!(
            !app.schedule_auto_poke_followup_if_needed(),
            "no todos means nothing to poke about this turn"
        );
        assert!(
            app.auto_poke_incomplete_todos,
            "a todo-free turn must not disable the default-on feature"
        );

        // Later work with incomplete todos must still be poked.
        crate::todo::save_todos(
            &app.session.id,
            &[crate::todo::TodoItem {
                id: "todo-1".to_string(),
                content: "Finish the thing".to_string(),
                status: "pending".to_string(),
                priority: "high".to_string(),
                confidence: Some(crate::todo::ConfidenceState::from_legacy_score(80)),
                ..Default::default()
            }],
        )
        .expect("save incomplete todo");

        assert!(
            app.schedule_auto_poke_followup_if_needed(),
            "incomplete todos on a later turn must still schedule a poke"
        );
    });
}

#[test]
fn auto_poke_does_not_repeat_until_incomplete_todos_change() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.auto_poke_incomplete_todos = true;
        let pending = |content: &str| crate::todo::TodoItem {
            id: "todo-1".to_string(),
            content: content.to_string(),
            status: "pending".to_string(),
            priority: "high".to_string(),
            confidence: Some(crate::todo::ConfidenceState::from_legacy_score(80)),
            ..Default::default()
        };

        crate::todo::save_todos(&app.session.id, &[pending("Wait for worker")]).expect("save");
        assert!(app.schedule_auto_poke_followup_if_needed());

        // Simulate dispatch and completion of the automatically poked turn.
        app.queued_messages.clear();
        app.pending_queued_dispatch = false;
        assert!(
            !app.schedule_auto_poke_followup_if_needed(),
            "an unchanged list must not consume another model turn"
        );

        crate::todo::save_todos(&app.session.id, &[pending("Review worker result")])
            .expect("update");
        assert!(
            app.schedule_auto_poke_followup_if_needed(),
            "changing the todo list must re-arm the automatic nudge"
        );

        app.queued_messages.clear();
        app.pending_queued_dispatch = false;
        crate::todo::save_todos(&app.session.id, &[]).expect("finish cycle");
        assert!(!app.schedule_auto_poke_followup_if_needed());
        crate::todo::save_todos(&app.session.id, &[pending("Review worker result")])
            .expect("start equivalent new cycle");
        assert!(
            app.schedule_auto_poke_followup_if_needed(),
            "an equivalent todo in a new cycle must receive one fresh nudge"
        );
    });
}

#[test]
fn completed_cycle_rearms_auto_poke_only_when_default_on() {
    with_temp_jcode_home(|| {
        let completed = |id: &str| crate::todo::TodoItem {
            id: id.to_string(),
            content: "Done".to_string(),
            status: "completed".to_string(),
            priority: "high".to_string(),
            confidence: Some(crate::todo::ConfidenceState::from_legacy_score(100)),
            completion_confidence: Some(crate::todo::ConfidenceState::from_legacy_score(100)),
            confidence_history: vec![
                crate::todo::ConfidenceState::Validated,
                crate::todo::ConfidenceState::Verified,
            ],
            ..Default::default()
        };

        let mut app = create_test_app();
        app.auto_poke_incomplete_todos = true;
        app.auto_poke_default_on = true;
        crate::todo::save_todos(&app.session.id, &[completed("todo-1")]).expect("save");
        crate::todo::save_goals(
            &app.session.id,
            &[crate::todo::TodoGoal {
                delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
                autonomy: Some(crate::todo::Autonomy::NecessaryFollowthrough),
                iteration_maturity: Some(crate::todo::IterationMaturity::OutcomeReached),
                feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::Representative),
                feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::MainPaths),
                feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
                trade_off: Some(crate::todo::TradeOffState::SomeConsidered),
                ..Default::default()
            }],
        )
        .expect("save passing goal");
        // Post-1bd235b5f a clean completion returns true because it requests
        // the concise final-response turn; it is not a poke or a gate retry.
        assert!(app.schedule_auto_poke_followup_if_needed());
        assert_eq!(
            app.queued_messages,
            vec![crate::todo::TODO_FINAL_RESPONSE_CONTINUATION_MESSAGE.to_string()]
        );
        assert!(
            app.auto_poke_incomplete_todos,
            "default-on auto-poke should cover the next batch of work too"
        );

        // An explicit /poke off must stick.
        let mut app = create_test_app();
        app.auto_poke_incomplete_todos = true;
        app.auto_poke_default_on = true;
        crate::tui::app::commands::disable_auto_poke(&mut app);
        crate::todo::save_todos(&app.session.id, &[completed("todo-2")]).expect("save");
        crate::todo::save_goals(
            &app.session.id,
            &[crate::todo::TodoGoal {
                delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
                autonomy: Some(crate::todo::Autonomy::NecessaryFollowthrough),
                iteration_maturity: Some(crate::todo::IterationMaturity::OutcomeReached),
                feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::Representative),
                feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::MainPaths),
                feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
                trade_off: Some(crate::todo::TradeOffState::SomeConsidered),
                ..Default::default()
            }],
        )
        .expect("save passing goal");
        app.auto_poke_incomplete_todos = true; // pretend a stale arm survived
        // Even though a stale arm lets the call proceed and it returns true to
        // request the final-response turn, the disarmed default must win: the
        // default-on re-arm never re-arms because auto_poke_default_on is false.
        assert!(app.schedule_auto_poke_followup_if_needed());
        assert!(
            !app.auto_poke_incomplete_todos,
            "/poke off must not be undone by the default-on re-arm"
        );
    });
}

#[test]
fn gate_budget_resets_when_gated_goal_state_progresses_between_pokes() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.auto_poke_incomplete_todos = true;

        // A completed todo whose goal is currently short of the delivery gate
        // (trade_off missing AND coverage too narrow for an "involved" goal).
        crate::todo::save_todos(
            &app.session.id,
            &[crate::todo::TodoItem {
                id: "todo-1".to_string(),
                content: "Ship the involved workflow".to_string(),
                status: "completed".to_string(),
                priority: "high".to_string(),
                group: Some("release".to_string()),
                confidence: Some(crate::todo::ConfidenceState::Verified),
                completion_confidence: Some(crate::todo::ConfidenceState::Verified),
                confidence_history: vec![crate::todo::ConfidenceState::Verified],
                ..Default::default()
            }],
        )
        .expect("save completed todo");

        let goal = |trade_off: Option<crate::todo::TradeOffState>,
                     coverage: Option<crate::todo::FeedbackLoopCoverage>|
         -> crate::todo::TodoGoal {
            crate::todo::TodoGoal {
                group: Some("release".to_string()),
                difficulty: Some(crate::todo::Difficulty::Involved),
                delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
                autonomy: Some(crate::todo::Autonomy::NecessaryFollowthrough),
                iteration_maturity: Some(crate::todo::IterationMaturity::OutcomeReached),
                feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::AcceptanceAligned),
                feedback_loop_coverage: coverage,
                feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
                trade_off,
                ..Default::default()
            }
        };

        // Simulate a model converging gate-by-gate across many turns. Each turn
        // it makes progress on the gated goal state (trade_off climbs, then
        // coverage climbs) but has not yet cleared every gate, so auto-poke
        // keeps raising a continuation. Because the state is genuinely moving,
        // the gate budget must keep resetting; only an unchanged, stalled state
        // may exhaust the circuit breaker.
        //
        // For an "involved" goal both trade_off >= diligent and coverage >=
        // edge_and_integration_paths are required to pass.
        let states: Vec<(crate::todo::TradeOffState, crate::todo::FeedbackLoopCoverage)> = vec![
            // Turn 1: everything low.
            (crate::todo::TradeOffState::NoneConsidered, crate::todo::FeedbackLoopCoverage::Narrow),
            // Turn 2: agent improved after the first poke (progress #1).
            (crate::todo::TradeOffState::Implicit, crate::todo::FeedbackLoopCoverage::Narrow),
            // Turn 3: agent improved again (progress #2) - still gated.
            (crate::todo::TradeOffState::SomeConsidered, crate::todo::FeedbackLoopCoverage::MainPaths),
            // Turn 4: agent improved yet again (progress #3).
            (crate::todo::TradeOffState::Diligent, crate::todo::FeedbackLoopCoverage::EdgeAndIntegrationPaths),
        ];

        // Turn 1 arm.
        crate::todo::save_goals(
            &app.session.id,
            &[goal(Some(states[0].0), Some(states[0].1))],
        )
        .expect("save turn-1 goal");
        assert!(
            app.schedule_auto_poke_followup_if_needed(),
            "turn 1 gates and must schedule a continuation"
        );
        assert!(
            app.auto_poke_incomplete_todos,
            "a progressing gate must not disarm"
        );
        assert_eq!(app.todo_ownership_gate_attempts, 1);

        // Turns 2..N: each makes the gated goal state strictly better. The
        // budget must reset on each progress step, so the model may always get
        // a fresh runway and is never disarmed while it keeps converging.
        for (idx, &state) in states.iter().enumerate().skip(1) {
            // Simulate the previous continuation having been dispatched and run.
            app.queued_messages.clear();
            app.pending_queued_dispatch = false;

            crate::todo::save_goals(
                &app.session.id,
                &[goal(Some(state.0), Some(state.1))],
            )
            .expect("save progressed goal");

            if idx == states.len() - 1 {
                // Final state clears every gate (diligent + edge coverage), so
                // the cycle finishes cleanly instead of poking again.
                assert!(app.schedule_auto_poke_followup_if_needed());
                assert!(
                    app.pending_queued_dispatch
                        || app.queued_messages
                            == vec![crate::todo::TODO_FINAL_RESPONSE_CONTINUATION_MESSAGE.to_string()],
                    "cleared gate should reach the final-response handoff"
                );
            } else {
                // Still gated, but progress was made: budget resets and the
                // next poke is armed without disarming.
                assert!(
                    app.schedule_auto_poke_followup_if_needed(),
                    "turn {} still gates and must keep offering runway", idx + 1
                );
                assert!(
                    app.auto_poke_incomplete_todos,
                    "turn {} must not disarm while the gated state progresses", idx + 1
                );
                assert_eq!(app.todo_ownership_gate_attempts, 1,
                    "turn {} must get a fresh budget after progress", idx + 1
                );
            }
        }
    });
}

#[test]
fn gate_budget_does_not_reset_when_only_unrelated_fields_churn() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.auto_poke_incomplete_todos = true;

        // A completed todo whose goal has a genuinely-stuck gate: trade_off
        // stays None (required for an "involved" goal), so delivery never
        // passes no matter how much unrelated content churns.
        crate::todo::save_todos(
            &app.session.id,
            &[crate::todo::TodoItem {
                id: "todo-1".to_string(),
                content: "Stuck gate workflow".to_string(),
                status: "completed".to_string(),
                priority: "high".to_string(),
                group: Some("release".to_string()),
                completion_confidence: Some(crate::todo::ConfidenceState::Verified),
                ..Default::default()
            }],
        )
        .expect("save completed todo");

        let stuck_goal = || crate::todo::TodoGoal {
            group: Some("release".to_string()),
            difficulty: Some(crate::todo::Difficulty::Involved),
            delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
            autonomy: Some(crate::todo::Autonomy::NecessaryFollowthrough),
            iteration_maturity: Some(crate::todo::IterationMaturity::OutcomeReached),
            feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::AcceptanceAligned),
            feedback_loop_coverage: Some(
                crate::todo::FeedbackLoopCoverage::EdgeAndIntegrationPaths,
            ),
            feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
            trade_off: None, // the stuck gate
            ..Default::default()
        };

        crate::todo::save_goals(&app.session.id, &[stuck_goal()]).expect("save stuck goal");

        // Drive the gate to exhaustion: 5 identical nudges with no gated-state
        // progress. Only unrelated content changes between turns.
        for attempt in 0..App::TODO_COMPLETION_GATE_MAX_ATTEMPTS {
            assert!(
                app.schedule_auto_poke_followup_if_needed(),
                "attempt {attempt} should still schedule a gate nudge"
            );
            app.queued_messages.clear();
            app.pending_queued_dispatch = false;
            // Unrelated churn: reword the todo content (not gated) but keep the
            // gated goal assessment identical.
            let mut todos = crate::todo::load_todos(&app.session.id).expect("load todos");
            todos[0].content = format!("Unrelated rewording #{}", attempt + 1);
            crate::todo::save_todos(&app.session.id, &todos).expect("rewrite todo");
        }

        // The stuck gate must still exhaust its budget and disarm, because
        // unrelated content churn is not progress on the gate.
        assert!(
            !app.schedule_auto_poke_followup_if_needed(),
            "stuck gate plus unrelated churn must still trip the circuit breaker"
        );
        assert!(
            !app.auto_poke_incomplete_todos,
            "a stuck gate must disarm even when unrelated fields churn"
        );
    });
}

#[test]
fn gate_budget_ignores_goal_for_non_completed_group_when_driving_stuck_gate() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.auto_poke_incomplete_todos = true;

        // A completed todo in the "release" group with a genuinely-stuck gate:
        // trade_off stays None (required for an involved goal), so "release"
        // never passes its delivery gate.
        crate::todo::save_todos(
            &app.session.id,
            &[
                crate::todo::TodoItem {
                    id: "release-1".to_string(),
                    content: "Stuck release".to_string(),
                    status: "completed".to_string(),
                    priority: "high".to_string(),
                    group: Some("release".to_string()),
                    completion_confidence: Some(crate::todo::ConfidenceState::Verified),
                    ..Default::default()
                },
                crate::todo::TodoItem {
                    id: "wip-1".to_string(),
                    content: "Cancelled work".to_string(),
                    // A cancelled todo is not "incomplete" (`is_incomplete_poke
                    // _todo` excludes cancelled), so the completion-gate branch
                    // still runs. But `group_is_complete` requires status ==
                    // "completed", so the "wip" group is NOT completed and its
                    // goal is not gated by the ownership gate.
                    status: "cancelled".to_string(),
                    priority: "high".to_string(),
                    group: Some("wip".to_string()),
                    ..Default::default()
                },
            ],
        )
        .expect("save completed + in-progress todos");

        let release_goal = || crate::todo::TodoGoal {
            group: Some("release".to_string()),
            difficulty: Some(crate::todo::Difficulty::Involved),
            delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
            autonomy: Some(crate::todo::Autonomy::NecessaryFollowthrough),
            iteration_maturity: Some(crate::todo::IterationMaturity::OutcomeReached),
            feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::AcceptanceAligned),
            feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::EdgeAndIntegrationPaths),
            feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
            trade_off: None, // the stuck gate on the completed group
            ..Default::default()
        };
        let wip_goal = |iteration: crate::todo::IterationMaturity| crate::todo::TodoGoal {
            group: Some("wip".to_string()),
            difficulty: Some(crate::todo::Difficulty::Routine),
            delivery_state: Some(crate::todo::DeliveryState::ChangeMade),
            iteration_maturity: Some(iteration), // churns below
            ..Default::default()
        };

        crate::todo::save_goals(
            &app.session.id,
            &[release_goal(), wip_goal(crate::todo::IterationMaturity::Exploring)],
        )
        .expect("save release + wip goals");

        // The "wip" group contains a cancelled (not completed) todo, so it is
        // NOT completed and its goal is not gated by the ownership gate.
        // Churning its assessment must not reset the stuck "release" gate's
        // budget.
        for attempt in 0..App::TODO_COMPLETION_GATE_MAX_ATTEMPTS {
            assert!(
                app.schedule_auto_poke_followup_if_needed(),
                "attempt {attempt} should still schedule the release gate nudge"
            );
            app.queued_messages.clear();
            app.pending_queued_dispatch = false;
            // Churn the non-completed "wip" group's non-gated assessment.
            crate::todo::save_goals(
                &app.session.id,
                &[
                    release_goal(),
                    wip_goal(if attempt % 2 == 0 {
                        crate::todo::IterationMaturity::Exploring
                    } else {
                        crate::todo::IterationMaturity::Improving
                    }),
                ],
            )
            .expect("save churned goals");
        }

        // The stuck "release" gate must still reach exhaustion and disarm,
        // because churning a non-completed group's goal is not progress on it.
        assert!(
            !app.schedule_auto_poke_followup_if_needed(),
            "non-completed group goal churn must not keep resetting the stuck gate budget"
        );
        assert!(!app.auto_poke_incomplete_todos);
    });
}

#[test]
fn per_gate_fingerprints_ignore_order_and_unrelated_content() {
    let mut todo_a = crate::todo::TodoItem {
        id: "a".to_string(),
        content: "first".to_string(),
        status: "completed".to_string(),
        group: Some("g".to_string()),
        ..Default::default()
    };
    todo_a.completion_confidence = Some(crate::todo::ConfidenceState::Validated);
    let mut todo_b = crate::todo::TodoItem {
        id: "b".to_string(),
        content: "second".to_string(),
        status: "completed".to_string(),
        group: Some("g".to_string()),
        ..Default::default()
    };
    todo_b.completion_confidence = Some(crate::todo::ConfidenceState::Verified);
    let goals = vec![crate::todo::TodoGoal {
        group: Some("g".to_string()),
        delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
        trade_off: Some(crate::todo::TradeOffState::SomeConsidered),
        ..Default::default()
    }];

    // Ownership fingerprint: same gated goal state, different goal order ->
    // identical fingerprint (todo content/order is not part of the ownership
    // signal; only goal assessments for completed groups are).
    let same = <App>::ownership_gate_fingerprint(&[todo_a.clone(), todo_b.clone()], &goals);
    let shuffled_goals = {
        let mut g = goals.clone();
        g.reverse();
        g
    };
    assert_eq!(
        same,
        <App>::ownership_gate_fingerprint(&[todo_b.clone(), todo_a.clone()], &shuffled_goals),
        "reordering completed todos or goals must not change the ownership fingerprint"
    );

    // Advancing a goal-assessment field DOES change the ownership fingerprint.
    let mut better_goal = goals[0].clone();
    better_goal.trade_off = Some(crate::todo::TradeOffState::Diligent);
    assert_ne!(
        same,
        <App>::ownership_gate_fingerprint(&[todo_a.clone()], &[better_goal]),
        "advancing a goal assessment must change the ownership fingerprint"
    );

    // Grouping matters: a goal for a NON-completed (in-progress) group is not
    // gated, so its assessment must not be part of the ownership fingerprint.
    let mut wip_todo = todo_a.clone();
    wip_todo.id = "wip-1".to_string();
    wip_todo.status = "in_progress".to_string();
    wip_todo.group = Some("wip".to_string());
    let mut wip_goal = goals[0].clone();
    wip_goal.group = Some("wip".to_string());
    wip_goal.trade_off = Some(crate::todo::TradeOffState::Diligent);
    let fp_with_gated = <App>::ownership_gate_fingerprint(
        &[todo_a.clone(), wip_todo],
        &[goals[0].clone(), wip_goal],
    );
    let fp_ignoring_wip = <App>::ownership_gate_fingerprint(&[todo_a.clone()], &goals);
    assert_eq!(
        fp_with_gated, fp_ignoring_wip,
        "a non-completed group's goal assessment must not enter the ownership fingerprint"
    );

    // Confidence fingerprint: same completed-todo state, different order and
    // wording -> identical fingerprint.
    let ab = [todo_a.clone(), todo_b.clone()];
    let ba = [todo_b.clone(), todo_a.clone()];
    let a_renamed = [
        crate::todo::TodoItem {
            content: "totally different wording".to_string(),
            ..todo_a.clone()
        },
        todo_b.clone(),
    ];
    let c1 = <App>::confidence_gate_fingerprint(&ab);
    let c2 = <App>::confidence_gate_fingerprint(&ba);
    let c3 = <App>::confidence_gate_fingerprint(&a_renamed);
    assert_eq!(
        c1, c2,
        "reordering completed todos must not change the confidence fingerprint"
    );
    assert_eq!(
        c1, c3,
        "rewording a completed todo must not change the confidence fingerprint"
    );

    // Advancing the confidence state DOES change the confidence fingerprint:
    // todo_a goes from Validated to Verified.
    let mut a_up = todo_a.clone();
    a_up.completion_confidence = Some(crate::todo::ConfidenceState::Verified);
    let c4 = <App>::confidence_gate_fingerprint(&[a_up, todo_b.clone()]);
    assert_ne!(
        c1, c4,
        "advancing completion confidence must change the confidence fingerprint"
    );

    // The confidence gate's weighted average uses priority as its weight, so a
    // priority change is also a gated-state change and must register as progress.
    let mut a_priority = todo_a.clone();
    a_priority.priority = "critical".to_string();
    let c5 = <App>::confidence_gate_fingerprint(&[a_priority, todo_b.clone()]);
    assert_ne!(
        c5, c1,
        "a completed todo's priority change must change the confidence fingerprint"
    );
}

#[test]
fn confidence_progress_does_not_mask_an_ownership_gate_stall() {
    with_temp_jcode_home(|| {
        let mut app = create_test_app();
        app.auto_poke_incomplete_todos = true;

        // A completed todo in group "release" whose goal is genuinely stuck:
        // trade_off stays None (required for an involved goal), so the
        // ownership gate never passes.
        crate::todo::save_todos(
            &app.session.id,
            &[crate::todo::TodoItem {
                id: "todo-1".to_string(),
                content: "Stuck ownership".to_string(),
                status: "completed".to_string(),
                priority: "high".to_string(),
                group: Some("release".to_string()),
                completion_confidence: Some(crate::todo::ConfidenceState::from_legacy_score(40)),
                ..Default::default()
            }],
        )
        .expect("save completed todo");

        let stuck_goal = || crate::todo::TodoGoal {
            group: Some("release".to_string()),
            difficulty: Some(crate::todo::Difficulty::Involved),
            delivery_state: Some(crate::todo::DeliveryState::WorkflowValidated),
            autonomy: Some(crate::todo::Autonomy::NecessaryFollowthrough),
            iteration_maturity: Some(crate::todo::IterationMaturity::OutcomeReached),
            feedback_loop_relevance: Some(crate::todo::FeedbackLoopRelevance::AcceptanceAligned),
            feedback_loop_coverage: Some(crate::todo::FeedbackLoopCoverage::EdgeAndIntegrationPaths),
            feedback_loop_traceability: Some(crate::todo::FeedbackLoopTraceability::Complete),
            trade_off: Some(crate::todo::TradeOffState::NoneConsidered), // stuck: needs diligent
            ..Default::default()
        };
        crate::todo::save_goals(&app.session.id, &[stuck_goal()]).expect("save stuck goal");

        // Turn 1: the ownership gate schedules a nudge (ownership budget = 1).
        assert!(app.schedule_auto_poke_followup_if_needed());
        assert_eq!(app.todo_ownership_gate_attempts, 1);
        app.queued_messages.clear();
        app.pending_queued_dispatch = false;

        // Turn 2: the model makes progress ONLY on completion confidence
        // (not on the stuck ownership goal), clearing the confidence gate. This
        // changes the confidence fingerprint but NOT the ownership fingerprint.
        // With the old shared fingerprint, this would reset the ownership budget
        // and mask the ownership stall. With per-gate fingerprints it must NOT.
        crate::todo::save_todos(
            &app.session.id,
            &[crate::todo::TodoItem {
                id: "todo-1".to_string(),
                content: "Stuck ownership".to_string(),
                status: "completed".to_string(),
                priority: "high".to_string(),
                group: Some("release".to_string()),
                completion_confidence: Some(crate::todo::ConfidenceState::Verified),
                ..Default::default()
            }],
        )
        .expect("save confidence-cleared todo");
        crate::todo::save_goals(&app.session.id, &[stuck_goal()]).expect("save same stuck goal");

        // Ownership gate still fails and its budget must NOT have been reset by
        // the confidence progress.
        assert!(app.schedule_auto_poke_followup_if_needed());
        assert_eq!(
            app.todo_ownership_gate_attempts, 2,
            "confidence progress must not reset the ownership gate budget"
        );

        // Drive the ownership gate to exhaustion. Confidence progress on later
        // turns must never extend the ownership runway.
        for attempt in 2..App::TODO_COMPLETION_GATE_MAX_ATTEMPTS {
            app.queued_messages.clear();
            app.pending_queued_dispatch = false;
            assert!(
                app.schedule_auto_poke_followup_if_needed(),
                "attempt {attempt} should still schedule an ownership nudge"
            );
        }
        app.queued_messages.clear();
        app.pending_queued_dispatch = false;
        assert!(
            !app.schedule_auto_poke_followup_if_needed(),
            "stuck ownership gate must still exhaust its own budget and disarm"
        );
        assert!(!app.auto_poke_incomplete_todos);
    });
}

#[test]
fn confidence_fingerprint_ignores_history_appends_that_keep_the_same_spike_tail() {
    // The confidence gate's spike detection reads only the last two
    // confidence_history entries. Appending an entry while the last two stay
    // identical must NOT register as gate progress, or a stuck confidence gate
    // could keep resetting its budget by re-appending the same tail.
    let base = crate::todo::TodoItem {
        id: "a".to_string(),
        content: "same".to_string(),
        status: "completed".to_string(),
        confidence: Some(crate::todo::ConfidenceState::from_legacy_score(50)),
        completion_confidence: Some(crate::todo::ConfidenceState::from_legacy_score(50)),
        confidence_history: vec![crate::todo::ConfidenceState::from_legacy_score(30)],
        ..Default::default()
    };

    // [x, y, y] and [x, y, y, y] both read the same spike-relevant tail (y, y):
    // appending while the tail stays equal must not change the fingerprint.
    let mut short = base.clone();
    short.confidence_history = vec![
        crate::todo::ConfidenceState::from_legacy_score(30),
        crate::todo::ConfidenceState::from_legacy_score(50),
        crate::todo::ConfidenceState::from_legacy_score(50),
    ];
    let mut long = base.clone();
    long.confidence_history = vec![
        crate::todo::ConfidenceState::from_legacy_score(30),
        crate::todo::ConfidenceState::from_legacy_score(50),
        crate::todo::ConfidenceState::from_legacy_score(50),
        crate::todo::ConfidenceState::from_legacy_score(50),
    ];
    let fp_short = <App>::confidence_gate_fingerprint(&[short]);
    let fp_long = <App>::confidence_gate_fingerprint(&[long]);
    assert_eq!(
        fp_short, fp_long,
        "appending history without changing the spike-relevant tail must not count as confidence-gate progress"
    );

    // But a genuinely-changed spike tail (a steep jump into the last two)
    // MUST change the fingerprint.
    let mut spiked = base.clone();
    spiked.confidence_history = vec![
        crate::todo::ConfidenceState::from_legacy_score(30),
        crate::todo::ConfidenceState::from_legacy_score(100),
    ];
    let jumped = <App>::confidence_gate_fingerprint(&[spiked]);
    assert_ne!(
        fp_short, jumped,
        "a changed spike-relevant tail must count as confidence-gate progress"
    );
}
