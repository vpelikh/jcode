use serde_json::Value;

/// Accumulates a remote tool call's streamed input JSON.
///
/// The client does not regenerate edit diffs: the server-side tools emit their
/// own line-numbered diffs, so the remote event handler passes the server
/// output through unchanged. This accumulator exists so the event loop can
/// parse a tool's input before it runs (`get_current_tool_input`), which drives
/// tool intent and the observe panel.
#[derive(Default)]
pub(crate) struct RemoteToolInput {
    current_tool_input: String,
}

impl RemoteToolInput {
    pub(crate) fn handle_tool_start(&mut self) {
        self.current_tool_input.clear();
    }

    pub(crate) fn handle_tool_input(&mut self, delta: &str) {
        self.current_tool_input.push_str(delta);
    }

    pub(crate) fn current_tool_input_json(&self) -> Value {
        serde_json::from_str(&self.current_tool_input).unwrap_or(Value::Null)
    }

    pub(crate) fn clear(&mut self) {
        self.current_tool_input.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::RemoteToolInput;
    use serde_json::{Value, json};

    #[test]
    fn accumulates_and_parses_streamed_input() {
        let mut input = RemoteToolInput::default();
        input.handle_tool_start();
        input.handle_tool_input("{\"file_");
        input.handle_tool_input("path\": \"demo.txt\"}");
        assert_eq!(
            input.current_tool_input_json(),
            json!({ "file_path": "demo.txt" })
        );
    }

    #[test]
    fn start_resets_accumulated_input() {
        let mut input = RemoteToolInput::default();
        input.handle_tool_input("garbage");
        input.handle_tool_start();
        assert_eq!(input.current_tool_input_json(), Value::Null);
    }
}
