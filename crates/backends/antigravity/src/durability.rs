use crate::harness::{AntigravityHarness, FAMILY};
use nanocode_agent_core::{
    HarnessFamily, SessionCheckpoint, Thinking,
    session::{ToolOutcome, TranscriptItem},
};
use nanocode_backend_antigravity_client::Content;
use nanocode_durability::{AdmittedTurn, CheckpointFormat, Error, Result, SessionRecord};
use serde_json::Value;

pub const FORMAT: &str = "antigravity.v1";

pub struct AntigravityCheckpointFormat;

impl CheckpointFormat for AntigravityCheckpointFormat {
    fn family(&self) -> HarnessFamily {
        HarnessFamily::new(FAMILY)
    }

    fn owns(&self, checkpoint: &Value) -> bool {
        checkpoint.get("format").and_then(Value::as_str) == Some(FORMAT)
    }

    fn session_checkpoint(
        &self,
        record: &SessionRecord,
        checkpoint: Value,
    ) -> Result<SessionCheckpoint> {
        let model = checkpoint
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
            .map_or_else(|| record.model.clone(), AntigravityHarness::selection);
        let thinking = checkpoint_thinking(&checkpoint)?;
        Ok(SessionCheckpoint::native(
            record.session_id.clone(),
            model,
            thinking,
            record.lineage.clone(),
            record.session_id.clone(),
            None,
            true,
            checkpoint,
        ))
    }

    fn thinking(&self, _record: &SessionRecord, checkpoint: &Value) -> Result<Thinking> {
        checkpoint_thinking(checkpoint)
    }

    fn transcript(&self, checkpoint: &Value, _turns: &[AdmittedTurn<'_>]) -> Vec<TranscriptItem> {
        let mut items = Vec::new();
        let contents = checkpoint
            .get("contents")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for content in contents {
            let Ok(content) = serde_json::from_value::<Content>(content.clone()) else {
                continue;
            };
            for part in content.parts {
                if let Some(call) = part.function_call {
                    items.push(TranscriptItem::Tool {
                        call_id: call.id.unwrap_or_else(|| call.name.clone()),
                        arguments: call.args.to_string(),
                        name: call.name,
                        parent_call_id: None,
                    });
                } else if let Some(response) = part.function_response {
                    items.push(TranscriptItem::ToolResult {
                        call_id: response.id.unwrap_or(response.name),
                        output: response.response.to_string(),
                        outcome: ToolOutcome::Completed,
                    });
                } else if let Some(text) = part.text.filter(|text| !text.is_empty()) {
                    items.push(match (content.role.as_str(), part.thought == Some(true)) {
                        ("model", true) => TranscriptItem::Reasoning(text),
                        ("model", false) => TranscriptItem::Assistant(text),
                        _ => TranscriptItem::User(text),
                    });
                }
            }
        }
        items
    }
}

fn checkpoint_thinking(checkpoint: &Value) -> Result<Thinking> {
    checkpoint
        .get("thinking")
        .cloned()
        .map_or(Ok(Thinking::default()), serde_json::from_value)
        .map_err(|error| Error::InvalidState(format!("antigravity checkpoint thinking: {error}")))
}
