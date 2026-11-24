use std::collections::{BTreeMap, BTreeSet};

use nanocode_agent_core::input::Prompt;
use nanocode_durability_core::{
    Base, BlockRef, Decision, DurableFold, EffectResult, Fact, FoldError, Input, Intent,
    IntentTarget, ModelOut, Seen,
};
use nanocode_tool_host::{ToolOutput, contract::ToolOutputWire};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

use crate::{ContentBlock, Message, MessageResponse, MessagesRequest, Role, ToolResultContent};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeDecision {
    Template {
        request: MessagesRequest,
        tool_result_tokens: u64,
    },
    Message(Message),
    Rendered(ContentBlock),
    Compacting {
        retained: u32,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ClaudeFold {
    template: Option<Template>,
    summary: String,
    messages: Vec<Message>,
    previous_message_id: Option<String>,
    container: Option<String>,
    queued: Vec<Message>,
    nested: BTreeSet<String>,
    phase: Phase,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Template {
    request: MessagesRequest,
    tool_result_tokens: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    #[default]
    Ready,
    Compacting {
        retained: u32,
    },
    Tools {
        model_out: u64,
        calls: Vec<Call>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Call {
    id: String,
    name: String,
    block: u32,
    intended: bool,
    output: bool,
    result: Option<ContentBlock>,
}

#[derive(Deserialize)]
struct Legacy {
    #[serde(default)]
    snapshot: Option<LegacySnapshot>,
    #[serde(default)]
    conversation: Option<LegacyConversation>,
    #[serde(default)]
    template: Option<MessagesRequest>,
    #[serde(default, deserialize_with = "crate::agent::stored_messages")]
    pending: Vec<Message>,
}

#[derive(Deserialize)]
struct LegacySnapshot {
    conversation: LegacyConversation,
}

#[derive(Deserialize)]
struct LegacyConversation {
    #[serde(default, deserialize_with = "crate::agent::stored_messages")]
    messages: Vec<Message>,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    previous_message_id: Option<String>,
    #[serde(default)]
    container: Option<String>,
}

fn rejected(seq: u64, reason: impl Into<String>) -> FoldError {
    FoldError::Rejected {
        seq,
        reason: reason.into(),
    }
}

fn decode<T: serde::de::DeserializeOwned>(
    seq: u64,
    what: &str,
    json: &RawValue,
) -> Result<T, FoldError> {
    serde_json::from_str(json.get()).map_err(|error| FoldError::Decode {
        seq,
        reason: format!("Claude {what} does not decode: {error}"),
    })
}

impl DurableFold for ClaudeFold {
    const FAMILY: &'static str = "claude";
    const FORMAT: u32 = 1;
    type Decision = ClaudeDecision;

    fn apply(&mut self, seq: u64, fact: Seen<'_, ClaudeDecision>) -> Result<(), FoldError> {
        match fact {
            Fact::Base(base) => self.base(seq, &base),
            Fact::Input(input) => self.input(seq, &input),
            Fact::ModelOut(out) => self.model_out(seq, &out),
            Fact::Intent(intent) => self.intent(seq, &intent),
            Fact::EffectResult(result) => self.effect_result(seq, &result),
            Fact::Decision(decision) => self.decision(seq, decision),
            Fact::Marker(_) => Ok(()),
        }
    }

    fn request(&self) -> Result<Vec<u8>, FoldError> {
        let template = self
            .template
            .as_ref()
            .ok_or_else(|| FoldError::Encode("the Claude fold has no request template".into()))?;
        let mut request = template.request.clone();
        request.messages = self.packed();
        request.container.clone_from(&self.container);
        serde_json::to_vec(&request).map_err(|error| FoldError::Encode(error.to_string()))
    }
}

impl ClaudeFold {
    fn packed(&self) -> Vec<Message> {
        let mut messages = Vec::with_capacity(self.messages.len().saturating_add(1));
        if !self.summary.is_empty() {
            messages.push(Message::text(
                Role::User,
                format!("{}\n\n{}", crate::agent::COMPACT_CONTINUED, self.summary),
            ));
        }
        messages.extend(self.messages.iter().cloned());
        messages
    }

    fn base(&mut self, seq: u64, base: &Base<&RawValue>) -> Result<(), FoldError> {
        if !self.messages.is_empty() || self.template.is_some() || !self.summary.is_empty() {
            return Err(rejected(seq, "a Claude base seeds only an empty fold"));
        }
        let legacy: Legacy = decode(seq, "checkpoint", base.checkpoint)?;
        let conversation = match (legacy.snapshot, legacy.conversation) {
            (Some(snapshot), None) => snapshot.conversation,
            (None, Some(conversation)) => conversation,
            (Some(_), Some(_)) => {
                return Err(rejected(seq, "a Claude checkpoint holds two conversations"));
            }
            (None, None) => return Err(rejected(seq, "a Claude checkpoint holds no conversation")),
        };
        self.messages = if legacy.pending.is_empty() {
            conversation.messages
        } else {
            legacy.pending
        };
        self.summary = conversation.summary;
        self.previous_message_id = conversation.previous_message_id;
        self.container = conversation.container;
        self.template = legacy.template.map(|mut request| {
            request.messages.clear();
            Template {
                request,
                tool_result_tokens: crate::agent::TOOL_RESULT_TOKEN_LIMIT,
            }
        });
        Ok(())
    }

    fn input(&mut self, seq: u64, input: &Input<&RawValue>) -> Result<(), FoldError> {
        let prompt: Prompt = decode(seq, "prompt", input.value)?;
        let messages =
            crate::prompt::messages(&prompt).map_err(|error| rejected(seq, error.to_string()))?;
        match self.phase {
            Phase::Ready => self.messages.extend(messages),
            Phase::Compacting { .. } | Phase::Tools { .. } => self.queued.extend(messages),
        }
        Ok(())
    }

    fn model_out(&mut self, seq: u64, out: &ModelOut<&RawValue>) -> Result<(), FoldError> {
        let response: MessageResponse = decode(seq, "response", out.response)?;
        if response.role != Role::Assistant {
            return Err(rejected(seq, "a Claude response role is not assistant"));
        }
        self.previous_message_id = Some(response.id.clone());
        if let Some(id) = response
            .container
            .as_ref()
            .and_then(|container| container.get("id"))
            .and_then(Value::as_str)
        {
            self.container = Some(id.to_owned());
        }
        match std::mem::take(&mut self.phase) {
            Phase::Ready => {
                self.answer(seq, response.content);
                Ok(())
            }
            Phase::Compacting { retained } => self.compacted(seq, retained, &response.content),
            phase @ Phase::Tools { .. } => {
                self.phase = phase;
                Err(rejected(
                    seq,
                    "a Claude response arrived before its round's tool results",
                ))
            }
        }
    }

    fn answer(&mut self, seq: u64, content: Vec<ContentBlock>) {
        let calls = content
            .iter()
            .enumerate()
            .filter_map(|(block, content)| match content {
                ContentBlock::ToolUse { id, name, .. } => Some(Call {
                    id: id.clone(),
                    name: name.clone(),
                    block: u32::try_from(block)
                        .expect("a Claude response holds fewer blocks than u32 counts"),
                    intended: false,
                    output: false,
                    result: None,
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.messages.push(Message {
            role: Role::Assistant,
            content,
            output_config: None,
        });
        if !calls.is_empty() {
            self.phase = Phase::Tools {
                model_out: seq,
                calls,
            };
        }
    }

    fn compacted(
        &mut self,
        seq: u64,
        retained: u32,
        content: &[ContentBlock],
    ) -> Result<(), FoldError> {
        let retained = usize::try_from(retained).expect("u32 fits usize");
        let Some(start) = self.messages.len().checked_sub(retained) else {
            return Err(rejected(
                seq,
                "a Claude compaction retains more messages than it holds",
            ));
        };
        let summary = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        if summary.is_empty() {
            return Err(rejected(
                seq,
                "a Claude compaction response holds no summary",
            ));
        }
        self.messages.drain(..start);
        self.summary = summary;
        self.flush_queued();
        Ok(())
    }

    fn intent(&mut self, seq: u64, intent: &Intent<&RawValue>) -> Result<(), FoldError> {
        let block = match &intent.target {
            IntentTarget::Nested { .. } => {
                if !self.nested.insert(intent.call_id.clone()) {
                    return Err(rejected(seq, "a nested Claude call is intended twice"));
                }
                return Ok(());
            }
            IntentTarget::ToolUse(block) => *block,
        };
        let call = self.call(seq, &intent.call_id)?;
        let (model_out, call) = call;
        if (BlockRef {
            model_out,
            block: call.block,
        }) != block
        {
            return Err(rejected(
                seq,
                "a Claude intent names another response block than its tool_use",
            ));
        }
        if call.name != intent.tool {
            return Err(rejected(
                seq,
                "a Claude intent names another tool than its tool_use",
            ));
        }
        if call.intended {
            return Err(rejected(seq, "a Claude tool_use is intended twice"));
        }
        call.intended = true;
        Ok(())
    }

    fn call(&mut self, seq: u64, id: &str) -> Result<(u64, &mut Call), FoldError> {
        let Phase::Tools { model_out, calls } = &mut self.phase else {
            return Err(rejected(
                seq,
                "a Claude tool call arrived outside a tool round",
            ));
        };
        let model_out = *model_out;
        calls
            .iter_mut()
            .find(|call| call.id == id)
            .map(|call| (model_out, call))
            .ok_or_else(|| rejected(seq, "a Claude tool call names no tool_use of its round"))
    }

    fn effect_result(
        &mut self,
        seq: u64,
        result: &EffectResult<&RawValue>,
    ) -> Result<(), FoldError> {
        if self.nested.remove(&result.call_id) {
            return Ok(());
        }
        let wire: ToolOutputWire = decode(seq, "tool output", result.output)?;
        let tokens = self
            .template
            .as_ref()
            .map_or(crate::agent::TOOL_RESULT_TOKEN_LIMIT, |template| {
                template.tool_result_tokens
            });
        let (_, call) = self.call(seq, &result.call_id)?;
        if !call.intended {
            return Err(rejected(
                seq,
                "a Claude tool result has no intent before it",
            ));
        }
        if call.output {
            return Err(rejected(seq, "a Claude tool result arrived twice"));
        }
        call.output = true;
        call.result =
            rendered(&call.id, &call.name, wire, tokens).map_err(|reason| rejected(seq, reason))?;
        self.settle();
        Ok(())
    }

    fn settle(&mut self) {
        let Phase::Tools { calls, .. } = &self.phase else {
            return;
        };
        if calls.iter().any(|call| call.result.is_none()) {
            return;
        }
        let Phase::Tools { calls, .. } = std::mem::take(&mut self.phase) else {
            unreachable!("the phase was just matched");
        };
        let results = calls
            .into_iter()
            .map(|call| {
                call.result
                    .expect("every call of a settled round has its result")
            })
            .collect();
        self.messages.push(Message::tool_results(results));
        self.flush_queued();
    }

    fn flush_queued(&mut self) {
        self.messages.append(&mut self.queued);
    }

    fn decision(&mut self, seq: u64, decision: Decision<&ClaudeDecision>) -> Result<(), FoldError> {
        match decision {
            Decision::Time { .. } | Decision::Id { .. } => Ok(()),
            Decision::Route { model } => {
                let template = self.template.as_mut().ok_or_else(|| {
                    rejected(seq, "a Claude route switch arrived before its template")
                })?;
                template.request.model = model;
                Ok(())
            }
            Decision::Backend(decision) => self.backend(seq, decision),
        }
    }

    fn backend(&mut self, seq: u64, decision: &ClaudeDecision) -> Result<(), FoldError> {
        match (decision, &mut self.phase) {
            (
                ClaudeDecision::Template {
                    request,
                    tool_result_tokens,
                },
                _,
            ) => {
                if !request.messages.is_empty() {
                    return Err(rejected(seq, "a Claude template carries messages"));
                }
                self.template = Some(Template {
                    request: request.clone(),
                    tool_result_tokens: *tool_result_tokens,
                });
                Ok(())
            }
            (ClaudeDecision::Message(message), Phase::Ready) => {
                self.messages.push(message.clone());
                Ok(())
            }
            (ClaudeDecision::Compacting { retained }, Phase::Ready) => {
                self.phase = Phase::Compacting {
                    retained: *retained,
                };
                Ok(())
            }
            (ClaudeDecision::Rendered(block), Phase::Tools { .. }) => self.rendered(seq, block),
            (ClaudeDecision::Message(_) | ClaudeDecision::Compacting { .. }, _) => Err(rejected(
                seq,
                "a Claude decision arrived inside a round that it cannot change",
            )),
            (ClaudeDecision::Rendered(_), _) => Err(rejected(
                seq,
                "a rendered Claude tool result arrived outside a tool round",
            )),
        }
    }

    fn rendered(&mut self, seq: u64, block: &ContentBlock) -> Result<(), FoldError> {
        let ContentBlock::ToolResult { tool_use_id, .. } = block else {
            return Err(rejected(
                seq,
                "a rendered Claude tool result is not a tool_result block",
            ));
        };
        let (_, call) = self.call(seq, tool_use_id)?;
        if !call.output || call.result.is_some() {
            return Err(rejected(
                seq,
                "a rendered Claude tool result has no unrendered output",
            ));
        }
        call.result = Some(block.clone());
        self.settle();
        Ok(())
    }
}

fn rendered(
    id: &str,
    name: &str,
    wire: ToolOutputWire,
    tokens: u64,
) -> Result<Option<ContentBlock>, String> {
    let output = ToolOutput::from_wire(wire).map_err(|error| error.to_string())?;
    let reply = crate::claude_tools::tool_reply(output)?;
    if let ToolResultContent::Blocks(blocks) = &reply.content
        && blocks.iter().any(|block| block["type"] != "text")
    {
        return Ok(None);
    }
    let content = match name {
        "Read" => reply.content,
        "exec" | "wait" => crate::agent::bound_tool_result(reply.content, tokens).0,
        _ => {
            crate::agent::bound_tool_result(
                reply.content,
                crate::agent::DIRECT_TOOL_RESULT_TOKEN_LIMIT,
            )
            .0
        }
    };
    Ok(Some(ContentBlock::ToolResult {
        tool_use_id: id.to_owned(),
        content,
        is_error: (reply.is_error || name == "Bash").then_some(reply.is_error),
        extra: BTreeMap::new(),
    }))
}
