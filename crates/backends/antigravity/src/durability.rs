use crate::driver::payload_model;
use crate::harness::FAMILY;
use nanocode_agent_core::{
    HarnessFamily, SessionCheckpoint, Thinking,
    backend::TranscriptBuilder,
    input::PromptMessage,
    session::{ToolOutcome, TranscriptItem},
};
use nanocode_backend_antigravity_client::Content;
use nanocode_durability::{
    AdmittedTurn, CheckpointFormat, Error, FormatFuture, PayloadReader, Result, SessionRecord,
};
use serde_json::Value;

pub const FORMAT: &str = "antigravity.v1";

const REQUEST_OPEN: &str = "<USER_REQUEST>\n";
const REQUEST_CLOSE: &str = "\n</USER_REQUEST>";
const SYSTEM: &str = "<SYSTEM_MESSAGE>";
const SUMMARY: &str = "<CONTEXT_SUMMARY>";
const UNKNOWN: &str = "outcome is unknown";
const INTERRUPTED: &str = "outcome unknown";

pub struct AntigravityCheckpointFormat;

pub(crate) fn user_text(text: &str) -> Option<String> {
    if text.contains(SYSTEM) || text.starts_with(SUMMARY) {
        return None;
    }
    let Some(request) = text.strip_prefix(REQUEST_OPEN) else {
        return Some(text.to_owned());
    };
    Some(
        request
            .split_once(REQUEST_CLOSE)
            .map_or(request, |(request, _)| request)
            .to_owned(),
    )
}

fn result(response: &Value) -> (String, ToolOutcome) {
    let (key, body) = response
        .as_object()
        .and_then(|map| map.iter().next())
        .map_or(("output", String::new()), |(key, value)| {
            (
                key.as_str(),
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned),
            )
        });
    let outcome = match key {
        "error" if body.contains(UNKNOWN) || body.contains(INTERRUPTED) => ToolOutcome::Unknown,
        "error" => ToolOutcome::Failed,
        _ => ToolOutcome::Completed,
    };
    (body, outcome)
}

fn contents(checkpoint: &Value) -> Vec<Content> {
    checkpoint
        .get("contents")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|content| serde_json::from_value::<Content>(content.clone()).ok())
        .collect()
}

pub(crate) fn transcript_items(contents: &[Content]) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    let mut open: Vec<String> = Vec::new();
    for (index, content) in contents.iter().enumerate() {
        let mut answered = 0;
        for (position, part) in content.parts.iter().enumerate() {
            if let Some(call) = &part.function_call {
                let call_id = call
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("call-{index}-{position}"));
                open.push(call_id.clone());
                items.push(TranscriptItem::Tool {
                    call_id,
                    arguments: call.args.to_string(),
                    name: call.name.clone(),
                    parent_call_id: None,
                });
            } else if let Some(response) = &part.function_response {
                let call_id = response.id.clone().unwrap_or_else(|| {
                    open.get(answered)
                        .cloned()
                        .unwrap_or_else(|| format!("result-{index}-{position}"))
                });
                answered += 1;
                let (body, outcome) = result(&response.response);
                items.push(TranscriptItem::tool_result(call_id, &body, outcome));
            } else if let Some(text) = part.text.as_deref().filter(|text| !text.is_empty()) {
                match (content.role.as_str(), part.is_thought()) {
                    ("model", true) => items.push(TranscriptItem::Reasoning(text.to_owned())),
                    ("model", false) => items.push(TranscriptItem::Assistant(text.to_owned())),
                    _ => items.extend(user_text(text).map(TranscriptItem::User)),
                }
            }
        }
        if content.role == "user" && answered > 0 {
            open.clear();
        }
        if content.role == "model" && content.calls().next().is_none() {
            open.clear();
        }
    }
    items
}

pub(crate) fn prompt_messages(contents: &[Content]) -> Vec<PromptMessage> {
    let mut builder = TranscriptBuilder::default();
    for content in contents {
        for part in &content.parts {
            if part.is_thought() {
                continue;
            }
            if let Some(call) = &part.function_call {
                builder.tool_call(&call.name, &call.args.to_string());
            } else if let Some(response) = &part.function_response {
                builder.tool_result(&response.name, &result(&response.response).0);
            } else if let Some(text) = &part.text {
                match content.role.as_str() {
                    "model" => builder.assistant(text),
                    _ => {
                        if let Some(text) = user_text(text) {
                            builder.user(&text);
                        }
                    }
                }
            }
        }
    }
    builder.finish()
}

impl CheckpointFormat for AntigravityCheckpointFormat {
    fn family(&self) -> HarnessFamily {
        HarnessFamily::new(FAMILY)
    }

    fn owns(&self, checkpoint: &Value) -> bool {
        checkpoint.get("format").and_then(Value::as_str) == Some(FORMAT)
    }

    fn hydrate_continuation<'a>(
        &'a self,
        _reader: PayloadReader<'a>,
        continuation: Value,
    ) -> FormatFuture<'a, Result<Option<Value>>> {
        let owned = self.owns(&continuation);
        Box::pin(async move { Ok(owned.then_some(continuation)) })
    }

    fn hydrate_cursor<'a>(
        &'a self,
        _reader: PayloadReader<'a>,
        continuation: Value,
    ) -> FormatFuture<'a, Result<Option<Value>>> {
        let owned = self.owns(&continuation);
        Box::pin(async move { Ok(owned.then_some(continuation)) })
    }

    fn session_checkpoint(
        &self,
        record: &SessionRecord,
        mut checkpoint: Value,
    ) -> Result<SessionCheckpoint> {
        let model = payload_model(&checkpoint).unwrap_or_else(|| record.model.clone());
        let thinking = checkpoint_thinking(record, &checkpoint)?;
        if let Some(map) = checkpoint.as_object_mut() {
            map.insert("operation".to_owned(), Value::Null);
            map.insert("progress".to_owned(), Value::Null);
        }
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

    fn thinking(&self, record: &SessionRecord, checkpoint: &Value) -> Result<Thinking> {
        checkpoint_thinking(record, checkpoint)
    }

    fn transcript(&self, checkpoint: &Value, _turns: &[AdmittedTurn<'_>]) -> Vec<TranscriptItem> {
        transcript_items(&contents(checkpoint))
    }
}

fn checkpoint_thinking(record: &SessionRecord, checkpoint: &Value) -> Result<Thinking> {
    match checkpoint.get("thinking") {
        None | Some(Value::Null) => Ok(record.model.default_thinking()),
        Some(thinking) => serde_json::from_value(thinking.clone()).map_err(|error| {
            Error::InvalidState(format!("antigravity checkpoint thinking: {error}"))
        }),
    }
}
