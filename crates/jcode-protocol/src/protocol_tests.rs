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
