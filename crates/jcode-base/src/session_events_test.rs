#![allow(unused_variables, unused_imports)]

use crate::session::event_types::SessionEventOp;
use crate::session::Session;
use crate::session::event_types::{SessionEvent, SessionEventMap};
use crate::message::ContentBlock;
use jcode_session_types::{StoredCompactionState, StoredMemoryInjection, StoredMessage};
use jcode_message_types::Role;
use chrono::Utc;
use serde_json::json;

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text { text: text.to_string(), cache_control: None }
}

#[test]
fn test_event_map_construction() {
    let mut session = Session::create_with_id(
        "test_session_1".to_string(),
        None,
        Some("Test Session".to_string()),
    );
    assert!(session.event_map.events.len() == 0);

    let test_message = StoredMessage {
        id: "msg_1".to_string(),
        role: Role::User,
        content: vec![text_block("Hello, world!")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };

    session.append_stored_message(test_message.clone());
    assert!(session.messages.len() >= 1);
    assert!(session.event_map.events.len() >= 1);
}

#[test]
fn test_event_map_message_operations() {
    let mut session = Session::create_with_id(
        "test_session_2".to_string(),
        None,
        Some("Test Insert/Replace".to_string()),
    );

    let msg1 = StoredMessage {
        id: "msg_1".to_string(),
        role: Role::User,
        content: vec![text_block("First message")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };
    let msg2 = StoredMessage {
        id: "msg_2".to_string(),
        role: Role::Assistant,
        content: vec![text_block("Second message")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };
    let msg3 = StoredMessage {
        id: "msg_3".to_string(),
        role: Role::User,
        content: vec![text_block("Third message")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };

    session.append_stored_message(msg1);
    session.append_stored_message(msg2);
    session.insert_message(1, msg3.clone());

    assert_eq!(session.messages.len(), 3);
    assert_eq!(session.messages[1].id, "msg_3");
    assert!(session.event_map.events.len() >= 3);
}

#[test]
fn test_insert_message_at_end_keeps_event_log_consistent() {
    // `insert_message` delegates to `Vec::insert`, which accepts `index == len`
    // as an append-at-end. `derive_messages` must replay that identically or the
    // event log desyncs from the legacy vector.
    let mut session = Session::create_with_id("test_insert_end".to_string(), None, None);
    let msg1 = StoredMessage {
        id: "msg1".to_string(),
        role: Role::User,
        content: vec![text_block("one")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };
    let msg_end = StoredMessage {
        id: "end".to_string(),
        role: Role::Assistant,
        content: vec![text_block("end")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };
    session.append_stored_message(msg1.clone());
    // Insert at the exact end (index == current len).
    session.insert_message(session.messages.len(), msg_end.clone());

    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[1].id, "end");
    let derived = session.derive_messages();
    assert_eq!(derived.len(), 2, "append-at-end insert must survive replay");
    assert_eq!(derived[0].id, "msg1");
    assert_eq!(derived[1].id, "end");
    session.rederive_all_checked().expect("event log must agree with legacy vector");
}

#[test]
fn test_insert_message_out_of_range_clamps_like_derive() {
    // `insert_message` with an out-of-range index (`index > len`) must not panic
    // the legacy vector. `derive_messages` clamps an `InsertMessage` to the live
    // length (an end-append), so the legacy path clamps identically to keep the
    // two sources of truth consistent and to avoid a `Vec::insert` panic.
    let mut session = Session::create_with_id("test_insert_oob".to_string(), None, None);
    let msg1 = StoredMessage {
        id: "msg1".to_string(),
        role: Role::User,
        content: vec![text_block("one")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };
    let msg_oob = StoredMessage {
        id: "oob".to_string(),
        role: Role::Assistant,
        content: vec![text_block("out of range")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };
    session.append_stored_message(msg1.clone());
    let len_before = session.messages.len();
    // index far beyond the live length.
    session.insert_message(len_before + 100, msg_oob.clone());

    // Must not panic; the OOB insert clamps to an end-append.
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[1].id, "oob", "OOB insert clamps to end-append");
    let derived = session.derive_messages();
    assert_eq!(derived.len(), 2, "derive and legacy must agree on the clamped insert");
    assert_eq!(derived[1].id, "oob");
    session.rederive_all_checked().expect("event log must agree with legacy vector");
}

#[test]
fn test_event_map_compaction_operations() {
    let mut session = Session::create_with_id(
        "test_session_3".to_string(),
        None,
        Some("Test Compaction".to_string()),
    );

    let compaction = StoredCompactionState {
        summary_text: "Compacted summary".to_string(),
        openai_encrypted_content: Some("encrypted_data".to_string()),
        covers_up_to_turn: 10,
        original_turn_count: 100,
        compacted_count: 5,
    };

    session.set_compaction(compaction.clone());
    assert!(session.compaction.is_some());
    assert_eq!(session.compaction.as_ref().unwrap().covers_up_to_turn, 10);

    assert!(session.event_map.events.iter().any(|e| {
        matches!(e.op, SessionEventOp::SetCompaction { .. })
    }));
}

/// Compaction is a log-bracketed operation (takeaway #5): a `CompactionStart`
/// marker precedes the reconstitution, and a `CompactionEnd` persists the
/// result. A balanced bracket yields no orphan and leaves `current_compaction`
/// equal to the persisted state.
#[test]
fn test_compaction_bracket_balanced_round_trip() {
    let mut map = SessionEventMap::default();
    map.start_compaction("comp_1", 6);

    // Mid-bracket, the surface is *not* yet considered compacted and the open
    // bracket is detectable as an orphan.
    assert!(
        map.orphaned_compaction().is_some(),
        "an open bracket before CompactionEnd must be flagged as orphaned"
    );
    assert_eq!(map.current_compaction(), None);

    let state = StoredCompactionState {
        summary_text: "summary".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 6,
        original_turn_count: 40,
        compacted_count: 6,
    };
    map.end_compaction(state.clone());

    // Closing clears the orphan and persists the state.
    assert!(map.orphaned_compaction().is_none());
    assert_eq!(map.current_compaction(), Some(state));

    // Balanced bracket survives serialization.
    let json = serde_json::to_string(&map).expect("serialize map");
    let back: SessionEventMap = serde_json::from_str(&json).expect("deserialize map");
    assert!(back.orphaned_compaction().is_none());
    match &back.events[0].op {
        SessionEventOp::CompactionStart { compaction_id, covers_up_to_turn } => {
            assert_eq!(compaction_id, "comp_1");
            assert_eq!(*covers_up_to_turn, 6);
        }
        _ => panic!("expected CompactionStart as first event"),
    }
    assert!(matches!(&back.events[1].op, SessionEventOp::CompactionEnd { .. }));
}

/// Every `SessionEventOp` variant must survive a full JSON round trip losslessly,
/// including the newly added `CompactionStart`/`CompactionEnd`/`Unknown`. This
/// pins the wire-fidelity contract of the manual (de)serializer: a value
/// serialize → deserialize → serialize must reproduce byte-identical JSON on the
/// second pass (stability), and the round-tripped value must equal the original.
#[test]
fn test_all_session_event_ops_round_trip_losslessly() {
    let msg = |id: &str| StoredMessage {
        id: id.to_string(),
        role: Role::User,
        content: vec![text_block("hello")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };
    let inj = StoredMemoryInjection {
        summary: "recalled 1 memory".to_string(),
        content: "memory".to_string(),
        count: 1,
        memory_ids: Vec::new(),
        age_ms: None,
        before_message: None,
        timestamp: chrono::Utc::now(),
    };
    let replay = crate::session::StoredReplayEvent {
        timestamp: chrono::Utc::now(),
        kind: crate::session::StoredReplayEventKind::DisplayMessage {
            role: "user".to_string(),
            title: None,
            content: "display".to_string(),
        },
    };
    let compaction = StoredCompactionState {
        summary_text: "sum".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 3,
        original_turn_count: 10,
        compacted_count: 3,
    };

    let ops = vec![
        SessionEventOp::AppendMessage {
            message_id: "m1".to_string(),
            message: msg("m1"),
        },
        SessionEventOp::ReplaceMessages {
            start_index: 0,
            end_index: usize::MAX,
            messages: vec![msg("a"), msg("b")],
        },
        SessionEventOp::InsertMessage {
            index: 1,
            message: msg("i"),
        },
        SessionEventOp::MemoryInjection { memory_injection: inj.clone() },
        SessionEventOp::ReplayEvent { replay_event: replay.clone() },
        SessionEventOp::SetCompaction { compaction: compaction.clone() },
        SessionEventOp::CompactionStart {
            compaction_id: "c1".to_string(),
            covers_up_to_turn: 3,
        },
        SessionEventOp::CompactionEnd { compaction: compaction.clone() },
        SessionEventOp::ClearAll,
        SessionEventOp::Unknown {
            event_type: "plugin/x".to_string(),
            data: serde_json::json!({ "n": 1, "s": "v" }),
        },
    ];

    for (i, op) in ops.iter().enumerate() {
        let json1 = serde_json::to_string(op).expect("serialize");
        let back: SessionEventOp = serde_json::from_str(&json1).expect("deserialize");
        let json2 = serde_json::to_string(&back).expect("re-serialize");
        assert_eq!(
            json1, json2,
            "variant #{i} must serialize stably (second JSON equals first)"
        );
    }
}

/// A crash mid-summarize leaves an open `CompactionStart` with no `CompactionEnd`
/// — the line between "compacted" and "incomplete compaction". `orphaned_compaction`
/// must surface it so replay can stop and report rather than trust a partial result.
#[test]
fn test_compaction_bracket_orphan_detection_on_crash() {
    let mut map = SessionEventMap::default();
    map.start_compaction("comp_crash", 12);

    // Simulate a crash: no CompactionEnd appended.
    let orphan = map
        .orphaned_compaction()
        .expect("an open bracket after a simulated crash must be orphaned");
    match &orphan.op {
        SessionEventOp::CompactionStart { compaction_id, covers_up_to_turn } => {
            assert_eq!(compaction_id, "comp_crash");
            assert_eq!(*covers_up_to_turn, 12);
        }
        _ => panic!("orphan must be the CompactionStart marker"),
    }

    // The invariant registry flags the orphaned bracket.
    let reg = crate::session::InvariantRegistry::builtin();
    let log = reg.check(&map);
    assert!(
        log.violations
            .iter()
            .any(|v| v.invariant == "session.compaction_bracket_balanced"),
        "expected an orphaned-bracket violation, got {:#?}",
        log.violations
    );

    // A balanced pair must not be flagged.
    map.end_compaction(StoredCompactionState {
        summary_text: "s".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 12,
        original_turn_count: 50,
        compacted_count: 12,
    });
    let log = reg.check(&map);
    assert!(
        log.is_green(),
        "balanced bracket should be green, got {:#?}",
        log.violations
    );
}

/// Nested-bracket corruption tolerance: `[Start A, Start B, End B]` leaves A
/// orphaned, but the naive "last Start after depth>0" tracking would wrongly
/// report the *closed* B. `orphaned_compaction` must return the innermost
/// unmatched Start (A), not the closed B.
#[test]
fn test_orphaned_compaction_reports_innermost_unmatched_start() {
    let mut map = SessionEventMap::default();
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "a".to_string(),
        op: SessionEventOp::CompactionStart {
            compaction_id: "outer".to_string(),
            covers_up_to_turn: 5,
        },
        parent_id: None,
        version: 1,
    });
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "b".to_string(),
        op: SessionEventOp::CompactionStart {
            compaction_id: "inner".to_string(),
            covers_up_to_turn: 5,
        },
        parent_id: None,
        version: 1,
    });
    // Close the inner bracket; the OUTER one is the real orphan.
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "end_b".to_string(),
        op: SessionEventOp::CompactionEnd {
            compaction: StoredCompactionState {
                summary_text: "inner".to_string(),
                openai_encrypted_content: None,
                covers_up_to_turn: 5,
                original_turn_count: 20,
                compacted_count: 5,
            },
        },
        parent_id: None,
        version: 1,
    });

    let orphan = map
        .orphaned_compaction()
        .expect("the outer bracket must remain orphaned");
    match &orphan.op {
        SessionEventOp::CompactionStart { compaction_id, .. } => {
            assert_eq!(
                compaction_id, "outer",
                "orphaned_compaction must report the innermost *unmatched* start"
            );
        }
        _ => panic!("orphan must be a CompactionStart marker"),
    }
}

/// A `CompactionEnd` without a preceding `CompactionStart` is itself an orphan
/// (an "unpaired close") and must be flagged by the bracket invariant.
#[test]
fn test_compaction_end_without_start_is_flagged() {
    let mut map = SessionEventMap::default();
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "bare_end".to_string(),
        op: SessionEventOp::CompactionEnd {
            compaction: StoredCompactionState {
                summary_text: "s".to_string(),
                openai_encrypted_content: None,
                covers_up_to_turn: 1,
                original_turn_count: 10,
                compacted_count: 1,
            },
        },
        parent_id: None,
        version: 1,
    });

    let reg = crate::session::InvariantRegistry::builtin();
    let log = reg.check(&map);
    assert!(
        log.violations
            .iter()
            .any(|v| v.invariant == "session.compaction_bracket_balanced"),
        "expected an unpaired-close violation, got {:#?}",
        log.violations
    );
}

