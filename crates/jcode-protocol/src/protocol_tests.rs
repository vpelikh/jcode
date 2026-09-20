use super::*;
use anyhow::{Result, anyhow};

fn parse_request_json(json: &str) -> Result<Request> {
    serde_json::from_str(json).map_err(Into::into)
}

fn parse_event_json(json: &str) -> Result<ServerEvent> {
    serde_json::from_str(json).map_err(Into::into)
}

include!("protocol_tests/core_events.rs");
include!("protocol_tests/comm_requests.rs");
include!("protocol_tests/comm_responses.rs");
include!("protocol_tests/comm_format_awaited.rs");
include!("protocol_tests/misc_events.rs");
include!("protocol_tests/randomized.rs");
include!("protocol_tests/model_usage.rs");

#[test]
fn prune_request_and_result_wire_roundtrip() {
    let request = super::decode_request(r#"{"type":"prune","id":42}"#).unwrap();
    assert!(matches!(request, super::Request::Prune { id: 42 }));
    assert_eq!(request.id(), 42);
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        serde_json::json!({"type":"prune","id":42})
    );
    for (images, results) in [(0, 0), (2, 3)] {
        let wire = serde_json::json!({
            "type": "prune_result", "id": 42,
            "images_stripped": images, "tool_results_truncated": results,
            "message": "Prune complete"
        });
        let event: super::ServerEvent = serde_json::from_value(wire.clone()).unwrap();
        assert!(matches!(&event, super::ServerEvent::PruneResult {
            id: 42, images_stripped, tool_results_truncated, message
        } if *images_stripped == images && *tool_results_truncated == results && message == "Prune complete"));
        assert_eq!(serde_json::to_value(event).unwrap(), wire);
    }
}

#[test]
fn handoff_list_and_import_wire_roundtrip() {
    let list = super::decode_request(r#"{"type":"handoff_list","id":7}"#).unwrap();
    assert!(matches!(list, super::Request::HandoffList { id: 7 }));
    assert_eq!(list.id(), 7);
    assert_eq!(
        serde_json::to_value(&list).unwrap(),
        serde_json::json!({"type":"handoff_list","id":7})
    );

    let import = super::decode_request(
        r#"{"type":"handoff_import","id":8,"payload":"{\"session_id\":\"src\"}"}"#,
    )
    .unwrap();
    assert!(matches!(&import, super::Request::HandoffImport {
        id: 8, payload, disposition: None
    } if payload == "{\"session_id\":\"src\"}"));
    assert_eq!(import.id(), 8);

    let listed: super::ServerEvent = serde_json::from_value(serde_json::json!({
        "type": "handoff_listed",
        "id": 7,
        "handoffs": [{
            "session_id": "import-src",
            "project_key": "git:https://example.com/repo",
            "ended_at": "2026-09-19T10:00:00Z",
            "disposition": "closed",
            "open_todos": [
                {"id": "t1", "content": "one", "status": "in_progress"},
                {"id": "t2", "content": "two", "status": "in_progress"},
                {"id": "t3", "content": "three", "status": "in_progress"}
            ],
            "intent": "Finish the plan",
        }]
    }))
    .unwrap();
    assert!(matches!(&listed, super::ServerEvent::HandoffListed {
        id: 7, handoffs
    } if handoffs.len() == 1 && handoffs[0].session_id == "import-src"
        && handoffs[0].open_todos.len() == 3
        && handoffs[0].intent.as_deref() == Some("Finish the plan")
        && handoffs[0].payload.is_none()));

    let imported: super::ServerEvent = serde_json::from_value(serde_json::json!({
        "type": "handoff_imported",
        "id": 8,
        "session_id": "import-src",
    }))
    .unwrap();
    assert!(matches!(&imported, super::ServerEvent::HandoffImported {
        id: 8, session_id
    } if session_id == "import-src"));
}

#[test]
fn handoff_apply_wire_roundtrip() {
    let apply = super::decode_request(
        r#"{"type":"handoff_apply","id":9,"payload":"{\"session_id\":\"src\"}","disposition":"interrupted"}"#,
    )
    .unwrap();
    assert!(matches!(&apply, super::Request::HandoffApply {
        id: 9, payload, disposition: Some(d)
    } if payload == "{\"session_id\":\"src\"}" && d == "interrupted"));
    assert_eq!(apply.id(), 9);
    assert_eq!(
        serde_json::to_value(&apply).unwrap(),
        serde_json::json!({"type":"handoff_apply","id":9,"payload":"{\"session_id\":\"src\"}","disposition":"interrupted"})
    );

    // disposition is optional (defaults to "closed" server-side).
    let apply_min = super::decode_request(
        r#"{"type":"handoff_apply","id":10,"payload":"{\"session_id\":\"src\"}"}"#,
    )
    .unwrap();
    assert!(matches!(apply_min, super::Request::HandoffApply {
        id: 10, disposition: None, ..
    }));
}

#[test]
fn handoff_resume_by_id_wire_roundtrip() {
    let resume = super::decode_request(
        r#"{"type":"handoff_resume_by_id","id":11,"session_id":"handoff-abc"}"#,
    )
    .unwrap();
    assert!(matches!(&resume, super::Request::HandoffResumeById {
        id: 11, session_id
    } if session_id == "handoff-abc"));
    assert_eq!(resume.id(), 11);
    assert_eq!(
        serde_json::to_value(&resume).unwrap(),
        serde_json::json!({"type":"handoff_resume_by_id","id":11,"session_id":"handoff-abc"})
    );

    // The success event carries the armed session id back to the client.
    let resumed: super::ServerEvent = serde_json::from_value(serde_json::json!({
        "type": "handoff_resumed",
        "id": 11,
        "session_id": "handoff-abc",
    }))
    .unwrap();
    assert!(matches!(&resumed, super::ServerEvent::HandoffResumed {
        id: 11, session_id
    } if session_id == "handoff-abc"));
}
