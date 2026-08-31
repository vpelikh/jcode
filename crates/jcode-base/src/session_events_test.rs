#![allow(unused_variables, unused_imports)]

use crate::session::event_types::SessionEventOp;
use crate::session::Session;
use crate::session::event_types::{SessionEvent, SessionEventMap};
use crate::message::ContentBlock;
use jcode_session_types::{StoredCompactionState, StoredMemoryInjection, StoredMessage};
use jcode_message_types::Role;

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

    session.sync_backward_compatibility();
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
            content: vec![],
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
        content: vec![],
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
        content: vec![],
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