#[test]
fn test_event_map_memory_injection_operations() {
    let mut session = Session::create_with_id(
        "test_session_4".to_string(),
        None,
        Some("Test Memory Injection".to_string()),
    );

    session.record_memory_injection(
        "Auto-recalled 3 memories".to_string(),
        "Memory content".to_string(),
        3,
        30000u64,
        vec!["mem_1".to_string(), "mem_2".to_string()],
    );
    assert!(session.memory_injections.len() >= 1);

    assert!(session.event_map.events.iter().any(|e| {
        matches!(e.op, SessionEventOp::MemoryInjection { .. })
    }));
}

#[test]
fn test_event_map_replay_event_operations() {
    let mut session = Session::create_with_id(
        "test_session_5".to_string(),
        None,
        Some("Test Replay Event".to_string()),
    );

    session.record_replay_display_message(
        "system",
        Some("System Notice".to_string()),
        "This is a system notice".to_string(),
    );
    assert!(session.replay_events.len() >= 1);

    assert!(session.event_map.events.iter().any(|e| {
        matches!(e.op, SessionEventOp::ReplayEvent { .. })
    }));
}

#[test]
fn test_event_map_fork_functionality() {
    let mut session = Session::create_with_id(
        "test_session_6".to_string(),
        None,
        Some("Test Fork".to_string()),
    );

    for i in 0..5 {
        let msg = StoredMessage {
            id: format!("msg_{}", i),
            role: Role::User,
            content: vec![text_block(&format!("Message {}", i))],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        };
        session.append_stored_message(msg);
    }

    let fork = session.fork_up_to_boundary(2);

    assert!(fork.id.len() > 0);
    assert!(fork.messages.len() <= 3);
    assert!(fork.event_map.events.len() <= session.event_map.events.len());
}

#[test]
fn test_event_map_rederive_functionality() {
    let mut session = Session::create_with_id(
        "test_session_7".to_string(),
        None,
        Some("Test Re-derive".to_string()),
    );

    let msg1 = StoredMessage {
        id: "msg_1".to_string(),
        role: Role::User,
        content: vec![text_block("First message")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };

    session.append_stored_message(msg1);

    let (derived_messages, _derived_compaction) = session.rederive_all();
    assert_eq!(derived_messages.len(), session.messages.len());

    let (derived_messages2, _) = session.rederive_all();
    assert_eq!(derived_messages.len(), derived_messages2.len());
}

#[test]
fn test_event_map_backward_compatibility() {
    let mut session = Session::create_with_id(
        "test_session_8".to_string(),
        None,
        Some("Test Backward Compatibility".to_string()),
    );

    for i in 0..3 {
        let msg = StoredMessage {
            id: format!("msg_{}", i),
            role: Role::User,
            content: vec![text_block(&format!("Message {}", i))],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        };
        session.append_stored_message(msg);
    }

    assert!(session.messages.len() >= 3);
    assert!(session.event_map.events.len() >= 3);
}

#[test]
fn test_event_op_serialization() {
    use serde_json;

    let append_op = SessionEventOp::AppendMessage {
        message_id: "test_123".to_string(),
        message: StoredMessage {
            id: "msg_123".to_string(),
            role: Role::User,
            content: vec![text_block("hello")],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        },
    };

    let json = serde_json::to_string(&append_op).expect("Serialization failed");
    let deserialized: SessionEventOp = serde_json::from_str(&json).expect("Deserialization failed");

    match (append_op, deserialized) {
        (SessionEventOp::AppendMessage { .. }, SessionEventOp::AppendMessage { .. }) => {}
        _ => panic!("Deserialized op doesn't match original"),
    }
}

/// Escape hatch (takeaway #13): an unknown `op` tag must deserialize into
/// `Unknown` with its full payload preserved, rather than erroring, so a future
/// plugin can add event kinds without editing the core enum.
#[test]
fn test_unknown_op_escape_hatch_preserves_payload() {
    let raw = r#"{"op":"plugin_custom_note","note":"hello","count":3,"nested":{"a":[1,2]}}"#;
    let op: SessionEventOp = serde_json::from_str(raw).expect("unknown op must deserialize");
    match &op {
        SessionEventOp::Unknown { event_type, data } => {
            assert_eq!(event_type, "plugin_custom_note");
            let data = data.as_object().expect("payload must be an object");
            assert_eq!(data.get("note").and_then(|v| v.as_str()), Some("hello"));
            assert_eq!(data.get("count").and_then(|v| v.as_u64()), Some(3));
            assert!(data.contains_key("nested"));
            // The `op` key itself must not be duplicated inside the payload.
            assert!(data.get("op").is_none());
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
    // Round-trip must re-emit the original `op` and all fields.
    let json = serde_json::to_string(&op).expect("serialize Unknown");
    let round: SessionEventOp = serde_json::from_str(&json).expect("round-trip deserialize");
    match (&op, &round) {
        (
            SessionEventOp::Unknown { event_type: a, data: da },
            SessionEventOp::Unknown { event_type: b, data: db },
        ) => {
            assert_eq!(a, b);
            assert_eq!(da, db);
        }
        _ => panic!("round trip changed kind"),
    }
}

/// A plugin event flowing through the session log append/derive path must be
/// tolerated: appending an `Unknown` event keeps the log valid and replayable
/// even though the core does not interpret its payload.
#[test]
fn test_unknown_op_flows_through_event_log() {
    let mut map = SessionEventMap::default();
    let unknown = SessionEventOp::Unknown {
        event_type: "plugin/checkpoint".to_string(),
        data: serde_json::json!({ "turns": 42, "sha": "abc" }),
    };
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "unknown_1".to_string(),
        op: unknown.clone(),
        parent_id: None,
        version: 1,
    });
    // The unknown op does not change the derived transcript but must remain in
    // the append-only log.
    assert_eq!(map.events.len(), 1);
    assert_eq!(map.derive_messages().len(), 0);
    assert!(matches!(&map.events[0].op, SessionEventOp::Unknown { .. }));

    // It must survive a full round trip through serialization of the map.
    let json = serde_json::to_string(&map).expect("serialize map");
    let back: SessionEventMap = serde_json::from_str(&json).expect("deserialize map");
    assert_eq!(back.events.len(), 1);
    match &back.events[0].op {
        SessionEventOp::Unknown { event_type, data } => {
            assert_eq!(event_type, "plugin/checkpoint");
            assert_eq!(data.get("turns").and_then(|v| v.as_u64()), Some(42));
        }
        _ => panic!("unknown op not preserved through map round trip"),
    }
}

/// `rebuild_event_map` must NOT drop log-only plugin `Unknown` events. It is
/// used as a "reconcile from the legacy vectors" tool by several callers (load
/// self-heal, the app-core sanitize-clear path), and dropping `Unknown` events
/// there would silently lose durable plugin data — defeating the escape hatch.
#[test]
fn test_rebuild_event_map_preserves_plugin_unknown_events() {
    let mut session = Session::create_with_id("rebuild_unknown".to_string(), None, None);
    // A real message plus a plugin `Unknown` event.
    let message = StoredMessage {
        id: "m1".to_string(),
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: "hello".to_string(),
            cache_control: None,
        }],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };
    session.append_stored_message(message);
    session.append_session_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "plugin_kept".to_string(),
        op: SessionEventOp::Unknown {
            event_type: "plugin/keep".to_string(),
            data: serde_json::json!({ "k": "v" }),
        },
        parent_id: None,
        version: 1,
    });
    assert!(
        session
            .event_map
            .events
            .iter()
            .any(|e| matches!(e.op, SessionEventOp::Unknown { .. })),
        "precondition: plugin event present"
    );

    // Rebuild from the legacy vectors (simulating reconcile / sanitize-clear).
    session.rebuild_event_map();

    // The plugin `Unknown` event must survive the rebuild.
    assert!(
        session
            .event_map
            .events
            .iter()
            .any(|e| matches!(
                &e.op,
                SessionEventOp::Unknown { event_type, .. } if event_type == "plugin/keep"
            )),
        "rebuild_event_map must preserve plugin Unknown events"
    );
    // Derived state still agrees with the legacy vectors.
    session
        .rederive_all_checked()
        .expect("rebuilt log must stay consistent");
}

/// Unknown ops must still be rejected by validation if their event id is empty,
/// and accepted when the payload is well formed, so plugin events do not
/// silently corrupt the log invariants.
#[test]
fn test_unknown_op_validation() {
    // Empty event id is rejected even for an unknown (plugin) op.
    let mut map = SessionEventMap::default();
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: String::new(),
        op: SessionEventOp::Unknown {
            event_type: "plugin/x".to_string(),
            data: serde_json::json!({}),
        },
        parent_id: None,
        version: 1,
    });
    assert_eq!(map.events.len(), 0, "empty event id must still be rejected");

    // A well-formed unknown op with a valid id is accepted.
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "unknown_ok".to_string(),
        op: SessionEventOp::Unknown {
            event_type: "plugin/x".to_string(),
            data: serde_json::json!({ "k": "v" }),
        },
        parent_id: None,
        version: 1,
    });
    assert_eq!(map.events.len(), 1);

    // A degenerate unknown op with an EMPTY type discriminator is rejected even
    // with a valid id: it matches no known variant and names no future plugin,
    // so the append-only log must not be polluted with an unroutable event.
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "unknown_empty_type".to_string(),
        op: SessionEventOp::Unknown {
            event_type: String::new(),
            data: serde_json::json!({ "k": "v" }),
        },
        parent_id: None,
        version: 1,
    });
    assert_eq!(
        map.events.len(),
        1,
        "empty event_type must be rejected for an Unknown op"
    );
}

#[test]
fn test_session_event_map_indices() {
    let mut session1 = Session::create_with_id(
        "test_session_9".to_string(),
        None,
        Some("Test Event IDs".to_string()),
    );

    let msg1 = StoredMessage {
        id: "msg_1".to_string(),
        role: Role::User,
        content: vec![text_block("placeholder")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };
    session1.append_stored_message(msg1);

    let mut session2 = Session::create_with_id(
        "test_session_10".to_string(),
        None,
        Some("Test Event IDs 2".to_string()),
    );

    let msg2 = StoredMessage {
        id: "msg_2".to_string(),
        role: Role::User,
        content: vec![text_block("placeholder")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };
    session2.append_stored_message(msg2);

    assert!(session1.event_map.events.len() >= 1);
    assert!(session2.event_map.events.len() >= 1);

    let id1 = session1.event_map.events[0].event_id.clone();
    let id2 = session2.event_map.events[0].event_id.clone();
    assert_ne!(id1, id2);
}

#[test]
fn test_session_event_validation_accepts_valid_events() {
    let mut map = SessionEventMap::default();
    let valid_event = SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "msg_valid".to_string(),
        op: SessionEventOp::AppendMessage {
            message_id: "0".to_string(),
            message: StoredMessage {
                id: "0".to_string(),
                role: Role::User,
                content: vec![text_block("hello")],
                display_role: None,
                timestamp: None,
                tool_duration_ms: None,
                token_usage: None,
            },
        },
        parent_id: None,
        version: 1,
    };
    map.append_event(valid_event);
    assert_eq!(map.events.len(), 1);
}

#[test]
fn test_session_event_validation_skips_invalid_event_id() {
    let mut map = SessionEventMap::default();
    let invalid_event = SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: String::new(), // invalid: empty
        op: SessionEventOp::ClearAll,
        parent_id: None,
        version: 1,
    };
    map.append_event(invalid_event);
    assert_eq!(map.events.len(), 0);
}

#[test]
fn test_session_event_validation_skips_invalid_compaction() {
    let mut map = SessionEventMap::default();
    let invalid_event = SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "compact_bad".to_string(),
        op: SessionEventOp::SetCompaction {
            compaction: StoredCompactionState {
                summary_text: "x".to_string(),
                openai_encrypted_content: None,
                covers_up_to_turn: 100,
                original_turn_count: 10, // invalid: covers > original
                compacted_count: 5,
            },
        },
        parent_id: None,
        version: 1,
    };
    map.append_event(invalid_event);
    assert_eq!(map.events.len(), 0);
}

