use nanocode_backend_antigravity_client::{Content, FunctionCall, Part};
use nanocode_durability::facts::{
    Base, BlockRef, Decision, DurableFold, EffectResult, Fact, FoldError, Intent, IntentTarget,
    ModelOut, Seen,
};
use nanocode_tool_host::{ToolOutput, contract::ToolOutputWire};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AntigravityDecision {
    Contents(Vec<Content>),
    Splice {
        from: u32,
        to: u32,
        contents: Vec<Content>,
    },
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct AntigravityFold {
    contents: Vec<Content>,
    answered: Option<(String, u32)>,
    nested: BTreeSet<String>,
    abandoned: BTreeSet<String>,
    settled: Vec<(String, Part)>,
    phase: Phase,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    #[default]
    Ready,
    Tools {
        model_out: u64,
        calls: Vec<Call>,
    },
}

#[derive(Clone, Deserialize, Serialize)]
struct Call {
    id: String,
    call: FunctionCall,
    block: u32,
    intended: bool,
    response: Option<Part>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Reply {
    pub(crate) call: u32,
    pub(crate) parts: Vec<Part>,
    pub(crate) ids: Vec<String>,
}

#[derive(Deserialize)]
struct Checkpoint {
    #[serde(default)]
    contents: Vec<Content>,
}

pub(crate) struct OpenCall {
    pub(crate) block: BlockRef,
    pub(crate) intended: bool,
    pub(crate) answered: bool,
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
        reason: format!("antigravity {what} does not decode: {error}"),
    })
}

pub(crate) fn answer(output: ToolOutput) -> (&'static str, String) {
    let text = crate::nanotools::body_text(&output.output);
    match output.success {
        true => ("output", text),
        false => ("error", text),
    }
}

impl DurableFold for AntigravityFold {
    const FAMILY: &'static str = crate::harness::FAMILY;
    const FORMAT: u32 = 1;
    type Decision = AntigravityDecision;

    fn apply(&mut self, seq: u64, fact: Seen<'_, AntigravityDecision>) -> Result<(), FoldError> {
        match fact {
            Fact::Base(base) => self.base(seq, &base),
            Fact::ModelOut(out) => self.model_out(seq, &out),
            Fact::Intent(intent) => self.intent(seq, &intent),
            Fact::EffectResult(result) => self.effect_result(seq, &result),
            Fact::Decision(Decision::Backend(decision)) => self.backend(seq, decision),
            Fact::Decision(_) | Fact::Input(_) | Fact::Marker(_) => Ok(()),
        }
    }

    fn request(&self) -> Result<Vec<u8>, FoldError> {
        serde_json::to_vec(&self.contents).map_err(|error| FoldError::Encode(error.to_string()))
    }
}

impl AntigravityFold {
    pub(crate) fn contents(&self) -> &[Content] {
        &self.contents
    }

    pub(crate) const fn ready(&self) -> bool {
        matches!(self.phase, Phase::Ready)
    }

    pub(crate) fn answered(&self, turn: &str, call: u32) -> bool {
        self.answered
            .as_ref()
            .is_some_and(|(answered, index)| answered == turn && *index == call)
    }

