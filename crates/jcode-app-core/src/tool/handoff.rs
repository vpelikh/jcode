//! The `handoff` tool: let the model save an explicit handoff, optionally with
//! a continuation task for the next session.
//!
//! Automatic capture at disconnect is mechanical and untargeted. This tool is
//! the deliberate counterpart: when the user says "save a handoff so the next
//! session reviews this branch", the model calls `handoff` with the task, and
//! the resumed session boots *ready to perform that task* rather than only being
//! aware of the old context.
//!
//! The snapshot is written with `disposition == "saved"` and is purely additive
//! (it never retires the live handoff when there is no work). The stored
//! continuation prompt is rendered at the top of the boot context by
//! [`crate::handoff::render_boot_context`].

use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub struct HandoffTool;

impl HandoffTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct HandoffInput {
    /// Action to perform: `save` (default) or `clear`.
    #[serde(default = "default_action")]
    action: String,
    /// Optional explicit continuation task/prompt for the resumed session.
    #[serde(default)]
    prompt: Option<String>,
}

fn default_action() -> String {
    "save".to_string()
}

#[async_trait]
impl Tool for HandoffTool {
    fn name(&self) -> &str {
        "handoff"
    }

    fn description(&self) -> &str {
        "Save or clear a handoff so a later session continues this work."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["save", "clear"],
                    "description": "Action: save a handoff, or clear its saved task."
                },
                "prompt": {
                    "type": "string",
                    "description": "Continuation task the next session should perform, e.g. review this branch's changes."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: HandoffInput = serde_json::from_value(input)?;
        let working_dir = ctx.working_dir.clone();
        let prompt = params.prompt.clone();
        let session_id = ctx.session_id.clone();

        if params.action == "clear" {
            // Drop the continuation task saved for this session; any open work
            // in the snapshot is left intact.
            let outcome = tokio::task::spawn_blocking(move || {
                crate::handoff::clear_continuation_prompt(&session_id)
            })
            .await?;
            return Ok(match outcome {
                Some(o) if o.removed => ToolOutput::new(
                    "Cleared the saved continuation task; the snapshot had no open work left and was removed.",
                )
                .with_title("Handoff task cleared"),
                Some(o) if o.had_task => ToolOutput::new(
                    "Cleared the saved continuation task; any open work in the snapshot is untouched.",
                )
                .with_title("Handoff task cleared"),
                Some(_) => ToolOutput::new("No saved continuation task to clear.")
                    .with_title("Handoff task: nothing to clear"),
                None => ToolOutput::new("No saved handoff for this session.")
                    .with_title("Handoff task: nothing to clear"),
            });
        }
        if params.action != "save" {
            anyhow::bail!(
                "unsupported action {:?}: supported actions are `save` and `clear`",
                params.action
            );
        }
        // Capture is disk + git work; keep it off the async executor.
        let saved = tokio::task::spawn_blocking(move || {
            crate::handoff::save_now_with_prompt(
                &session_id,
                working_dir.as_deref(),
                None,
                prompt.as_deref(),
            )
        })
        .await?;

        match saved {
            Some(snapshot) => {
                let mut summary = format!("Saved handoff `{}`.", snapshot.session_id);
                if let Some(prompt) = &snapshot.continuation_prompt {
                    summary.push_str(&format!(
                        "\nContinuation task: {prompt}\nA later session in this project will boot with this task."
                    ));
                } else {
                    summary.push_str(
                        "\nA later session in this project can resume from the captured open work.",
                    );
                }
                Ok(ToolOutput::new(summary)
                    .with_title("Handoff saved")
                    .with_metadata(json!({
                        "session_id": snapshot.session_id,
                        "has_prompt": snapshot.continuation_prompt.is_some(),
                        "open_todos": snapshot.open_todos.len(),
                    })))
            }
            None => Ok(ToolOutput::new(
                "Nothing to save: the session has no open todos, no saved plan intent, and no continuation prompt was given, or its project could not be resolved (no working directory). If the user did ask to save a handoff, call `handoff` again with a `prompt` stating the task the next session should pick up (or track the work with the `todo` tool first).",
            )
            .with_title("Handoff: nothing to save")),
        }
    }
}

#[cfg(test)]
#[path = "handoff_tool_tests.rs"]
mod tests;