#[test]
fn test_rederive_all_checked_consistency() {
    let mut session = Session::create_with_id(
        "test_session_rederive_checked".to_string(),
        None,
        Some("Re-derive checked".to_string()),
    );
    for i in 0..3 {
        session.append_stored_message(StoredMessage {
            id: format!("m_{}", i),
            role: Role::User,
            content: vec![text_block(&format!("msg {}", i))],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        });
    }
    // No compaction: should be Ok and match message count
    let (msgs, compaction) = session.rederive_all_checked().expect("derived state consistent");
    assert_eq!(msgs.len(), 3);
    assert!(compaction.is_none());
}

#[test]
fn test_event_map_hydrated_on_disk_round_trip() {
    use std::io::Write;

    let dir = std::env::temp_dir().join(format!("jcode_event_map_rt_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.json");

    // Build a session in memory; its event_map is populated via append path.
    let mut session = Session::create_with_id(
        "test_session_rt".to_string(),
        None,
        Some("Round-trip".to_string()),
    );
    for i in 0..4 {
        session.append_stored_message(StoredMessage {
            id: format!("rt_{}", i),
            role: Role::User,
            content: vec![text_block(&format!("msg {}", i))],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        });
    }
    let in_memory_events = session.event_map.events.len();
    assert!(in_memory_events >= 4);

    // Serialize and reload through the real load path (snapshot + hydrate).
    let json = serde_json::to_string(&session).expect("serialize");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(json.as_bytes()).unwrap();
    drop(f);

    let loaded = Session::load_from_path(&path).expect("load_from_path");
    // event_map is #[serde(skip)], so without hydration it would be empty.
    assert_eq!(
        loaded.event_map.events.len(),
        loaded.messages.len(),
        "event_map must be hydrated to match the transcript on load"
    );
    assert_eq!(loaded.event_map.events.len(), 4);

    // Derived state must equal the legacy vector after hydration.
    let (derived, _) = loaded.rederive_all_checked().expect("hydration consistent");
    assert_eq!(derived.len(), 4);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_replace_after_truncate_replays_deterministically() {
    use crate::session::event_types::SessionEventMap;

    let mut map = SessionEventMap::default();
    let mk = |id: &str| StoredMessage {
        id: id.to_string(),
        role: Role::User,
        content: vec![text_block(id)],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };

    // 1) append three messages
    for id in ["a", "b", "c"] {
        map.append_event(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: format!("rehydrate_{}", id),
            op: SessionEventOp::AppendMessage { message_id: id.to_string(), message: mk(id) },
            parent_id: None,
            version: 1,
        });
    }
    // 2) truncate to first two (splice out the tail from index 2)
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "truncate".to_string(),
        op: SessionEventOp::ReplaceMessages {
            start_index: 2,
            end_index: usize::MAX,
            messages: vec![],
        },
        parent_id: None,
        version: 1,
    });
    // The derived transcript is now truncated to the first two messages.
    let derived_after_truncate: Vec<String> =
        map.derive_messages().iter().map(|m| m.id.clone()).collect();
    assert_eq!(
        derived_after_truncate,
        vec!["a".to_string(), "b".to_string()],
        "truncate must splice out the tail, not leave it intact"
    );
    // 3) full replacement (end_index::MAX semantics)
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "replace_all".to_string(),
        op: SessionEventOp::ReplaceMessages {
            start_index: 0,
            end_index: usize::MAX,
            messages: vec![mk("x"), mk("y"), mk("z")],
        },
        parent_id: None,
        version: 1,
    });

    let derived = map.derive_messages();
    let ids: Vec<&str> = derived.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["x", "y", "z"], "full replacement must drop earlier tail, not splice");
}

#[test]
fn test_append_stored_message_with_empty_id_records_event() {
    let mut session = Session::create_with_id(
        "test_empty_id_append".to_string(),
        None,
        Some("Empty id".to_string()),
    );
    let empty_id_msg = StoredMessage {
        id: String::new(),
        role: Role::User,
        content: vec![text_block("no id")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };
    session.append_stored_message(empty_id_msg);

    // Both sources of truth must stay in sync.
    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.event_map.events.len(), 1);
    let (derived, _) = session.rederive_all_checked().expect("consistent");
    assert_eq!(derived.len(), 1);
}

#[test]
fn test_clear_messages_emits_clearall_and_drops_compaction() {
    let mut session = Session::create_with_id(
        "test_clear".to_string(),
        None,
        Some("Clear".to_string()),
    );
    session.append_stored_message(StoredMessage {
        id: "m1".to_string(),
        role: Role::User,
        content: vec![text_block("hi")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });
    session.set_compaction(StoredCompactionState {
        summary_text: "sum".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 1,
        original_turn_count: 1,
        compacted_count: 1,
    });
    assert!(session.compaction.is_some());

    session.clear_messages();
    assert!(session.messages.is_empty());
    assert!(session.compaction.is_none());
    assert!(session.derive_messages().is_empty());
    assert!(session.derive_compaction().is_none());
}

#[test]
fn test_session_fork_event_log_prefix() {
    // Session-level fork: derived fields reflect only the prefix events.
    let mut session = Session::create_with_id(
        "test_fork_session".to_string(),
        None,
        Some("Fork".to_string()),
    );
    for i in 0..5usize {
        session.append_stored_message(StoredMessage {
            id: format!("msg_{}", i),
            role: Role::User,
            content: vec![text_block(&format!("m{}", i))],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        });
    }
    let mut fork = session.fork_up_to_boundary(2);
    assert_eq!(fork.derive_messages().len(), 3);
    assert_ne!(fork.id, session.id);
    // The fork's provider-message cache must reflect the truncated transcript,
    // not the parent's longer cache.
    assert_eq!(fork.messages_for_provider().len(), 3);
}

#[test]
fn test_replace_after_clear_replays_deterministically() {
    use crate::session::event_types::SessionEventMap;

    let mut map = SessionEventMap::default();
    let mk = |id: &str| StoredMessage {
        id: id.to_string(),
        role: Role::User,
        content: vec![text_block(id)],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };

    // 1) append two messages
    for id in ["a", "b"] {
        map.append_event(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: format!("rehydrate_{}", id),
            op: SessionEventOp::AppendMessage { message_id: id.to_string(), message: mk(id) },
            parent_id: None,
            version: 1,
        });
    }
    // 2) clear all
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "clear_all".to_string(),
        op: SessionEventOp::ClearAll,
        parent_id: None,
        version: 1,
    });
    // 3) replace (a full replacement issued after a clear must populate the
    //    transcript, not be silently dropped because the derived length is 0).
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "replace_all".to_string(),
        op: SessionEventOp::ReplaceMessages {
            start_index: 0,
            end_index: usize::MAX,
            messages: vec![mk("x"), mk("y")],
        },
        parent_id: None,
        version: 1,
    });

    let derived = map.derive_messages();
    let ids: Vec<&str> = derived.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["x", "y"], "replace after clear must repopulate, not stay empty");
}

#[test]
fn test_truncate_to_zero_keeps_event_log_consistent() {
    let mut session = Session::create_with_id("truncate_zero".to_string(), None, None);
    for (i, id) in ["a", "b", "c"].iter().enumerate() {
        session.append_stored_message(StoredMessage {
            id: format!("m_{}", i),
            role: Role::User,
            content: vec![text_block(id)],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        });
    }
    assert_eq!(session.derive_messages().len(), 3);
    session.truncate_messages(0);
    assert_eq!(session.messages.len(), 0, "legacy vector cleared");
    assert_eq!(session.derive_messages().len(), 0, "event log must also be empty");
}

#[test]
fn test_truncate_messages_partial_keeps_log_consistent() {
    // Regression: a partial `truncate_messages(len)` (len > 0) must truncate the
    // *derived* event-log transcript too, not just the legacy vector. Previously
    // the emitted ReplaceMessages spliced `[0..len]` back with the same prefix,
    // leaving the tail `[len..]` intact, so `derive_messages()` kept the dropped
    // messages while `self.messages` had been truncated — a hydration mismatch
    // that failed `rederive_all_checked`.
    let mut session = Session::create_with_id("truncate_partial".to_string(), None, None);
    for (i, id) in ["a", "b", "c", "d"].iter().enumerate() {
        session.append_stored_message(StoredMessage {
            id: format!("m_{}", i),
            role: Role::User,
            content: vec![text_block(id)],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        });
    }
    session.truncate_messages(2);
    assert_eq!(session.messages.len(), 2, "legacy transcript truncated");
    assert_eq!(session.derive_messages().len(), 2, "derived transcript truncated too");
    let ids: Vec<String> = session.derive_messages().iter().map(|m| m.id.clone()).collect();
    assert_eq!(ids, vec!["m_0".to_string(), "m_1".to_string()]);
    session.rederive_all_checked().expect("truncate must keep legacy and derived consistent");
}

#[test]
fn test_strip_and_truncate_keep_event_log_consistent() {
    // Test strip_oversized_images
    let mut session = Session::create_with_id("test_strip".to_string(), None, None);
    session.append_stored_message(StoredMessage {
        id: "msg1".to_string(),
        role: Role::User,
        content: vec![ContentBlock::Image {
            media_type: "image/jpeg".to_string(),
            data: "dGVzdA==".to_string(),
        }],
        display_role: None,
        timestamp: Some(Utc::now()),
        tool_duration_ms: None,
        token_usage: None,
    });
    let before_event_count = session.event_map.events.len();
    let before_msg_count = session.messages.len();
    let stripped = session.strip_oversized_images(0); // force strip
    assert!(stripped > 0);
    assert_eq!(session.messages.len(), before_msg_count); // legacy vector unchanged count-wise
    // event log should have grown by exactly one event (the strip emits ClearAll)
    assert_eq!(session.event_map.events.len(), before_event_count + 1);

    // Test emergency_truncate_tool_results (truncates ToolResult blocks only)
    let mut session2 = Session::create_with_id("test_trunc".to_string(), None, None);
    session2.append_stored_message(StoredMessage {
        id: "msg2".to_string(),
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "tool1".to_string(),
            content: "very long tool result that will be truncated".repeat(200),
            is_error: None,
        }],
        display_role: None,
        timestamp: Some(Utc::now()),
        tool_duration_ms: None,
        token_usage: None,
    });
    let before_event_count2 = session2.event_map.events.len();
    let before_msg_count2 = session2.messages.len();
    let truncated = session2.emergency_truncate_tool_results(10); // tiny budget
    assert!(truncated > 0);
    assert_eq!(session2.messages.len(), before_msg_count2); // legacy vector unchanged count-wise
    // event log should have grown by exactly one event (the mutation emits a ReplaceMessages event)
    assert_eq!(session2.event_map.events.len(), before_event_count2 + 1);
}

#[test]
fn test_remove_tool_use_blocks_keeps_event_log_consistent() {
    let mut session = Session::create_with_id("test_tooluse".to_string(), None, None);
    let tool_msg = StoredMessage {
        id: "tool_msg".to_string(),
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text { text: "Before tool".to_string(), cache_control: None },
            ContentBlock::ToolUse { id: "tool1".to_string(), name: "test_tool".to_string(), input: json!({"arg": "value"}), thought_signature: None },
            ContentBlock::Text { text: "After tool".to_string(), cache_control: None },
        ],
        display_role: None,
        timestamp: Some(Utc::now()),
        tool_duration_ms: None,
        token_usage: None,
    };
    session.append_stored_message(tool_msg.clone());
    let before_event_count = session.event_map.events.len();
    let before_msg_content_len = session.messages[0].content.len();

    session.remove_tool_use_blocks("tool_msg");
    // legacy vector still has one message, but content blocks changed
    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.messages[0].id, "tool_msg");
    // content should have lost the ToolUse block
    assert_eq!(session.messages[0].content.len(), before_msg_content_len - 1);
    // event log should have grown by exactly one event (rebuild adds a rehydrate event)
    assert_eq!(session.event_map.events.len(), before_event_count + 1);
    // derived messages should reflect the stripped content
    let derived = session.derive_messages();
    assert_eq!(derived.len(), 1);
    assert_eq!(derived[0].content.len(), before_msg_content_len - 1);
}

/// A fork truncates the event log to the boundary, so its derived transcript is
/// (usually) smaller than the parent's. The fork must not inherit the parent's
/// cached memory profile: `memory_profile_snapshot` reads the cached counts, so
/// a fork that keeps the parent's clean cache would report the parent's larger
/// message count. This pins that forking invalidates the memory-profile cache.
#[test]
fn test_fork_invalidates_memory_profile_cache() {
    let mut parent = Session::create_with_id("fork_mp_parent".to_string(), None, None);
    for i in 0..6 {
        parent.append_stored_message(StoredMessage {
            id: format!("m{i}"),
            role: Role::User,
            content: vec![text_block(&format!("msg {i}"))],
            display_role: None,
            timestamp: Some(Utc::now()),
            tool_duration_ms: None,
            token_usage: None,
        });
    }
    // Ensure the parent's memory-profile cache is clean (built).
    parent.memory_profile_snapshot();
    assert_eq!(parent.messages.len(), 6);

    // Fork at a boundary that keeps only the first 2 messages (first 2 events).
    let mut fork = parent.fork_up_to_boundary(1);
    assert_eq!(fork.messages.len(), 2, "fork truncates to the boundary");
    // The fork's memory profile must reflect 2 messages, NOT the parent's 6.
    let fork_profile = fork.memory_profile_snapshot();
    assert_eq!(
        fork_profile.message_count, 2,
        "fork memory profile must reflect the truncated transcript, not the parent's"
    );
    // The parent is unaffected.
    let parent_profile = parent.memory_profile_snapshot();
    assert_eq!(parent_profile.message_count, 6);
}

