use super::*;
use crate::durability::{CodexDecision, CodexFacts};
use nanocode_durability::facts::{
    Decision, EffectResult, Fact, Intent, IntentTarget, ModelOut, OwnedFact,
};
use nanocode_tool_host::contract::ToolOutputWire;

fn raw(value: &impl Serialize) -> Result<Box<RawValue>> {
    serde_json::value::to_raw_value(value).map_err(|error| {
        NanocodeError::InvalidExecutionPolicy(format!("a Codex fact does not encode: {error}"))
    })
}

const fn decided(decision: CodexDecision) -> OwnedFact<CodexDecision> {
    Fact::Decision(Decision::Backend(decision))
}

fn same(left: &ResponseItem, right: &ResponseItem) -> bool {
    let encoded = |item: &ResponseItem| {
        serde_json::to_vec(item).expect("a Codex response item encodes as JSON")
    };
    encoded(left) == encoded(right)
}

fn same_all(left: &[ResponseItem], right: &[ResponseItem]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| same(left, right))
}

fn index(at: usize) -> Result<u32> {
    u32::try_from(at).map_err(|_| {
        NanocodeError::InvalidExecutionPolicy("a Codex history outgrows u32 items".into())
    })
}

struct Synced {
    template: bool,
    prefix: bool,
    from: usize,
    to: usize,
    kept: usize,
    ready: bool,
}

pub(super) struct FactRequest<'a> {
    pub(super) template: serde_json::Map<String, Value>,
    pub(super) tool_output_tokens: usize,
    pub(super) prefix: &'a [ResponseItem],
    pub(super) sent: &'a [&'a ResponseItem],
}

pub(super) async fn sync_facts(facts: &CodexFacts, request: FactRequest<'_>) -> Result<()> {
    let FactRequest {
        template,
        tool_output_tokens,
        prefix,
        sent,
    } = request;
    let synced = facts
        .read(|fold| {
            let history = fold.history();
            let from = history
                .iter()
                .zip(sent)
                .take_while(|(left, right)| same(left, right))
                .count();
            let kept = history[from..]
                .iter()
                .rev()
                .zip(sent[from..].iter().rev())
                .take_while(|(left, right)| same(left, right))
                .count();
            Synced {
                template: fold.template() != Some((&template, tool_output_tokens)),
                prefix: !same_all(fold.prefix(), prefix),
                from,
                to: history.len(),
                kept,
                ready: fold.ready(),
            }
        })
        .await?;
    if synced.template {
        let decision = CodexDecision::Template {
            request: raw(&template)?,
            tool_output_tokens,
        };
        facts.commit(decided(decision)).await?;
    }
    if synced.prefix {
        facts
            .commit(decided(CodexDecision::Prefix(prefix.to_vec())))
            .await?;
    }
    if synced.from == synced.to && synced.ready {
        if sent.len() > synced.from {
            let items = sent[synced.from..]
                .iter()
                .map(|item| (*item).clone())
                .collect();
            facts.commit(decided(CodexDecision::Items(items))).await?;
        }
        return Ok(());
    }
    let decision = CodexDecision::Splice {
        from: index(synced.from)?,
        to: index(synced.to - synced.kept)?,
        items: sent[synced.from..sent.len() - synced.kept]
            .iter()
            .map(|item| (*item).clone())
            .collect(),
    };
    facts.commit(decided(decision)).await.map(drop)
}

pub(super) async fn fact_model(
    facts: &CodexFacts,
    turn: &str,
    id: &str,
    output: &[ResponseItem],
) -> Result<()> {
    if facts
        .read(|fold| fold.previous_response_id() == Some(id))
        .await?
    {
        return Ok(());
    }
    let fact = Fact::ModelOut(ModelOut {
        turn: turn.to_owned(),
        response: raw(&serde_json::json!({"id": id, "output": output}))?,
    });
    facts.commit(fact).await.map(drop)
}

pub(super) async fn fact_intent(steps: &ExecutionSteps, call_id: &str, tool: &str) -> Result<()> {
    let Some(facts) = steps.facts() else {
        return Ok(());
    };
    let open = facts
        .read(|fold| {
            fold.open_call(call_id)
                .filter(|call| !call.intended)
                .map(|call| call.block)
        })
        .await?;
    let Some(block) = open else {
        return Ok(());
    };
    let fact = Fact::Intent(Intent {
        turn: Some(steps.operation_id().to_owned()),
        call_id: call_id.to_owned(),
        tool: tool.to_owned(),
        target: IntentTarget::ToolUse(block),
    });
    facts.commit(fact).await.map(drop)
}

pub(super) async fn fact_result(
    steps: &ExecutionSteps,
    completed: &CompletedToolCall,
) -> Result<()> {
    let Some(facts) = steps.facts() else {
        return Ok(());
    };
    let open = facts
        .read(|fold| {
            fold.open_call(&completed.call_id)
                .is_some_and(|call| call.intended && !call.output)
        })
        .await?;
    if !open {
        return Ok(());
    }
    let wire = ToolOutputWire {
        output: completed.output.clone(),
        success: completed.success,
        structured_result: None,
        metadata: completed.metadata.clone(),
        process_trace: None,
    };
    let fact = Fact::EffectResult(EffectResult {
        call_id: completed.call_id.clone(),
        output: raw(&wire)?,
    });
    facts.commit(fact).await.map(drop)
}

pub(super) async fn settled_output(
    steps: &ExecutionSteps,
    call_id: &str,
) -> Result<Option<ResponseItem>> {
    let Some(facts) = steps.facts() else {
        return Ok(None);
    };
    facts
        .read(|fold| fold.settled_output(call_id).cloned())
        .await
}

pub(super) fn named_outputs(
    call_id: &str,
    tokens: usize,
    items: Vec<ResponseItem>,
) -> Vec<ResponseItem> {
    items
        .into_iter()
        .enumerate()
        .filter_map(|(at, item)| {
            crate::client::session::folded_item(item, tokens, &format!("{call_id}:{at}"))
        })
        .collect()
}
