use std::collections::BTreeSet;

use nanocode_durability_core::{
    Base, BlockRef, Decision, DurableFold, EffectResult, Fact, FoldError, Input, Intent,
    IntentTarget, ModelOut, Seen,
};
use nanocode_tool_host::{ToolOutput, contract::ToolOutputWire};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, value::RawValue};

use crate::client::{
    Prompt, PromptInput, UserInput,
    responses::{ContentItem, ResponseItem},
    session::{TOOL_OUTPUT_TOKEN_LIMIT, folded_item, install_history_with_provenance},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexDecision {
    Template {
        request: Box<RawValue>,
        tool_output_tokens: usize,
    },
    Items(Vec<ResponseItem>),
    ClientItems(Vec<ResponseItem>),
    UserContent(Vec<ContentItem>),
    Rendered {
        call_id: String,
        items: Vec<ResponseItem>,
    },
    Compacted {
        item: ResponseItem,
        initial_context: Vec<ResponseItem>,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct CodexFold {
    template: Option<Template>,
    prefix: Vec<ResponseItem>,
    history: Vec<ResponseItem>,
    client_authored: BTreeSet<String>,
    previous_response_id: Option<String>,
    queued: Vec<ResponseItem>,
    nested: BTreeSet<String>,
    phase: Phase,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Template {
    request: Map<String, Value>,
    tool_output_tokens: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    #[default]
    Ready,
    Prompting {
        turn: String,
        prompt: Prompt,
    },
    Tools {
        model_out: u64,
        calls: Vec<Call>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Function,
    Custom,
    Rendered,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Call {
    id: String,
    source: String,
    kind: Kind,
    block: u32,
    intended: bool,
    output: bool,
    items: Option<Vec<ResponseItem>>,
}

#[derive(Deserialize)]
struct Legacy {
    #[serde(default)]
    conversation: Option<LegacySnapshot>,
    #[serde(flatten)]
    snapshot: Option<LegacySnapshot>,
}

#[derive(Deserialize)]
struct LegacySnapshot {
    history: Vec<ResponseItem>,
    #[serde(default)]
    request_prefix: Option<Vec<ResponseItem>>,
    #[serde(default)]
    client_authored: BTreeSet<String>,
}

#[derive(Deserialize)]
struct Response {
    id: String,
    output: Vec<ResponseItem>,
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
        reason: format!("Codex {what} does not decode: {error}"),
    })
}

impl DurableFold for CodexFold {
    const FAMILY: &'static str = "codex";
    const FORMAT: u32 = 1;
    type Decision = CodexDecision;

    fn apply(&mut self, seq: u64, fact: Seen<'_, CodexDecision>) -> Result<(), FoldError> {
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
        let mut request = self
            .template
            .as_ref()
            .map_or_else(Map::new, |template| template.request.clone());
        let input = self.prefix.iter().chain(&self.history).collect::<Vec<_>>();
        let input =
            serde_json::to_value(input).map_err(|error| FoldError::Encode(error.to_string()))?;
        request.insert("input".into(), input);
        serde_json::to_vec(&request).map_err(|error| FoldError::Encode(error.to_string()))
    }
}

impl CodexFold {
    fn tokens(&self) -> usize {
        self.template
            .as_ref()
            .map_or(TOOL_OUTPUT_TOKEN_LIMIT, |template| {
                template.tool_output_tokens
            })
    }

    fn record(&mut self, items: impl IntoIterator<Item = ResponseItem>, name: &str) {
        let tokens = self.tokens();
        let recorded = items
            .into_iter()
            .enumerate()
            .filter_map(|(index, item)| folded_item(item, tokens, &format!("{name}:{index}")));
        match self.phase {
            Phase::Ready => self.history.extend(recorded),
            Phase::Prompting { .. } | Phase::Tools { .. } => self.queued.extend(recorded),
        }
    }

    fn base(&mut self, seq: u64, base: &Base<&RawValue>) -> Result<(), FoldError> {
        if !self.history.is_empty() || self.template.is_some() {
            return Err(rejected(seq, "a Codex base seeds only an empty fold"));
        }
        let legacy: Legacy = decode(seq, "checkpoint", base.checkpoint)?;
        let snapshot = legacy
            .conversation
            .or(legacy.snapshot)
            .ok_or_else(|| rejected(seq, "a Codex checkpoint holds no conversation"))?;
        self.history = snapshot.history;
        self.prefix = snapshot.request_prefix.unwrap_or_default();
        self.client_authored = snapshot.client_authored;
        Ok(())
    }

    fn input(&mut self, seq: u64, input: &Input<&RawValue>) -> Result<(), FoldError> {
        let prompt: Prompt = decode(seq, "prompt", input.value)?;
        match text_content(&prompt.instruction) {
            Some(content) => {
                self.record(crate::model::prompt_messages(&prompt, content), &input.turn);
                Ok(())
            }
            None if matches!(self.phase, Phase::Ready) => {
                self.phase = Phase::Prompting {
                    turn: input.turn.clone(),
                    prompt,
                };
                Ok(())
            }
            None => Err(rejected(
                seq,
                "a Codex prompt with media arrived inside a round",
            )),
        }
    }

    fn model_out(&mut self, seq: u64, out: &ModelOut<&RawValue>) -> Result<(), FoldError> {
        if !matches!(self.phase, Phase::Ready) {
            return Err(rejected(
                seq,
                "a Codex response arrived before its round settled",
            ));
        }
        let response: Response = decode(seq, "response", out.response)?;
        let calls = response
            .output
            .iter()
            .enumerate()
            .filter_map(|(block, item)| call(block, item))
            .collect::<Vec<_>>();
        self.previous_response_id = Some(response.id);
        self.record(response.output, &format!("{seq}"));
        if !calls.is_empty() {
            self.phase = Phase::Tools {
                model_out: seq,
                calls,
            };
        }
        Ok(())
    }

    fn intent(&mut self, seq: u64, intent: &Intent<&RawValue>) -> Result<(), FoldError> {
        let block = match &intent.target {
            IntentTarget::Nested { .. } => {
                if !self.nested.insert(intent.call_id.clone()) {
                    return Err(rejected(seq, "a nested Codex call is intended twice"));
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
                "a Codex intent names another output item than its call",
            ));
        }
        if call.intended {
            return Err(rejected(seq, "a Codex call is intended twice"));
        }
        call.intended = true;
        Ok(())
    }

    fn call(&mut self, seq: u64, id: &str) -> Result<(u64, &mut Call), FoldError> {
        let Phase::Tools { model_out, calls } = &mut self.phase else {
            return Err(rejected(
                seq,
                "a Codex tool call arrived outside a tool round",
            ));
        };
        let model_out = *model_out;
        calls
            .iter_mut()
            .find(|call| call.id == id)
            .map(|call| (model_out, call))
            .ok_or_else(|| rejected(seq, "a Codex tool call names no call of its round"))
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
        let output =
            ToolOutput::from_wire(wire).map_err(|error| rejected(seq, error.to_string()))?;
        let (_, call) = self.call(seq, &result.call_id)?;
        if !call.intended {
            return Err(rejected(seq, "a Codex tool result has no intent before it"));
        }
        if call.output {
            return Err(rejected(seq, "a Codex tool result arrived twice"));
        }
        call.output = true;
        call.items = match call.kind {
            Kind::Function => Some(vec![crate::model::function_tool_output(
                call.id.clone(),
                output.output,
            )]),
            Kind::Custom => Some(vec![crate::model::custom_tool_output(
                call.id.clone(),
                output.output,
            )]),
            Kind::Rendered => None,
        };
        self.settle();
        Ok(())
    }

    fn settle(&mut self) {
        let Phase::Tools { calls, .. } = &mut self.phase else {
            return;
        };
        let done = calls.iter().take_while(|call| call.items.is_some()).count();
        let settled = calls.drain(..done).collect::<Vec<_>>();
        let finished = calls.is_empty();
        if finished {
            self.phase = Phase::Ready;
        }
        let tokens = self.tokens();
        for call in settled {
            let items = call.items.expect("a settled Codex call has its output");
            self.history
                .extend(items.into_iter().enumerate().filter_map(|(index, item)| {
                    folded_item(item, tokens, &format!("{}:{index}", call.source))
                }));
        }
        if finished {
            self.history.append(&mut self.queued);
        }
    }

    fn decision(&mut self, seq: u64, decision: Decision<&CodexDecision>) -> Result<(), FoldError> {
        match decision {
            Decision::Time { .. } | Decision::Id { .. } | Decision::Host { .. } => Ok(()),
            Decision::Route { model } => {
                let template = self.template.as_mut().ok_or_else(|| {
                    rejected(seq, "a Codex route switch arrived before its template")
                })?;
                template
                    .request
                    .insert("model".into(), Value::String(model));
                Ok(())
            }
            Decision::Backend(decision) => self.backend(seq, decision),
        }
    }

    fn backend(&mut self, seq: u64, decision: &CodexDecision) -> Result<(), FoldError> {
        match decision {
            CodexDecision::Template {
                request,
                tool_output_tokens,
            } => {
                let request: Map<String, Value> = decode(seq, "request template", request)?;
                if request.contains_key("input") {
                    return Err(rejected(seq, "a Codex template carries input"));
                }
                self.template = Some(Template {
                    request,
                    tool_output_tokens: *tool_output_tokens,
                });
                Ok(())
            }
            CodexDecision::Items(items) => {
                self.record(items.iter().cloned(), &format!("{seq}"));
                Ok(())
            }
            CodexDecision::ClientItems(items) => self.client_items(seq, items),
            CodexDecision::UserContent(content) => self.user_content(seq, content),
            CodexDecision::Rendered { call_id, items } => self.rendered(seq, call_id, items),
            CodexDecision::Compacted {
                item,
                initial_context,
            } => self.compacted(seq, item, initial_context),
        }
    }

    fn client_items(&mut self, seq: u64, items: &[ResponseItem]) -> Result<(), FoldError> {
        for item in items {
            let Some(id) = item.id().filter(|id| !id.is_empty()) else {
                return Err(rejected(seq, "a client-authored Codex item carries no id"));
            };
            if matches!(
                item,
                ResponseItem::Message {
                    role: crate::client::MessageRole::Developer,
                    ..
                }
            ) {
                self.client_authored.insert(id.to_string());
            }
        }
        self.record(items.iter().cloned(), &format!("{seq}"));
        Ok(())
    }

    fn user_content(&mut self, seq: u64, content: &[ContentItem]) -> Result<(), FoldError> {
        let Phase::Prompting { turn, prompt } = std::mem::take(&mut self.phase) else {
            return Err(rejected(
                seq,
                "Codex user content arrived without a prompt awaiting it",
            ));
        };
        self.record(
            crate::model::prompt_messages(&prompt, content.to_vec()),
            &turn,
        );
        self.history.append(&mut self.queued);
        Ok(())
    }

    fn rendered(
        &mut self,
        seq: u64,
        call_id: &str,
        items: &[ResponseItem],
    ) -> Result<(), FoldError> {
        let (_, call) = self.call(seq, call_id)?;
        if call.kind != Kind::Rendered || !call.output || call.items.is_some() {
            return Err(rejected(
                seq,
                "a rendered Codex tool output has no unrendered call",
            ));
        }
        call.items = Some(items.to_vec());
        self.settle();
        Ok(())
    }

    fn compacted(
        &mut self,
        seq: u64,
        item: &ResponseItem,
        initial_context: &[ResponseItem],
    ) -> Result<(), FoldError> {
        if !matches!(self.phase, Phase::Ready) {
            return Err(rejected(seq, "a Codex compaction arrived inside a round"));
        }
        self.history = install_history_with_provenance(
            &self.history,
            initial_context,
            item.clone(),
            &self.client_authored,
        );
        self.previous_response_id = None;
        Ok(())
    }
}

fn call(block: usize, item: &ResponseItem) -> Option<Call> {
    let (id, kind) = match item {
        ResponseItem::FunctionCall { call_id, .. } => (call_id.to_string(), Kind::Function),
        ResponseItem::CustomToolCall { call_id, .. } => (call_id.to_string(), Kind::Custom),
        ResponseItem::ToolSearchCall {
            call_id: Some(call_id),
            execution,
            ..
        } if &**execution == "client" => (call_id.to_string(), Kind::Rendered),
        _ => return None,
    };
    Some(Call {
        source: item
            .id()
            .map_or_else(|| id.clone(), |source| source.to_string()),
        id,
        kind,
        block: u32::try_from(block).expect("a Codex response holds fewer items than u32 counts"),
        intended: false,
        output: false,
        items: None,
    })
}

fn text_content(instruction: &PromptInput) -> Option<Vec<ContentItem>> {
    match instruction {
        PromptInput::Text(text) => Some(vec![ContentItem::input_text(text.as_str())]),
        PromptInput::Content(items) => items
            .iter()
            .map(|item| match item {
                UserInput::Text { text } => Some(ContentItem::input_text(text.as_str())),
                _ => None,
            })
            .collect(),
    }
}