#[test]
fn test_refresh_initial_session_context_message_keeps_event_log_consistent() {
    let mut session = Session::create_with_id("test_context".to_string(), None, None);
    // ensure we have a session-context message
    assert!(session.ensure_initial_session_context_message());
    let before_event_count = session.event_map.events.len();
    let before_ctx_text: String = session.messages[0]
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .concat();

    // change the working directory to trigger a refresh
    session.working_dir = Some("/new/path".to_string());
    let changed = session.refresh_initial_session_context_message();
    assert!(changed); // should have changed since working_dir changed

    // legacy vector still has one message
    assert_eq!(session.messages.len(), 1);
    // content text should reflect the new context (edited in place, same block count)
    let mut ctx_text = String::new();
    for block in &session.messages[0].content {
        if let ContentBlock::Text { text, .. } = block {
            ctx_text.push_str(text);
        }
    }
    assert!(ctx_text.contains("/new/path"), "refreshed context should mention the new working dir");
    assert_ne!(ctx_text, before_ctx_text);
    // event log should have grown by exactly one event (the mutation emits a ReplaceMessages event)
    assert_eq!(session.event_map.events.len(), before_event_count + 1);
    // derived messages should reflect the new context
    let derived = session.derive_messages();
    assert_eq!(derived.len(), 1);
    assert!(derived[0]
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Text { text, .. } if text.contains("/new/path"))));
}

#[test]
fn test_rebuild_event_map_agrees_with_legacy_state() {
    let mut session = Session::create_with_id("test_idempotent".to_string(), None, None);
    session.append_stored_message(StoredMessage {
        id: "msg1".to_string(),
        role: Role::User,
        content: vec![text_block("hello")],
        display_role: None,
        timestamp: Some(Utc::now()),
        tool_duration_ms: None,
        token_usage: None,
    });
    session.append_stored_message(StoredMessage {
        id: "msg2".to_string(),
        role: Role::Assistant,
        content: vec![text_block("world")],
        display_role: None,
        timestamp: Some(Utc::now()),
        tool_duration_ms: None,
        token_usage: None,
    });

    // A rebuild replaces the event log with the legacy vectors as the source of
    // truth: one AppendMessage event per stored message.
    session.rebuild_event_map();
    assert_eq!(session.derive_messages().len(), 2);
    assert_eq!(session.event_map.events.len(), 2);
    let (derived, _) = session.rederive_all_checked().expect("must stay consistent");

    // Rebuilding again is a stable no-op on the derived state (same messages).
    let events_after_first = session.event_map.events.len();
    session.rebuild_event_map();
    assert_eq!(session.event_map.events.len(), events_after_first);
    assert_eq!(session.derive_messages().len(), 2);
    let (derived_again, _) = session.rederive_all_checked().expect("still consistent");
    let ids: Vec<_> = derived.iter().map(|m| m.id.clone()).collect();
    let ids_again: Vec<_> = derived_again.iter().map(|m| m.id.clone()).collect();
    assert_eq!(ids, ids_again);
}

#[test]
fn test_rebuild_event_map_reflects_direct_field_sync() {
    // Forking sets compaction/memory_injections/replay_events directly without
    // emitting matching events. rebuild_event_map must capture them so derived
    // state matches the legacy vectors (the unconditional-rebuild contract).
    let mut session = Session::create_with_id("test_direct_sync".to_string(), None, None);
    session.append_stored_message(StoredMessage {
        id: "msg1".to_string(),
        role: Role::User,
        content: vec![text_block("hello")],
        display_role: None,
        timestamp: Some(Utc::now()),
        tool_duration_ms: None,
        token_usage: None,
    });

    // Simulate a fork path that sets fields directly.
    session.compaction = Some(StoredCompactionState {
        summary_text: "summary".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 1,
        original_turn_count: 2,
        compacted_count: 1,
    });
    session.memory_injections.push(StoredMemoryInjection {
        summary: "🧠 auto-recalled 1 memory".to_string(),
        content: "note".to_string(),
        count: 1,
        memory_ids: vec![],
        age_ms: None,
        before_message: Some(1),
        timestamp: Utc::now(),
    });

    session.rebuild_event_map();
    session.rederive_all_checked().expect("fork sync must be consistent");
    assert_eq!(session.derive_compaction().map(|c| c.summary_text), Some("summary".to_string()));
    assert_eq!(session.derive_memory_injections().len(), 1);
}

