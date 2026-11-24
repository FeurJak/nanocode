use std::collections::{BTreeMap, BTreeSet, VecDeque};

use nanocode_agent_core::input::Prompt;
use nanocode_durability_core::{
    Base, BlockRef, Decision, DurableFold, EffectResult, Fact, FoldError, Input, InputKind, Intent,
    IntentTarget, Marker, ModelOut, Seen, TurnStatus,
};
use nanocode_tool_host::{
    ToolOutput,
    contract::{ToolOutputContent, ToolOutputWire},
};
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
    Admitted {
        input: u32,
    },
    Rendered(ContentBlock),
    Compacting {
        retained: u32,
    },
    Splice {
        from: u32,
        to: u32,
        messages: Vec<Message>,
    },
    Carried {
        container: Option<String>,
        previous_message_id: Option<String>,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ClaudeFold {
    template: Option<Template>,
    messages: Vec<Message>,
    previous_message_id: Option<String>,
    container: Option<String>,
    inputs: VecDeque<Queued>,
    nested: BTreeSet<String>,
    abandoned: BTreeSet<String>,
    phase: Phase,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Queued {
    turn: String,
    message: Message,
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
        assistant: usize,
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

pub(crate) struct OpenCall<'a> {
    pub(crate) block: BlockRef,
    pub(crate) name: &'a str,
    pub(crate) intended: bool,
    pub(crate) output: bool,
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

#[derive(Serialize)]
struct Undecided<'a> {
    messages: &'a [Message],
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

const fn is_thinking(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
    )
}

fn summarized(summary: &str, messages: Vec<Message>) -> Vec<Message> {
    let mut packed = vec![Message::text(
        Role::User,
        format!("{}\n\n{summary}", crate::agent::COMPACT_CONTINUED),
    )];
    packed.extend(messages.into_iter().filter_map(|mut message| {
        message.content.retain(|block| !is_thinking(block));
        (!message.content.is_empty() || message.is_effort_update()).then_some(message)
    }));
    packed
}

impl DurableFold for ClaudeFold {
    const FAMILY: &'static str = "claude";
    const FORMAT: u32 = 3;
    type Decision = ClaudeDecision;

    fn apply(&mut self, seq: u64, fact: Seen<'_, ClaudeDecision>) -> Result<(), FoldError> {
        match fact {
            Fact::Base(base) => self.base(seq, &base),
            Fact::Input(input) => self.input(seq, &input),
            Fact::ModelOut(out) => self.model_out(seq, &out),
            Fact::Intent(intent) => self.intent(seq, &intent),
            Fact::EffectResult(result) => self.effect_result(seq, &result),
            Fact::Decision(decision) => self.decision(seq, decision),
            Fact::Marker(Marker::Status {
                turn,
                status: TurnStatus::Finished { .. },
            }) => {
                self.inputs.retain(|queued| queued.turn != turn);
                Ok(())
            }
            Fact::Marker(_) => Ok(()),
        }
    }

    fn request(&self) -> Result<Vec<u8>, FoldError> {
        let encoded = match &self.template {
            Some(template) => {
                let mut request = template.request.clone();
                request.messages.clone_from(&self.messages);
                request.container.clone_from(&self.container);
                if request.diagnostics.is_some() {
                    request.diagnostics =
                        Some(serde_json::json!({"previous_message_id": self.previous_message_id}));
                }
                serde_json::to_vec(&request)
            }
            None => serde_json::to_vec(&Undecided {
                messages: &self.messages,
            }),
        };
        encoded.map_err(|error| FoldError::Encode(error.to_string()))
    }
}

impl ClaudeFold {
    pub(crate) fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub(crate) fn template(&self) -> Option<(&MessagesRequest, u64)> {
        self.template
            .as_ref()
            .map(|template| (&template.request, template.tool_result_tokens))
    }

    pub(crate) fn previous_message_id(&self) -> Option<&str> {
        self.previous_message_id.as_deref()
    }

    pub(crate) fn container(&self) -> Option<&str> {
        self.container.as_deref()
    }

    pub(crate) fn result(&self, id: &str) -> Option<&ContentBlock> {
        let answered = |block: &&ContentBlock| matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id);
        if let Phase::Tools { calls, .. } = &self.phase {
            return calls
                .iter()
                .filter_map(|call| call.result.as_ref())
                .find(answered);
        }
        self.messages
            .iter()
            .rev()
            .filter(|message| message.role == Role::User)
            .find_map(|message| message.content.iter().find(answered))
    }

    pub(crate) fn queued(&self) -> impl Iterator<Item = &Message> {
        self.inputs.iter().map(|queued| &queued.message)
    }

    pub(crate) fn open_call(&self, id: &str) -> Option<OpenCall<'_>> {
        let Phase::Tools {
            model_out, calls, ..
        } = &self.phase
        else {
            return None;
        };
        calls
            .iter()
            .find(|call| call.id == id)
            .map(|call| OpenCall {
                block: BlockRef {
                    model_out: *model_out,
                    block: call.block,
                },
                name: &call.name,
                intended: call.intended,
                output: call.output,
            })
    }

    pub(crate) fn unrendered(&self) -> Vec<String> {
        match &self.phase {
            Phase::Tools { calls, .. } => calls
                .iter()
                .filter(|call| call.result.is_none())
                .map(|call| call.id.clone())
                .collect(),
            Phase::Ready | Phase::Compacting { .. } => Vec::new(),
        }
    }

    fn base(&mut self, seq: u64, base: &Base<&RawValue>) -> Result<(), FoldError> {
        if !self.messages.is_empty() || self.template.is_some() || !self.inputs.is_empty() {
            return Err(rejected(seq, "a Claude base seeds only an empty fold"));
        }
        let legacy: Legacy = decode(seq, "checkpoint", base.checkpoint)?;
        let conversation = match (legacy.snapshot, legacy.conversation) {
            (Some(snapshot), None) => snapshot.conversation,
            (None, Some(conversation)) => conversation,
            (Some(_), Some(_)) => {
                return Err(rejected(seq, "a Claude checkpoint holds two conversations"));
            }
            (None, None) => {
                return Err(rejected(seq, "a Claude checkpoint holds no conversation"));
            }
        };
        let messages = if legacy.pending.is_empty() {
            conversation.messages
        } else {
            legacy.pending
        };
        self.messages = if conversation.summary.is_empty() {
            messages
        } else {
            summarized(&conversation.summary, messages)
        };
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
        let queued = messages.into_iter().map(|message| Queued {
            turn: input.turn.clone(),
            message,
        });
        if !matches!(input.kind, InputKind::Edit { .. }) {
            self.inputs.extend(queued);
            return Ok(());
        }
        let Some(at) = self
            .inputs
            .iter()
            .position(|queued| queued.turn == input.turn)
        else {
            return Err(rejected(seq, "a Claude edit names no queued turn"));
        };
        self.inputs.retain(|queued| queued.turn != input.turn);
        for (offset, queued) in queued.enumerate() {
            self.inputs.insert(at + offset, queued);
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
        let assistant = self.messages.len();
        self.messages.push(Message {
            role: Role::Assistant,
            content,
            output_config: None,
        });
        if !calls.is_empty() {
            self.phase = Phase::Tools {
                model_out: seq,
                assistant,
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
        let kept = self.messages.split_off(start);
        self.messages = summarized(&summary, kept);
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
        let (model_out, call) = self.call(seq, &intent.call_id)?;
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
        let Phase::Tools {
            model_out, calls, ..
        } = &mut self.phase
        else {
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
        if self.nested.remove(&result.call_id) || self.abandoned.remove(&result.call_id) {
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
        if call.output || call.result.is_some() {
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
        match (decision, &self.phase) {
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
            (ClaudeDecision::Splice { from, to, messages }, _) => {
                self.splice(seq, *from, *to, messages)
            }
            (
                ClaudeDecision::Carried {
                    container,
                    previous_message_id,
                },
                _,
            ) => {
                self.container.clone_from(container);
                self.previous_message_id.clone_from(previous_message_id);
                Ok(())
            }
            (ClaudeDecision::Message(message), Phase::Ready) => {
                self.messages.push(message.clone());
                Ok(())
            }
            (ClaudeDecision::Admitted { input }, Phase::Ready) => {
                let input = usize::try_from(*input).expect("u32 fits usize");
                let queued = self
                    .inputs
                    .remove(input)
                    .ok_or_else(|| rejected(seq, "a Claude admission names no queued input"))?;
                self.messages.push(queued.message);
                Ok(())
            }
            (ClaudeDecision::Compacting { retained }, Phase::Ready) => {
                self.phase = Phase::Compacting {
                    retained: *retained,
                };
                Ok(())
            }
            (ClaudeDecision::Rendered(block), Phase::Tools { .. }) => self.rendered(seq, block),
            (
                ClaudeDecision::Message(_)
                | ClaudeDecision::Admitted { .. }
                | ClaudeDecision::Compacting { .. },
                _,
            ) => Err(rejected(
                seq,
                "a Claude decision arrived inside a round that it cannot change",
            )),
            (ClaudeDecision::Rendered(_), _) => Err(rejected(
                seq,
                "a rendered Claude tool result arrived outside a tool round",
            )),
        }
    }

    fn splice(
        &mut self,
        seq: u64,
        from: u32,
        to: u32,
        messages: &[Message],
    ) -> Result<(), FoldError> {
        let from = usize::try_from(from).expect("u32 fits usize");
        let to = usize::try_from(to).expect("u32 fits usize");
        if from > to || to > self.messages.len() {
            return Err(rejected(
                seq,
                "a Claude splice falls outside the conversation",
            ));
        }
        if let Phase::Tools {
            assistant, calls, ..
        } = &self.phase
            && from <= *assistant
        {
            self.abandoned.extend(
                calls
                    .iter()
                    .filter(|call| call.intended && !call.output)
                    .map(|call| call.id.clone()),
            );
            self.phase = Phase::Ready;
        }
        self.messages.splice(from..to, messages.iter().cloned());
        Ok(())
    }

    fn rendered(&mut self, seq: u64, block: &ContentBlock) -> Result<(), FoldError> {
        let ContentBlock::ToolResult { tool_use_id, .. } = block else {
            return Err(rejected(
                seq,
                "a rendered Claude tool result is not a tool_result block",
            ));
        };
        let (_, call) = self.call(seq, tool_use_id)?;
        if call.result.is_some() {
            return Err(rejected(
                seq,
                "a rendered Claude tool result has its result already",
            ));
        }
        let unanswered = call.intended && !call.output;
        call.result = Some(block.clone());
        if unanswered {
            self.abandoned.insert(tool_use_id.clone());
        }
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

pub(crate) fn tool_output(
    content: &ToolResultContent,
    is_error: bool,
    metadata: Option<&Value>,
) -> serde_json::Result<ToolOutputWire> {
    let mut output = match content {
        ToolResultContent::Text(text) => ToolOutput::text(text.clone()),
        ToolResultContent::Blocks(blocks) => ToolOutput::content(
            blocks
                .iter()
                .map(|block| match block["text"].as_str() {
                    Some(text) if block["type"] == "text" => ToolOutputContent::InputText {
                        text: text.to_owned(),
                    },
                    _ => ToolOutputContent::InputText {
                        text: Value::to_string(block),
                    },
                })
                .collect(),
        ),
    };
    output.success = !is_error;
    output.metadata = metadata.map(serde_json::value::to_raw_value).transpose()?;
    output.into_wire()
}