    pub(crate) fn open_call(&self, id: &str) -> Option<OpenCall> {
        let Phase::Tools { model_out, calls } = &self.phase else {
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
                intended: call.intended,
                answered: call.response.is_some(),
            })
    }

    pub(crate) fn settled(&self, id: &str) -> Option<&Part> {
        if let Phase::Tools { calls, .. } = &self.phase {
            return calls
                .iter()
                .find(|call| call.id == id)
                .and_then(|call| call.response.as_ref());
        }
        self.settled
            .iter()
            .find(|(settled, _)| settled == id)
            .map(|(_, part)| part)
    }

    fn base(&mut self, seq: u64, base: &Base<&RawValue>) -> Result<(), FoldError> {
        if !self.contents.is_empty() || !self.ready() {
            return Err(rejected(
                seq,
                "an antigravity base seeds only an empty fold",
            ));
        }
        let checkpoint: Checkpoint = decode(seq, "checkpoint", base.checkpoint)?;
        self.contents = checkpoint.contents;
        Ok(())
    }

    fn model_out(&mut self, seq: u64, out: &ModelOut<&RawValue>) -> Result<(), FoldError> {
        if !self.ready() {
            return Err(rejected(
                seq,
                "an antigravity reply arrived before its round settled",
            ));
        }
        let reply: Reply = decode(seq, "reply", out.response)?;
        let found = reply
            .parts
            .iter()
            .enumerate()
            .filter_map(|(block, part)| Some((block, part.function_call.clone()?)))
            .collect::<Vec<_>>();
        if found.len() != reply.ids.len() {
            return Err(rejected(
                seq,
                "an antigravity reply names another number of call ids than it has calls",
            ));
        }
        let mut calls = Vec::with_capacity(found.len());
        for ((block, call), id) in found.into_iter().zip(reply.ids) {
            let block = u32::try_from(block).map_err(|_| {
                rejected(seq, "an antigravity reply holds more parts than u32 counts")
            })?;
            calls.push(Call {
                id,
                call,
                block,
                intended: false,
                response: None,
            });
        }
        self.contents.push(Content::model(reply.parts));
        self.answered = Some((out.turn.clone(), reply.call));
        self.settled.clear();
        if !calls.is_empty() {
            self.phase = Phase::Tools {
                model_out: seq,
                calls,
            };
        }
        Ok(())
    }

    fn call(&mut self, seq: u64, id: &str) -> Result<(u64, &mut Call), FoldError> {
        let Phase::Tools { model_out, calls } = &mut self.phase else {
            return Err(rejected(
                seq,
                "an antigravity tool call arrived outside a tool round",
            ));
        };
        let model_out = *model_out;
        calls
            .iter_mut()
            .find(|call| call.id == id)
            .map(|call| (model_out, call))
            .ok_or_else(|| rejected(seq, "an antigravity tool call names no call of its round"))
    }

    fn intent(&mut self, seq: u64, intent: &Intent<&RawValue>) -> Result<(), FoldError> {
        let block = match &intent.target {
            IntentTarget::Nested { .. } => {
                if !self.nested.insert(intent.call_id.clone()) {
                    return Err(rejected(seq, "a nested antigravity call is intended twice"));
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
                "an antigravity intent names another part than its call",
            ));
        }
        if call.call.name != intent.tool {
            return Err(rejected(
                seq,
                "an antigravity intent names another tool than its call",
            ));
        }
        if call.intended {
            return Err(rejected(seq, "an antigravity call is intended twice"));
        }
        call.intended = true;
        Ok(())
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
        let output =
            ToolOutput::from_wire(wire).map_err(|error| rejected(seq, error.to_string()))?;
        let (_, call) = self.call(seq, &result.call_id)?;
        if !call.intended {
            return Err(rejected(
                seq,
                "an antigravity tool result has no intent before it",
            ));
        }
        if call.response.is_some() {
            return Err(rejected(seq, "an antigravity tool result arrived twice"));
        }
        let (key, body) = answer(output);
        call.response = Some(crate::driver::response(&call.call, key, body));
        self.settle();
        Ok(())
    }

    fn settle(&mut self) {
        let Phase::Tools { calls, .. } = &self.phase else {
            return;
        };
        if calls.iter().any(|call| call.response.is_none()) {
            return;
        }
        let Phase::Tools { calls, .. } = std::mem::take(&mut self.phase) else {
            return;
        };
        self.settled = calls
            .into_iter()
            .map(|call| {
                let response = call
                    .response
                    .expect("a settled antigravity call has its response");
                (call.id, response)
            })
            .collect();
        self.contents.push(Content::user(
            self.settled.iter().map(|(_, part)| part.clone()).collect(),
        ));
    }

    fn backend(&mut self, seq: u64, decision: &AntigravityDecision) -> Result<(), FoldError> {
        match decision {
            AntigravityDecision::Contents(contents) => {
                if !self.ready() {
                    return Err(rejected(
                        seq,
                        "antigravity contents arrived inside a tool round",
                    ));
                }
                self.contents.extend(contents.iter().cloned());
                Ok(())
            }
            AntigravityDecision::Splice { from, to, contents } => {
                self.splice(seq, *from, *to, contents)
            }
        }
    }

    fn splice(
        &mut self,
        seq: u64,
        from: u32,
        to: u32,
        contents: &[Content],
    ) -> Result<(), FoldError> {
        let from = usize::try_from(from).expect("u32 fits usize");
        let to = usize::try_from(to).expect("u32 fits usize");
        if from > to || to > self.contents.len() {
            return Err(rejected(
                seq,
                "an antigravity splice falls outside the contents",
            ));
        }
        if let Phase::Tools { calls, .. } = std::mem::take(&mut self.phase) {
            self.abandoned.extend(
                calls
                    .into_iter()
                    .filter(|call| call.intended && call.response.is_none())
                    .map(|call| call.id),
            );
        }
        self.settled.clear();
        self.contents.splice(from..to, contents.iter().cloned());
        Ok(())
    }
}
