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
    // 2) truncate to first two (partial replace [0..2])
    map.append_event(SessionEvent {
        timestamp: chrono::Utc::now(),
        event_id: "truncate".to_string(),
        op: SessionEventOp::ReplaceMessages {
            start_index: 0,
            end_index: 2,
            messages: vec![mk("a"), mk("b")],
        },
        parent_id: None,
        version: 1,
    });
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
    let fork = session.fork_up_to_boundary(2);
    assert_eq!(fork.derive_messages().len(), 3);
    assert_ne!(fork.id, session.id);
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