#[test]
fn test_rebuild_event_map_after_clearing_compaction_drops_stale_setcompaction() {
    // Mirrors the agent/TUI compaction-clear path: set a compaction (emits a
    // SetCompaction event), then clear the legacy `compaction` field directly
    // and rebuild the log. derive_compaction() must return None; otherwise a
    // stale SetCompaction event would make the derived view disagree with the
    // cleared legacy state.
    let mut session = Session::create_with_id("test_clear_compaction_rebuild".to_string(), None, None);
    session.append_stored_message(StoredMessage {
        id: "m1".to_string(),
        role: Role::User,
        content: vec![text_block("hi")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });
    session.set_compaction(StoredCompactionState {
        summary_text: "sum".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 1,
        original_turn_count: 1,
        compacted_count: 1,
    });
    assert!(session.derive_compaction().is_some());

    // Simulation of sync_session_compaction_state_from_manager clearing state.
    session.compaction = None;
    session.rebuild_event_map();

    assert!(session.compaction.is_none());
    assert!(session.derive_compaction().is_none());
    assert!(session.derive_messages().len() == 1, "message log must be preserved");
    session.rederive_all_checked().expect("event log must agree with legacy vectors");
}

#[test]
fn test_strip_transcript_for_remote_client_keeps_event_log_consistent() {
    // strip_transcript_for_remote_client clears messages (via clear_messages),
    // memory injections, and replay events directly. The derived views must all
    // agree with the emptied legacy state after the rebuild.
    let mut session = Session::create_with_id("test_strip_remote".to_string(), None, None);
    session.append_stored_message(StoredMessage {
        id: "m1".to_string(),
        role: Role::User,
        content: vec![text_block("hi")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });
    session.record_memory_injection(
        "auto-recalled".to_string(),
        "some content".to_string(),
        1,
        0,
        vec![],
    );
    session.record_replay_event(&crate::session::StoredReplayEvent {
        timestamp: Utc::now(),
        kind: crate::session::StoredReplayEventKind::DisplayMessage {
            role: "system".to_string(),
            title: None,
            content: "notice".to_string(),
        },
    });

    assert!(session.derive_messages().len() == 1);
    assert_eq!(session.derive_memory_injections().len(), 1);
    assert_eq!(session.derive_replay_events().len(), 1);

    session.strip_transcript_for_remote_client();

    assert!(session.messages.is_empty());
    assert!(session.memory_injections.is_empty());
    assert!(session.replay_events.is_empty());
    // Derived views must match the cleared legacy vectors.
    assert!(session.derive_messages().is_empty());
    assert!(session.derive_memory_injections().is_empty());
    assert!(session.derive_replay_events().is_empty());
    assert!(session.derive_compaction().is_none());
}

#[test]
fn test_strip_transcript_for_remote_client_drops_compaction_when_messages_empty() {
    // Edge case: a compacted remote transcript may render locally with an empty
    // messages vector but retained compaction state. Stripping must still drop
    // compaction so the surviving (rebuilt) event log does not re-record a stale
    // SetCompaction event.
    let mut session = Session::create_with_id("test_strip_remote_empty".to_string(), None, None);
    // No messages, but a compaction that survived (as in a fully-compacted remote).
    session.compaction = Some(StoredCompactionState {
        summary_text: "compacted".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 1,
        original_turn_count: 2,
        compacted_count: 1,
    });
    session.record_memory_injection(
        "auto-recalled".to_string(),
        "content".to_string(),
        1,
        0,
        vec![],
    );
    session.record_replay_event(&crate::session::StoredReplayEvent {
        timestamp: Utc::now(),
        kind: crate::session::StoredReplayEventKind::DisplayMessage {
            role: "system".to_string(),
            title: None,
            content: "notice".to_string(),
        },
    });

    session.strip_transcript_for_remote_client();

    assert!(session.messages.is_empty());
    assert!(session.compaction.is_none());
    assert!(session.memory_injections.is_empty());
    assert!(session.replay_events.is_empty());
    // Rebuilt log must agree with the stripped legacy state.
    session.rederive_all_checked().expect("strip must stay consistent even with empty messages");
    assert!(session.derive_compaction().is_none());
    assert!(session.derive_messages().is_empty());
    assert!(session.derive_memory_injections().is_empty());
    assert!(session.derive_replay_events().is_empty());
}

#[test]
fn test_append_and_insert_empty_content_message_stay_consistent() {
    // append_stored_message / insert_message must not silently desync the event
    // log from the legacy vector when validation rejects the emitted event.
    // Here an empty-content message is rejected (validate_message) but still
    // lands in `messages`; the methods must rebuild the log from legacy so the
    // two sources of truth agree.
    let mut session = Session::create_with_id("test_empty_content".to_string(), None, None);
    let empty = StoredMessage {
        id: "empty".to_string(),
        role: Role::User,
        content: vec![], // empty content is rejected by event validation
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };

    session.append_stored_message(empty.clone());
    session.append_stored_message(StoredMessage {
        id: "ok1".to_string(),
        role: Role::User,
        content: vec![text_block("fine")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });
    session.insert_message(1, empty.clone());

    // Event log and legacy vector must agree (no panic/desync), even though the
    // empty-content events themselves are not recorded with the exact ids.
    session.rederive_all_checked().expect("empty-content mutated session must stay consistent");
    assert_eq!(session.messages.len(), 3);
    assert_eq!(session.derive_messages().len(), session.messages.len());
}

#[test]
fn test_replace_messages_then_direct_compaction_clear_stays_consistent() {
    // Mirrors apply_judge_visible_context_if_needed: replace the transcript
    // (emits a ReplaceMessages event) then clear compaction directly and
    // rebuild. derive_compaction() must be None while messages reflect the
    // replacement.
    let mut session = Session::create_with_id("test_judge_fork".to_string(), None, None);
    session.append_stored_message(StoredMessage {
        id: "orig".to_string(),
        role: Role::User,
        content: vec![text_block("original")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });
    session.set_compaction(StoredCompactionState {
        summary_text: "old".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 1,
        original_turn_count: 1,
        compacted_count: 1,
    });

    let transcript = vec![StoredMessage {
        id: "replaced".to_string(),
        role: Role::Assistant,
        content: vec![text_block("judge view")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    }];
    session.replace_messages(transcript);
    session.compaction = None;
    session.rebuild_event_map();

    session.rederive_all_checked().expect("event log must agree with legacy vectors");
    assert_eq!(session.derive_messages().len(), 1);
    assert_eq!(session.derive_messages()[0].id, "replaced");
    assert!(session.derive_compaction().is_none());
}

#[test]
fn test_load_path_hydrates_compaction_mem_inj_and_replay_events() {
    // Round-2 integration boundary: Session::load_from_path hydrates the event
    // log from ALL four legacy vectors (messages, compaction, memory
    // injections, replay events), not just messages. Any of these set directly
    // before persist must survive the load and be derivable from the log.
    use std::io::Write;

    let dir = std::env::temp_dir().join(format!("jcode_evt_hydrate_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.json");

    let mut session = Session::create_with_id(
        "test_hydrate_all".to_string(),
        None,
        Some("Hydrate all".to_string()),
    );
    session.append_stored_message(StoredMessage {
        id: "m1".to_string(),
        role: Role::User,
        content: vec![text_block("msg")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });
    session.compaction = Some(StoredCompactionState {
        summary_text: "hydrated-summary".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 1,
        original_turn_count: 2,
        compacted_count: 1,
    });
    session.memory_injections.push(StoredMemoryInjection {
        summary: "recalled".to_string(),
        content: "note".to_string(),
        count: 1,
        memory_ids: vec![],
        age_ms: None,
        before_message: Some(1),
        timestamp: Utc::now(),
    });
    session.replay_events.push(crate::session::StoredReplayEvent {
        timestamp: Utc::now(),
        kind: crate::session::StoredReplayEventKind::DisplayMessage {
            role: "system".to_string(),
            title: None,
            content: "notice".to_string(),
        },
    });
    // Reconcile before persisting (as fork/clone paths do).
    session.rebuild_event_map();

    let json = serde_json::to_string(&session).expect("serialize");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(json.as_bytes()).unwrap();
    drop(f);

    let loaded = Session::load_from_path(&path).expect("load_from_path");
    loaded.rederive_all_checked().expect("load hydration consistent");
    assert_eq!(loaded.derive_messages().len(), 1);
    assert_eq!(
        loaded.derive_compaction().map(|c| c.summary_text),
        Some("hydrated-summary".to_string())
    );
    assert_eq!(loaded.derive_memory_injections().len(), 1);
    assert_eq!(loaded.derive_replay_events().len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_mixed_sequence_replays_consistently_after_serialize() {
    use std::io::Write;

    // End-to-end determinism: a realistic mixed transcript mutation sequence
    // (append, insert-at-end, clear, re-append, truncate, replace) must replay
    // to exactly the same transcript as the legacy vector, both in memory and
    // after persisting and reloading through the real load path.
    let dir = std::env::temp_dir().join(format!("jcode_evt_mixed_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.json");

    let mut session = Session::create_with_id("test_mixed".to_string(), None, None);
    let mk = |id: &str, body: &str| StoredMessage {
        id: id.to_string(),
        role: Role::User,
        content: vec![text_block(body)],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };

    session.append_stored_message(mk("a", "A"));
    session.append_stored_message(mk("b", "B"));
    session.insert_message(session.messages.len(), mk("c", "C")); // append-at-end insert
    session.clear_messages();                                    // ClearAll
    session.append_stored_message(mk("x", "X"));
    session.append_stored_message(mk("y", "Y"));
    session.append_stored_message(mk("z", "Z"));
    session.truncate_messages(2);                                // keep X,Y
    session.replace_messages(vec![mk("r1", "R1"), mk("r2", "R2")]); // full replace

    // In-memory consistency between derived log and legacy vector.
    session.rederive_all_checked().expect("mixed sequence must stay consistent in memory");
    let expected: Vec<String> = session.messages.iter().map(|m| m.id.clone()).collect();
    let derived: Vec<String> = session.derive_messages().iter().map(|m| m.id.clone()).collect();
    assert_eq!(derived, expected);
    assert_eq!(derived, vec!["r1", "r2"]);

    // Persist and reload; derived state must be identical after hydration.
    let json = serde_json::to_string(&session).expect("serialize");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(json.as_bytes()).unwrap();
    drop(f);

    let loaded = Session::load_from_path(&path).expect("load_from_path");
    loaded.rederive_all_checked().expect("mixed sequence must stay consistent after reload");
    let loaded_derived: Vec<String> =
        loaded.derive_messages().iter().map(|m| m.id.clone()).collect();
    assert_eq!(loaded_derived, vec!["r1", "r2"]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_replace_messages_clamps_out_of_range_bounds() {
    // Round C: derive_messages must clamp ReplaceMessages bounds. A replacement
    // issued against a shorter-than-expected transcript (e.g. after a clear)
    // where start_index exceeds the live length must append rather than be
    // silently dropped, and end_index=usize::MAX always means "to the end".
    use crate::session::event_types::SessionEventMap;

    let mk = |id: &str| StoredMessage {
        id: id.to_string(),
        role: Role::User,
        content: vec![text_block(id)],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };

    // Empty transcript, then a full replacement with a high start index
    // (simulating a replace issued after the transcript was cleared to empty).
    let mut map = SessionEventMap::default();
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "r1".to_string(),
        op: SessionEventOp::ReplaceMessages {
            start_index: 5, // exceeds the (empty) current transcript
            end_index: 8,
            messages: vec![mk("a"), mk("b")],
        },
        parent_id: None,
        version: 1,
    });
    let ids: Vec<String> = map.derive_messages().iter().map(|m| m.id.clone()).collect();
    assert_eq!(ids, vec!["a", "b"], "out-of-range start must append, not drop");

    // A mid-list replace where end_index exceeds the live length is clamped.
    let mut map2 = SessionEventMap::default();
    for id in ["a", "b", "c"] {
        map2.append_event(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: format!("append_{}", id),
            op: SessionEventOp::AppendMessage { message_id: id.to_string(), message: mk(id) },
            parent_id: None,
            version: 1,
        });
    }
    map2.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "partial".into(),
        op: SessionEventOp::ReplaceMessages {
            start_index: 1,
            end_index: 999, // exceeds live length; clamped to 3
            messages: vec![mk("X"), mk("Y")],
        },
        parent_id: None,
        version: 1,
    });
    let ids2: Vec<String> = map2.derive_messages().iter().map(|m| m.id.clone()).collect();
    assert_eq!(ids2, vec!["a", "X", "Y"], "end clamped to live length");
}

#[test]
fn test_replace_messages_reversed_bounds_do_not_panic() {
    // Regression: a ReplaceMessages whose `start_index > end_index` (reversed
    // span) must not panic inside `Vec::splice` during replay. The event log is
    // corruption-tolerant by design, so a malformed event must degrade to a no-op
    // rather than crash `derive_messages`. (Producers never emit reversed bounds
    // through the current API; this guards the replay path against a bad event.)
    use crate::session::event_types::SessionEventMap;

    let mk = |id: &str| StoredMessage {
        id: id.to_string(),
        role: Role::User,
        content: vec![text_block(id)],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };
    let mut map = SessionEventMap::default();
    for id in ["a", "b", "c"] {
        map.append_event(SessionEvent {
            timestamp: chrono::Utc::now(),
            event_id: format!("append_{}", id),
            op: SessionEventOp::AppendMessage {
                message_id: id.to_string(),
                message: mk(id),
            },
            parent_id: None,
            version: 1,
        });
    }
    // Reversed span: start_index 2 > end_index 1.
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "reversed".into(),
        op: SessionEventOp::ReplaceMessages {
            start_index: 2,
            end_index: 1,
            messages: vec![mk("X")],
        },
        parent_id: None,
        version: 1,
    });
    // Must not panic; the reversed span degrades to a point-insertion at `start`
    // (end is clamped up to start, matching equal-bounds semantics), never a
    // crash. This keeps replay corruption-tolerant.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| map.derive_messages()));
    let messages = result.expect("derive_messages must not panic on reversed bounds");
    let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
    assert_eq!(
        ids,
        vec!["a", "b", "X", "c"],
        "reversed span degrades to an insertion at start (no panic)"
    );
}

#[test]
fn test_memory_injection_and_replay_event_derived_order_matches_legacy() {
    // Round B: at the public Session API boundary, derived memory injections and
    // replay events must come back in the same submission order as the legacy
    // vectors, including after a rebuild/reload (hydration preserves order).
    use std::io::Write;

    let dir = std::env::temp_dir().join(format!("jcode_ord_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.json");

    let mut session = Session::create_with_id("test_order".to_string(), None, None);
    session.append_stored_message(StoredMessage {
        id: "m1".to_string(),
        role: Role::User,
        content: vec![text_block("hi")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });
    session.record_memory_injection("first".to_string(), "A".to_string(), 1, 0, vec![]);
    session.record_replay_event(&crate::session::StoredReplayEvent {
        timestamp: Utc::now(),
        kind: crate::session::StoredReplayEventKind::DisplayMessage {
            role: "system".to_string(),
            title: None,
            content: "notice1".to_string(),
        },
    });
    session.record_memory_injection("second".to_string(), "B".to_string(), 2, 0, vec![]);
    session.record_replay_event(&crate::session::StoredReplayEvent {
        timestamp: Utc::now(),
        kind: crate::session::StoredReplayEventKind::DisplayMessage {
            role: "system".to_string(),
            title: None,
            content: "notice2".to_string(),
        },
    });

    let inj_derived: Vec<String> =
        session.derive_memory_injections().iter().map(|i| i.summary.clone()).collect();
    let inj_legacy: Vec<String> =
        session.memory_injections.iter().map(|i| i.summary.clone()).collect();
    assert_eq!(inj_derived, vec!["first", "second"]);
    assert_eq!(inj_derived, inj_legacy);

    let rp_derived: Vec<String> = session
        .derive_replay_events()
        .iter()
        .filter_map(|e| match &e.kind {
            crate::session::StoredReplayEventKind::DisplayMessage { content, .. } => {
                Some(content.clone())
            }
            _ => None,
        })
        .collect();
    let rp_legacy: Vec<String> = session
        .replay_events
        .iter()
        .filter_map(|e| match &e.kind {
            crate::session::StoredReplayEventKind::DisplayMessage { content, .. } => {
                Some(content.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(rp_derived, vec!["notice1", "notice2"]);
    assert_eq!(rp_derived, rp_legacy);

    // Persist + reload: hydration must preserve this order.
    let json = serde_json::to_string(&session).expect("serialize");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(json.as_bytes()).unwrap();
    drop(f);
    let loaded = Session::load_from_path(&path).expect("load_from_path");
    let li: Vec<String> =
        loaded.derive_memory_injections().iter().map(|i| i.summary.clone()).collect();
    assert_eq!(li, vec!["first", "second"]);
    let lr: Vec<String> = loaded
        .derive_replay_events()
        .iter()
        .filter_map(|e| match &e.kind {
            crate::session::StoredReplayEventKind::DisplayMessage { content, .. } => {
                Some(content.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(lr, vec!["notice1", "notice2"]);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_compaction_cache_tracks_clear_and_reset_order() {
    // Round H: the compaction_event_index cache must reflect the physical event
    // order across mixed streams (SetCompaction -> ClearAll -> re-set). At the
    // public Session API boundary, derive_compaction must agree with the
    // current_compaction cache and the legacy compaction field.
    let mk_comp = |summary: &str| StoredCompactionState {
        summary_text: summary.to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 1,
        original_turn_count: 1,
        compacted_count: 1,
    };

    let mut session = Session::create_with_id("test_comp_order".to_string(), None, None);
    session.append_stored_message(StoredMessage {
        id: "m1".to_string(),
        role: Role::User,
        content: vec![text_block("hi")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });

    // Set -> Clear -> re-set.
    session.set_compaction(mk_comp("first"));
    assert_eq!(session.derive_compaction().map(|c| c.summary_text), Some("first".to_string()));
    assert!(session.event_map.current_compaction().is_some());

    session.clear_messages();
    assert!(session.derive_compaction().is_none(), "ClearAll must drop cached compaction");
    assert!(session.event_map.current_compaction().is_none());

    session.set_compaction(mk_comp("second"));
    assert_eq!(session.derive_compaction().map(|c| c.summary_text), Some("second".to_string()));

    // Legacy and derived must agree throughout.
    session.rederive_all_checked().expect("compaction ordering must stay consistent");
    assert_eq!(session.compaction.as_ref().map(|c| c.summary_text.clone()),
               session.derive_compaction().map(|c| c.summary_text.clone()));
}

/// `set_compaction` with an invalid compaction state must not leave the legacy
/// `self.compaction` vector diverging from the derived log. `append_event`
/// silently skips an invalid `SetCompaction` (e.g. `covers_up_to_turn` exceeding
/// `original_turn_count`); if the method still set `self.compaction = Some(...)`,
/// `derive_compaction()` (None) and the legacy vector (Some) would disagree and
/// `rederive_all_checked` would fail. The method must only publish the legacy
/// vector when the event was actually recorded.
#[test]
fn test_set_compaction_invalid_state_does_not_desync() {
    let mut session = Session::create_with_id("setc_bad_rt".to_string(), None, Some("bad".to_string()));
    let bad = StoredCompactionState {
        summary_text: "bad".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 5,
        original_turn_count: 1,
        compacted_count: 1,
    };
    session.set_compaction(bad.clone());

    // The invalid SetCompaction event is rejected, so neither the log nor the
    // legacy vector records it.
    assert_eq!(
        session
            .event_map
            .events
            .iter()
            .filter(|e| matches!(e.op, SessionEventOp::SetCompaction { .. }))
            .count(),
        0,
        "invalid SetCompaction event must be rejected"
    );
    assert!(
        session.compaction.is_none(),
        "invalid compaction must not be published to the legacy vector"
    );
    assert_eq!(session.derive_compaction(), None, "derived compaction must be None too");
    session
        .rederive_all_checked()
        .expect("event log must stay consistent with legacy after rejected set_compaction");
}

#[test]
fn test_in_place_mutation_reflects_in_derived_and_provider_view() {
    // Round F: an in-place content mutation (remove_tool_use_blocks) must be
    // reflected in both the derived event-log view AND the provider message
    // view (which rebuilds from the mutated legacy transcript).
    let mut session = Session::create_with_id("test_provider_mutation".to_string(), None, None);
    session.append_stored_message(StoredMessage {
        id: "tool_msg".to_string(),
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text { text: "before".to_string(), cache_control: None },
            ContentBlock::ToolUse { id: "t1".to_string(), name: "tool".to_string(), input: json!({"a":1}), thought_signature: None },
        ],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });

    let provider_before = session.messages_for_provider();
    assert_eq!(provider_before[0].content.len(), 2);

    session.remove_tool_use_blocks("tool_msg");
    assert_eq!(session.messages[0].content.len(), 1);
    assert_eq!(session.derive_messages()[0].content.len(), 1);

    // Provider view must rebuild and reflect the mutation.
    let provider_after = session.messages_for_provider();
    assert_eq!(provider_after[0].content.len(), 1);
    session.rederive_all_checked().expect("in-place mutation must stay consistent");
}

#[test]
fn test_duplicate_id_empty_content_append_stays_consistent() {
    // Round O regression: a later append whose content is empty (rejected by
    // event validation) but whose id matches an earlier accepted message must
    // still trigger the rebuild fallback. The previous tail-id heuristic was
    // fooled by the shared id and left the log and legacy vector desynced.
    let mut session = Session::create_with_id("test_dup_id_empty".to_string(), None, None);
    // First: an accepted message with id "X".
    session.append_stored_message(StoredMessage {
        id: "X".to_string(),
        role: Role::User,
        content: vec![text_block("ok")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });

    // Second: same id "X" but empty content -> validation rejects the event.
    session.append_stored_message(StoredMessage {
        id: "X".to_string(),
        role: Role::User,
        content: vec![], // rejected
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });

    // The log must be reconciled: derived id list equals legacy id list.
    let legacy_ids: Vec<String> = session.messages.iter().map(|m| m.id.clone()).collect();
    let derived_ids: Vec<String> = session.derive_messages().iter().map(|m| m.id.clone()).collect();
    assert_eq!(derived_ids, legacy_ids, "log and legacy must agree even with duplicate empty-content append");
    assert_eq!(derived_ids, vec!["X", "X"]);
    session.rederive_all_checked().expect("duplicate-id empty append must stay consistent");
}

#[test]
fn test_empty_and_replay_only_session_hydrate_consistently() {
    // Round T: an empty session and a replay-only session (no messages) must
    // round-trip through the real load path and stay consistent after rebuild.
    use std::io::Write;

    let dir = std::env::temp_dir().join(format!("jcode_edge_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // 1) Empty session (no messages/injections/compaction/replay).
    let empty = Session::create_with_id("edge_empty".to_string(), None, None);
    let p1 = dir.join("empty.json");
    std::fs::write(&p1, serde_json::to_string(&empty).unwrap()).unwrap();
    let loaded_empty = Session::load_from_path(&p1).expect("load empty");
    loaded_empty.rederive_all_checked().expect("empty session consistent");
    assert!(loaded_empty.messages.is_empty());
    assert!(loaded_empty.derive_messages().is_empty());

    // 2) Replay-only session (a replay event but no messages). before_message on
    //    the replay event is independent; hydration must preserve the event.
    let mut replay_only = Session::create_with_id("edge_replay".to_string(), None, None);
    replay_only.record_replay_event(&crate::session::StoredReplayEvent {
        timestamp: Utc::now(),
        kind: crate::session::StoredReplayEventKind::DisplayMessage {
            role: "system".to_string(),
            title: None,
            content: "notice".to_string(),
        },
    });
    // Bypass the in-process event append to simulate a persisted-session load
    // where only the legacy replay vector is present.
    replay_only.rebuild_event_map();
    let p2 = dir.join("replay.json");
    std::fs::write(&p2, serde_json::to_string(&replay_only).unwrap()).unwrap();
    let loaded_replay = Session::load_from_path(&p2).expect("load replay-only");
    loaded_replay.rederive_all_checked().expect("replay-only consistent");
    assert!(loaded_replay.messages.is_empty());
    assert_eq!(loaded_replay.derive_replay_events().len(), 1);
    assert_eq!(loaded_replay.replay_events.len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_multi_insert_repair_pattern_stays_consistent() {
    // Round U: mirrors agent.rs repair_missing_tool_outputs - an assistant
    // message with two missing tool uses is followed by two sequential
    // insert_message calls using the (index + 1 + inserted + offset) arithmetic.
    // The event log must stay consistent with the legacy vector throughout.
    let mut session = Session::create_with_id("test_multi_repair".to_string(), None, None);
    session.append_stored_message(StoredMessage {
        id: "user".to_string(),
        role: Role::User,
        content: vec![text_block("run tools")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });
    // Assistant at index 1, with two ToolUse blocks that need results.
    session.append_stored_message(StoredMessage {
        id: "asst".to_string(),
        role: Role::Assistant,
        content: vec![
            ContentBlock::ToolUse { id: "t1".to_string(), name: "tool".to_string(), input: json!({"a":1}), thought_signature: None },
            ContentBlock::ToolUse { id: "t2".to_string(), name: "tool".to_string(), input: json!({"b":2}), thought_signature: None },
        ],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });

    // Repair: insert two tool results after the assistant (index 1), using the
    // offset arithmetic from repair_missing_tool_outputs. `inserted` only
    // advances per assistant message (by this message's missing count), while
    // `offset` indexes within the message's missing items.
    let missing_for_message = vec!["t1", "t2"];
    let mut inserted = 0usize;
    for (offset, tid) in missing_for_message.iter().enumerate() {
        let stored = StoredMessage {
            id: format!("result_{tid}"),
            role: Role::User,
            content: vec![ContentBlock::ToolResult { tool_use_id: tid.to_string(), content: "ok".to_string(), is_error: Some(false) }],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        };
        session.insert_message(1 + 1 + inserted + offset, stored);
    }
    inserted += missing_for_message.len();

    session.rederive_all_checked().expect("multi-insert repair must stay consistent");
    let ids: Vec<String> = session.messages.iter().map(|m| m.id.clone()).collect();
    let dids: Vec<String> = session.derive_messages().iter().map(|m| m.id.clone()).collect();
    assert_eq!(ids, dids);
    // Expected order: user, asst, result_t1, result_t2
    assert_eq!(ids, vec!["user", "asst", "result_t1", "result_t2"]);
}

#[test]
fn test_fork_preserves_aged_events() {
    // Round AA: forking a log whose events carry old (beyond the ±1yr insert
    // validation window) timestamps must NOT drop them. Re-validating on fork
    // would silently truncate long-lived or imported sessions.
    use crate::session::event_types::SessionEventMap;

    let old = chrono::Utc::now() - chrono::Duration::days(700); // ~2 years ago
    let mut map = SessionEventMap::default();
    map.push_event(SessionEvent {
        timestamp: old,
        event_id: "rehydrate_0".to_string(),
        op: SessionEventOp::AppendMessage {
            message_id: "m0".to_string(),
            message: StoredMessage {
                id: "m0".to_string(),
                role: Role::User,
                content: vec![text_block("aged")],
                display_role: None,
                timestamp: Some(old),
                tool_duration_ms: None,
                token_usage: None,
            },
        },
        parent_id: None,
        version: 1,
    });

    // Fork up to boundary 0 must preserve the aged message.
    let fork = map.fork_up_to_boundary(0);
    let ids: Vec<String> = fork.derive_messages().iter().map(|m| m.id.clone()).collect();
    assert_eq!(ids, vec!["m0"], "fork must not drop an aged (validated-at-insert) event");
}

/// The event log is the authoritative append-only record and now persists in
/// the snapshot. After a **snapshot** round-trip (no journaling), the
/// persisted log must be kept as-is on reload, including a log-bracketed
/// compaction (CompactionStart/End markers) and any `Unknown` plugin event
/// that a rebuild-from-legacy-vectors cannot reproduce.
#[test]
fn test_persisted_event_log_survives_snapshot_round_trip_with_bracket_and_unknown() {
    use std::io::Write;

    let dir = std::env::temp_dir().join(format!(
        "jcode_event_log_snapshot_rt_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.json");

    let mut session = Session::create_with_id(
        "snapshot_bracket_rt".to_string(),
        None,
        Some("Snapshot bracket".to_string()),
    );
    session.append_stored_message(StoredMessage {
        id: "m1".to_string(),
        role: Role::User,
        content: vec![text_block("hello")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });

    // A balanced compaction bracket (takeaway #5), plus a plugin `Unknown`
    // event only the event log can carry (takeaway #13). Neither is a legacy
    // vector, so they prove the persisted log (not a rebuild) is authoritative.
    let compaction = StoredCompactionState {
        summary_text: "summarized".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 1,
        original_turn_count: 1,
        compacted_count: 1,
    };
    session.event_map.start_compaction("comp_1", 1);
    // Bracket start does not touch the legacy vectors; a real producer would
    // call replace_messages first, but for this persistence test we emulate
    // the surface the log derives after the bracket closes.
    session.event_map.end_compaction(compaction.clone());
    session.compaction = Some(compaction.clone()); // legacy vector mirrors the close

    let unknown_event = SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "plugin_1".to_string(),
        op: SessionEventOp::Unknown {
            event_type: "review_round".to_string(),
            data: serde_json::json!({ "rounds": 3 }),
        },
        parent_id: None,
        version: 1,
    };
    session.event_map.append_event(unknown_event);

    // Sanity: the in-memory log has the bracket markers + unknown preserved.
    assert!(session.event_map.orphaned_compaction().is_none());

    // Persist to a snapshot and reload through the real load path.
    let json = serde_json::to_string(&session).expect("serialize");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(json.as_bytes()).unwrap();
    drop(f);

    let loaded = Session::load_from_path(&path).expect("load_from_path");
    let kept = {
        // reconcile already ran inside load; assert the authoritative log kept.
        loaded.event_map.events.iter().any(|e| {
            matches!(e.op, SessionEventOp::CompactionStart { .. })
                && matches!(&e.op, SessionEventOp::CompactionStart { .. })
        })
    };
    assert!(
        kept,
        "persisted bracket (CompactionStart) must survive the snapshot round-trip"
    );
    assert!(
        loaded.event_map.events.iter().any(|e| matches!(
            e.op,
            SessionEventOp::Unknown { .. }
        )),
        "persisted Unknown plugin event must survive the snapshot round-trip"
    );
    assert!(
        loaded.event_map.orphaned_compaction().is_none(),
        "balanced bracket must remain balanced after reload"
    );


    let _ = std::fs::remove_dir_all(&dir);
}

/// `Session::compact_transcript_with_bracket` is the producer seam for a
/// log-bracketed compaction (takeaway #5): it emits a balanced CompactionStart
/// / CompactionEnd pair around the single surface mutation (replace), keeps the
/// legacy `compaction` vector in sync, and leaves a replayer a correct derived
/// surface plus an `End`-carried state that `current_compaction` picks up.
#[test]
fn test_compact_transcript_with_bracket_produces_balanced_durable_bracket() {
    let mut session = Session::create_with_id(
        "bracket_producer".to_string(),
        None,
        Some("Bracket producer".to_string()),
    );
    for i in 0..3 {
        session.append_stored_message(StoredMessage {
            id: format!("m{i}"),
            role: Role::User,
            content: vec![text_block(&format!("msg {i}"))],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        });
    }

    let tail = vec![StoredMessage {
        id: "tail".to_string(),
        role: Role::User,
        content: vec![text_block("[summary]")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    }];
    let compaction = StoredCompactionState {
        summary_text: "summarized".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 3,
        original_turn_count: 3,
        compacted_count: 2,
    };

    let id = session.compact_transcript_with_bracket("comp_a", tail.clone(), compaction.clone(), 3);

    // Balanced bracket, no orphan, and a single persisting state.
    assert!(
        session.event_map.orphaned_compaction().is_none(),
        "completed bracket must not leave an orphaned lock"
    );
    let ops: Vec<&SessionEventOp> = session.event_map.events.iter().map(|e| &e.op).collect();
    let has_start = ops.iter().any(|op| matches!(op, SessionEventOp::CompactionStart { .. }));
    let has_end = ops.iter().any(|op| matches!(op, SessionEventOp::CompactionEnd { .. }));
    assert!(has_start && has_end, "bracket must contain both markers; ops={ops:?}");

    // Legacy vectors agree with what the log derives.
    let (derived_messages, derived_compaction) = session.rederive_all();
    assert_eq!(
        derived_messages.len(),
        tail.len(),
        "derived surface length must match the summarized tail"
    );
    assert_eq!(
        derived_messages.last().map(|m| m.id.as_str()),
        Some("tail"),
        "derived surface must be the summarized tail"
    );
    assert_eq!(
        derived_compaction.as_ref().map(|c| &c.summary_text),
        Some(&"summarized".to_string())
    );
    session
        .rederive_all_checked()
        .expect("bracket producer must yield a consistent event log");

    // The bracket must survive a full serialize + reload, staying balanced.
    let json = serde_json::to_string(&session).unwrap();
    let back: Session = serde_json::from_str(&json).unwrap();
    assert!(
        back.event_map.orphaned_compaction().is_none(),
        "balanced bracket must survive serialize round-trip"
    );
    assert_eq!(
        back.event_map.current_compaction().as_ref().map(|c| &c.summary_text),
        Some(&"summarized".to_string()),
        "current_compaction must report the bracket's End-carried state"
    );
    assert_eq!(back.compaction.as_ref().map(|c| &c.summary_text), Some(&"summarized".to_string()));
    back.rederive_all_checked()
        .expect("reloaded bracket producer log must stay consistent");
}

/// An `Unknown` event constructed in-memory with a **non-object** `data` payload
/// (e.g. `data: json!(123)`) must serialize to exactly one top-level `op` key.
/// Regression for a bug where the non-object branch emitted `op` inside the
/// match *and* again after it, producing invalid JSON with duplicate keys:
/// `{"op":"x","data":123,"op":"x"}`. The serialized form is also shape-stable
/// after the first round-trip (a scalar becomes the object-wrapped form, matching
/// the on-wire shape, and stays stable on subsequent round-trips).
#[test]
fn test_unknown_op_in_memory_non_object_serializes_single_op() {
    let op = SessionEventOp::Unknown {
        event_type: "plugin_scalar".to_string(),
        data: serde_json::json!(123),
    };
    // The RAW serialized string must contain exactly one `op` key (no duplicates).
    let json = serde_json::to_string(&op).expect("serialize in-memory non-object");
    let raw_op_count = json.matches("\"op\":").count();
    assert_eq!(
        raw_op_count,
        1,
        "in-memory non-object Unknown must serialize exactly one op key; got: {json}"
    );
    // The round-trip must be stable: after the first deserialize the value settles
    // into the object-wrapped wire form and stays unchanged on further round-trips.
    let back: SessionEventOp = serde_json::from_str(&json).expect("deserialize");
    let json2 = serde_json::to_string(&back).expect("re-serialize");
    let again: SessionEventOp = serde_json::from_str(&json2).expect("re-deserialize");
    let json3 = serde_json::to_string(&again).expect("re-serialize 2");
    assert_eq!(json2, json3, "shape must stabilize after first round-trip");
    assert_eq!(json2.matches("\"op\":").count(), 1);
}

/// The `Unknown` escape hatch must round-trip **stably** even when the remaining
/// payload is not a flat JSON object (e.g. `{"op":"x","data":123}`). Such a
/// value is preserved as an object wrapper (`data: { "data": 123 }`) so it
/// carries an `op` alongside the payload; a second serialize→deserialize must
/// reproduce the exact same in-memory value (no unbounded nesting growth). This
/// pins the documented behavior of the non-object branch of `Serialize`.
#[test]
fn test_unknown_op_non_object_payload_round_trips_losslessly() {
    // Serialize a non-object data directly (matches the on-disk `{"op","data"}`)
    // and confirm Deserialize recovers the same value WITHOUT re-nesting.
    let raw = r#"{"op":"plugin_scalar","data":123}"#;
    let op: SessionEventOp = serde_json::from_str(raw).expect("deserialize scalar unknown");
    match &op {
        SessionEventOp::Unknown { event_type, data } => {
            assert_eq!(event_type, "plugin_scalar");
            // Round-trip once. A non-object payload must stay a scalar, not
            // collapse into `{"data":123}`.
            let json = serde_json::to_string(&op).expect("serialize");
            let again: SessionEventOp = serde_json::from_str(&json).expect("round-trip");
            match &again {
                SessionEventOp::Unknown { data: d2, .. } => {
                    assert_eq!(
                        d2,
                        data,
                        "non-object Unknown payload must round-trip without shape change"
                    );
                }
                other => panic!("changed kind: {other:?}"),
            }
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}

/// A retried compaction after a crash must not double-open the bracket. If a
/// `CompactionStart` is already orphaned (a previous run crashed mid-bracket),
/// `compact_transcript_with_bracket` should complete it rather than start a
/// second, nested bracket — which would leave a depth-2 malformed bracket the
/// `CompactionBracket` invariant flags as silent corruption. After the retry the
/// log must contain exactly one balanced bracket.
#[test]
fn test_compact_transcript_retry_recovers_orphaned_bracket() {
    let mut session = Session::create_with_id(
        "bracket_retry".to_string(),
        None,
        Some("Bracket retry".to_string()),
    );
    session.append_stored_message(StoredMessage {
        id: "m1".to_string(),
        role: Role::User,
        content: vec![text_block("msg 1")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });

    // Simulate a crashed in-flight compaction: an orphaned CompactionStart.
    session.event_map.start_compaction("crashed_1", 1);
    assert!(session.event_map.orphaned_compaction().is_some());
    let orphaned_before = session
        .event_map
        .events
        .iter()
        .filter(|e| matches!(e.op, SessionEventOp::CompactionStart { .. }))
        .count();

    // Retry completes the in-flight bracket instead of double-opening.
    let tail = vec![StoredMessage {
        id: "tail".to_string(),
        role: Role::User,
        content: vec![text_block("[summary]")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    }];
    let compaction = StoredCompactionState {
        summary_text: "summarized".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 1,
        original_turn_count: 1,
        compacted_count: 1,
    };
    session.compact_transcript_with_bracket("retry_1", tail, compaction.clone(), 1);

    // Exactly one start and one end (no depth-2 double bracket).
    let starts = session
        .event_map
        .events
        .iter()
        .filter(|e| matches!(e.op, SessionEventOp::CompactionStart { .. }))
        .count();
    let ends = session
        .event_map
        .events
        .iter()
        .filter(|e| matches!(e.op, SessionEventOp::CompactionEnd { .. }))
        .count();
    assert_eq!(
        starts, orphaned_before,
        "retry must not add another CompactionStart (no nesting)"
    );
    assert_eq!(starts, ends, "bracket must be balanced after retry");
    assert!(
        session.event_map.orphaned_compaction().is_none(),
        "retry must close the orphaned bracket"
    );
    // The recovered bracket is accepted by the invariant registry.
    let inv = crate::session::InvariantRegistry::builtin();
    assert!(
        inv.check(&session.event_map).is_green(),
        "retried bracket must satisfy the CompactionBracket invariant"
    );
    session
        .rederive_all_checked()
        .expect("retried bracket producer must stay consistent");
}

/// A `compact_transcript_with_bracket` whose `CompactionStart` is rejected by
/// validation (e.g. `covers_up_to_turn == 0`) must NOT emit a dangling
/// `CompactionEnd`. `append_event` silently skips an invalid `CompactionStart`,
/// so without a guard the method would close a bracket that was never opened,
/// producing a malformed bracket the `CompactionBracket` invariant flags. The
/// fix detects the skipped start (event-count non-growth) and falls back to a
/// plain `SetCompaction`, keeping the log consistently balanced.
#[test]
fn test_compact_transcript_rejected_start_falls_back_to_set_compaction() {
    let mut session = Session::create_with_id(
        "bracket_rejected_start".to_string(),
        None,
        Some("Rejected start".to_string()),
    );
    session.append_stored_message(StoredMessage {
        id: "m1".to_string(),
        role: Role::User,
        content: vec![text_block("msg 1")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });
    let tail = vec![StoredMessage {
        id: "tail".to_string(),
        role: Role::User,
        content: vec![text_block("[summary]")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    }];
    let compaction = StoredCompactionState {
        summary_text: "summarized".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 1,
        original_turn_count: 1,
        compacted_count: 1,
    };
    // covers_up_to_turn == 0 -> CompactionStart validation rejects it.
    session.compact_transcript_with_bracket("bad_start", tail, compaction.clone(), 0);

    // No dangling CompactionEnd, no orphaned start; the bracket invariant is green.
    let starts = session
        .event_map
        .events
        .iter()
        .filter(|e| matches!(e.op, SessionEventOp::CompactionStart { .. }))
        .count();
    let ends = session
        .event_map
        .events
        .iter()
        .filter(|e| matches!(e.op, SessionEventOp::CompactionEnd { .. }))
        .count();
    assert_eq!(starts, 0, "rejected CompactionStart must not appear");
    assert_eq!(ends, 0, "no dangling CompactionEnd when start was rejected");
    assert!(
        session.event_map.orphaned_compaction().is_none(),
        "no orphaned bracket"
    );
    assert!(
        crate::session::InvariantRegistry::builtin()
            .check(&session.event_map)
            .is_green(),
        "bracket invariant must stay green"
    );
    // The compaction is still persisted (via a plain SetCompaction fallback),
    // so the legacy vector and the derived log agree.
    assert_eq!(
        session.compaction.as_ref().map(|c| &c.summary_text),
        Some(&"summarized".to_string()),
        "compaction must still be recorded"
    );
    session
        .rederive_all_checked()
        .expect("rejected-start bracket producer must stay consistent");
}

/// A `compact_transcript_with_bracket` whose `compaction` state is invalid (e.g.
/// `covers_up_to_turn` exceeding `original_turn_count`) must not open a bracket.
/// Such a state cannot be represented in any event — both a `CompactionEnd` and a
/// `SetCompaction` would be rejected by validation — so opening a
/// `CompactionStart` would leave a permanent orphaned bracket (the append-only
/// log could never close it with a valid End). The method must instead apply only
/// the message replacement and leave `self.compaction` unset, keeping the log
/// balanced and the invariant green.
#[test]
fn test_compact_transcript_invalid_compaction_state_opens_no_bracket() {
    let mut session = Session::create_with_id(
        "bracket_invalid_state".to_string(),
        None,
        Some("Invalid state".to_string()),
    );
    session.append_stored_message(StoredMessage {
        id: "m1".to_string(),
        role: Role::User,
        content: vec![text_block("msg 1")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    });
    let tail = vec![StoredMessage {
        id: "tail".to_string(),
        role: Role::User,
        content: vec![text_block("[summary]")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    }];
    // Invalid compaction: covers_up_to_turn (5) > original_turn_count (1).
    let bad = StoredCompactionState {
        summary_text: "bad".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: 5,
        original_turn_count: 1,
        compacted_count: 1,
    };
    session.compact_transcript_with_bracket("bad_state", tail, bad.clone(), 1);

    // No bracket opened, no orphan, invalid compaction not persisted.
    let starts = session
        .event_map
        .events
        .iter()
        .filter(|e| matches!(e.op, SessionEventOp::CompactionStart { .. }))
        .count();
    let ends = session
        .event_map
        .events
        .iter()
        .filter(|e| matches!(e.op, SessionEventOp::CompactionEnd { .. }))
        .count();
    assert_eq!(starts, 0, "no bracket opened with invalid compaction");
    assert_eq!(ends, 0, "no dangling CompactionEnd");
    assert!(session.event_map.orphaned_compaction().is_none(), "no orphaned bracket");
    assert!(
        session.compaction.is_none(),
        "invalid compaction state must not be persisted"
    );
    assert!(
        crate::session::InvariantRegistry::builtin()
            .check(&session.event_map)
            .is_green(),
        "bracket invariant must stay green"
    );
    session
        .rederive_all_checked()
        .expect("invalid-compaction producer must stay consistent");
}

/// The `Unknown` escape hatch must not corrupt a plugin payload that happens to
/// contain an `op` field of its own: `op` is the **reserved** wire discriminator,
/// so a top-level payload field named `op` is dropped deterministically (exactly
/// one `op` = the tag) rather than producing duplicate `op` keys — which would be
/// invalid/ambiguous JSON on the wire. Other payload fields survive.
#[test]
fn test_unknown_op_with_reserved_op_key_in_payload() {
    let op = SessionEventOp::Unknown {
        event_type: "plugin_complex".to_string(),
        data: serde_json::json!({
            "op": "not-the-discriminator",
            "x": 1,
        }),
    };
    // Serialize should produce ONE top-level `op` (the tag) and keep the payload's
    // `op` intact under the same object (it collides on the wire, so we assert the
    // round-trip preserves the payload value rather than hard-failing).
    let json = serde_json::to_string(&op).expect("serialize");
    let back: SessionEventOp = serde_json::from_str(&json).expect("round-trip");
    match back {
        SessionEventOp::Unknown { event_type, data } => {
            assert_eq!(event_type, "plugin_complex");
            assert_eq!(
                data.get("x").and_then(|v| v.as_u64()),
                Some(1),
                "non-reserved payload field must survive"
            );
            // `op` is the reserved wire discriminator, so a payload field named
            // `op` is not representable at the top level. The serializer must
            // drop it deterministically (one `op` = the tag) rather than emit
            // duplicate `op` keys (invalid/ambiguous JSON). This is the documented
            // reserved-key contract, not silent corruption.
            assert_eq!(
                event_type.as_str(),
                "plugin_complex",
                "the tag stays the discriminator (payload op is reserved)"
            );
            assert_eq!(
                data.get("op"),
                None,
                "reserved payload 'op' is dropped deterministically (avoid duplicate keys)"
            );
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}

/// `current_compaction` reflects the last *completed* compaction, not an open
/// in-flight bracket. When a prior SetCompaction exists and a new
/// `CompactionStart` is opened (not yet closed), `current_compaction` must still
/// return the prior completed state on both the live cache and after a reload
/// (reverse-scan), while `orphaned_compaction` reports the open bracket.
#[test]
fn test_current_compaction_ignores_open_bracket_and_keeps_last_completed() {
    let mut map = SessionEventMap::default();
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "set1".to_string(),
        op: SessionEventOp::SetCompaction {
            compaction: StoredCompactionState {
                summary_text: "completed_at_set".to_string(),
                openai_encrypted_content: None,
                covers_up_to_turn: 1,
                original_turn_count: 1,
                compacted_count: 1,
            },
        },
        parent_id: None,
        version: 1,
    });
    // Open a new in-flight bracket (no End yet).
    map.start_compaction("in_flight", 2);

    // Live: last completed compaction is still the SetCompaction.
    assert_eq!(
        map.current_compaction().map(|c| c.summary_text).as_deref(),
        Some("completed_at_set"),
        "an open bracket must not hide the last completed compaction (live cache)"
    );
    assert!(
        map.orphaned_compaction().is_some(),
        "the open bracket is still an orphan"
    );

    // After a full serialize round-trip (cache is serde-skipped → reverse-scan),
    // the same semantics must hold.
    let json = serde_json::to_string(&map).unwrap();
    let back: SessionEventMap = serde_json::from_str(&json).unwrap();
    assert_eq!(
        back.current_compaction().map(|c| c.summary_text).as_deref(),
        Some("completed_at_set"),
        "reverse-scan must also return the last completed compaction"
    );
    assert!(back.orphaned_compaction().is_some());
}

/// The in-memory `compaction_event_index` cache and the post-deserialization
/// reverse-scan fallback must always return the SAME `current_compaction`. The
/// cache is `#[serde(skip)]`, so after a JSON round trip it is gone and the
/// reverse-scan becomes authoritative; if the two ever diverged, a live session
/// and a reloaded session would see different compaction state. Pin equivalence
/// across SetCompaction, ClearAll, balanced-bracket, orphaned-bracket, and
/// bracket-after-Set sequences.
#[test]
fn test_current_compaction_cache_matches_reverse_scan_after_reload() {
    let comp = |turn: usize| StoredCompactionState {
        summary_text: "s".to_string(),
        openai_encrypted_content: None,
        covers_up_to_turn: turn,
        original_turn_count: 10,
        compacted_count: turn,
    };

    let cases: Vec<Vec<SessionEvent>> = vec![
        // 1: plain SetCompaction.
        vec![SessionEvent {
            timestamp: Utc::now(),
            event_id: "set".to_string(),
            op: SessionEventOp::SetCompaction { compaction: comp(1) },
            parent_id: None,
            version: 1,
        }],
        // 2: SetCompaction then ClearAll -> cleared.
        vec![
            SessionEvent {
                timestamp: Utc::now(),
                event_id: "set".to_string(),
                op: SessionEventOp::SetCompaction { compaction: comp(1) },
                parent_id: None,
                version: 1,
            },
            SessionEvent {
                timestamp: Utc::now(),
                event_id: "clear".to_string(),
                op: SessionEventOp::ClearAll,
                parent_id: None,
                version: 1,
            },
        ],
        // 3: balanced bracket -> the completed compaction.
        vec![
            SessionEvent {
                timestamp: Utc::now(),
                event_id: "start".to_string(),
                op: SessionEventOp::CompactionStart {
                    compaction_id: "c".to_string(),
                    covers_up_to_turn: 1,
                },
                parent_id: None,
                version: 1,
            },
            SessionEvent {
                timestamp: Utc::now(),
                event_id: "end".to_string(),
                op: SessionEventOp::CompactionEnd { compaction: comp(1) },
                parent_id: None,
                version: 1,
            },
        ],
        // 4: orphaned bracket (no End) -> no completed compaction.
        vec![SessionEvent {
            timestamp: Utc::now(),
            event_id: "start".to_string(),
            op: SessionEventOp::CompactionStart {
                compaction_id: "c".to_string(),
                covers_up_to_turn: 1,
            },
            parent_id: None,
            version: 1,
        }],
        // 5: SetCompaction then an orphaned Start -> keeps the SetCompaction.
        vec![
            SessionEvent {
                timestamp: Utc::now(),
                event_id: "set".to_string(),
                op: SessionEventOp::SetCompaction { compaction: comp(1) },
                parent_id: None,
                version: 1,
            },
            SessionEvent {
                timestamp: Utc::now(),
                event_id: "start".to_string(),
                op: SessionEventOp::CompactionStart {
                    compaction_id: "c".to_string(),
                    covers_up_to_turn: 1,
                },
                parent_id: None,
                version: 1,
            },
        ],
    ];

    for (i, events) in cases.iter().enumerate() {
        let mut live = SessionEventMap::default();
        for e in events {
            live.append_event(e.clone());
        }
        let live_compaction = live.current_compaction();

        // Reload drops the in-memory cache (serde(skip)) -> reverse-scan is the
        // authority. Both paths must report identical compaction state.
        let json = serde_json::to_string(&live).expect("serialize");
        let reloaded: SessionEventMap = serde_json::from_str(&json).expect("deserialize");
        let reloaded_compaction = reloaded.current_compaction();
        assert_eq!(
            live_compaction, reloaded_compaction,
            "case {i}: live cache and reloaded reverse-scan must agree on current_compaction"
        );

        // The truth is the last persisting op: cross-check with the raw scan too.
        assert_eq!(
            reloaded_compaction,
            expected_last_compaction(events),
            "case {i}: reverse-scan must match the expected last persisting op"
        );
    }
}

/// Reference implementation of what the reverse-scan should report: the state of
/// the last `SetCompaction`/`CompactionEnd`, unless a later `ClearAll` clears it.
fn expected_last_compaction(events: &[SessionEvent]) -> Option<StoredCompactionState> {
    let mut found = None;
    for event in events.iter().rev() {
        match &event.op {
            SessionEventOp::SetCompaction { compaction }
            | SessionEventOp::CompactionEnd { compaction } => {
                found = Some(compaction.clone());
                break;
            }
            SessionEventOp::ClearAll => return None,
            _ => {}
        }
    }
    found
}

/// Validation robustness: a wildly-futuristic event timestamp must be rejected,
/// a far-past timestamp too, and duplicate event ids must be accepted (the log
/// deliberately permits the same id when the payload differs, e.g. re-inserting
/// a tool-output repair at the same index). These pin the validation contract so
/// a corrupt/rogue event cannot silently bend the append-only timestamp window.
#[test]
fn test_event_validation_rejects_extreme_timestamps_but_accepts_duplicate_ids() {
    let mut map = SessionEventMap::default();
    let msg = |id: &str| StoredMessage {
        id: id.to_string(),
        role: Role::User,
        content: vec![text_block("hi")],
        display_role: None,
        timestamp: None,
        tool_duration_ms: None,
        token_usage: None,
    };

    // Far-future timestamp (400 days > the ~1yr window) -> rejected.
    map.append_event(SessionEvent {
        timestamp: Utc::now() + chrono::Duration::days(400),
        event_id: "future".to_string(),
        op: SessionEventOp::AppendMessage {
            message_id: "m_future".to_string(),
            message: msg("m_future"),
        },
        parent_id: None,
        version: 1,
    });
    assert_eq!(
        map.events.len(),
        0,
        "a far-future event timestamp must be rejected by validation"
    );

    // Far-past timestamp (400 days ago) -> rejected.
    map.append_event(SessionEvent {
        timestamp: Utc::now() - chrono::Duration::days(400),
        event_id: "past".to_string(),
        op: SessionEventOp::AppendMessage {
            message_id: "m_past".to_string(),
            message: msg("m_past"),
        },
        parent_id: None,
        version: 1,
    });
    assert_eq!(map.events.len(), 0, "a far-past event timestamp must be rejected");

    // Duplicate event_id with different payload -> accepted (documented).
    let shared_id = "same_event_id".to_string();
    map.append_event(SessionEvent {
        timestamp: Utc::now(),
        event_id: shared_id.clone(),
        op: SessionEventOp::AppendMessage {
            message_id: "m_a".to_string(),
            message: msg("m_a"),
        },
        parent_id: None,
        version: 1,
    });
    map.append_event(SessionEvent {
        timestamp: Utc::now(),
        event_id: shared_id.clone(),
        op: SessionEventOp::AppendMessage {
            message_id: "m_b".to_string(),
            message: msg("m_b"),
        },
        parent_id: None,
        version: 1,
    });
    assert_eq!(
        map.events.len(),
        2,
        "duplicate event_id with distinct payloads must both be recorded"
    );
    assert_eq!(
        map.derive_messages().len(),
        2,
        "both duplicate-id messages must derive"
    );
}

/// `memory_profile_snapshot` (used for telemetry/metrics) must account for the
/// event-sourced log exactly like `debug_memory_profile` does. Without this the
/// cached `total_json_bytes` undercounts a session's footprint even though the
/// debug profile reports the event log, so the two observability surfaces would
/// disagree.
#[test]
fn test_memory_profile_snapshot_includes_event_log() {
    let mut session = Session::create_with_id("mp_snapshot_evt".to_string(), None, None);
    for i in 0..3 {
        session.append_stored_message(StoredMessage {
            id: format!("m{i}"),
            role: Role::User,
            content: vec![text_block(&format!("message {i}"))],
            display_role: None,
            timestamp: Some(Utc::now()),
            tool_duration_ms: None,
            token_usage: None,
        });
    }
    let snapshot = session.memory_profile_snapshot();
    assert_eq!(
        snapshot.event_log_count,
        session.event_map.events.len(),
        "snapshot event_log_count must match the live event map"
    );
    assert!(
        snapshot.event_log_json_bytes > 0,
        "snapshot event_log_json_bytes must be > 0 once events exist"
    );
    assert!(
        snapshot.total_json_bytes >= snapshot.event_log_json_bytes,
        "snapshot total_json_bytes must include the event log footprint"
    );
}

/// Forward-compat of the escape hatch: a FUTURE core build may promote an
/// `Unknown` op to a known variant whose payload shape differs from what this
/// build expects. Deserializing such an event must degrade to `Unknown`
/// (preserving the full payload) rather than failing to load the whole log.
#[test]
fn test_known_tag_with_mismatched_shape_degrades_to_unknown() {
    // `append_message` normally requires {message_id, message}. A payload with a
    // different shape (here `message_id` only) must NOT hard-error; it degrades.
    let raw = r#"{"op":"append_message","message_id":"m_x"}"#;
    let back: SessionEventOp = serde_json::from_str(raw).expect("must not error");
    match &back {
        SessionEventOp::Unknown { event_type, data } => {
            assert_eq!(event_type, "append_message");
            assert_eq!(
                data.get("message_id").and_then(|v| v.as_str()),
                Some("m_x"),
                "mismatched known-tag payload must be preserved as Unknown"
            );
        }
        other => panic!("expected degradation to Unknown, got {other:?}"),
    }
    // Reserialize: the degraded Unknown must round-trip stably.
    let re = serde_json::to_string(&back).expect("reserialize");
    let again: SessionEventOp = serde_json::from_str(&re).expect("re-deserialize");
    assert!(matches!(again, SessionEventOp::Unknown { .. }));
}

/// The public `Session::event_log()` read accessor lets plugins (and callers
/// outside `jcode-base`) enumerate the append-only event log — including their
/// own `Unknown` escape-hatch events — without reaching into the crate-private
/// `event_map`. Pins that the accessor returns the live committed log.
#[test]
fn test_public_event_log_accessor_exposes_committed_events() {
    let mut session = Session::create_with_id("event_log_accessor".to_string(), None, None);
    session.append_stored_message(StoredMessage {
        id: "m1".to_string(),
        role: Role::User,
        content: vec![text_block("hello")],
        display_role: None,
        timestamp: Some(Utc::now()),
        tool_duration_ms: None,
        token_usage: None,
    });
    session.append_session_event(SessionEvent {
        timestamp: Utc::now(),
        event_id: "plugin_evt".to_string(),
        op: SessionEventOp::Unknown {
            event_type: "plugin/marker".to_string(),
            data: serde_json::json!({ "k": "v" }),
        },
        parent_id: None,
        version: 1,
    });

    let log = session.event_log();
    // The message append + the plugin Unknown event are both committed.
    assert_eq!(log.len(), session.event_map.events.len());
    assert!(
        log.iter().any(|e| matches!(
            &e.op,
            SessionEventOp::Unknown { event_type, .. } if event_type == "plugin/marker"
        )),
        "the public accessor must expose the plugin Unknown event"
    );
}
