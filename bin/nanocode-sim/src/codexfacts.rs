use nanocode_backend_codex::durability::CodexFold;
use nanocode_durability_core::{Decision, Digest, Fact, FoldError, Folded, Marker, StoredFact};
use serde_json::Value;

const RESPONSE_PREFIX: &str = "resp_carry";
const FUNCTIONS_NAMESPACE: &str = "functions";
const AUTHORED: [&str; 3] = ["{\"items\"", "{\"splice\"", "{\"prefix\""];

pub struct Checked {
    pub calls: u64,
    pub matched: u64,
}

fn flattened(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .flat_map(|tool| match tool.get("tools").and_then(Value::as_array) {
            Some(children)
                if tool["type"] == "namespace" && tool["name"] == FUNCTIONS_NAMESPACE =>
            {
                children.clone()
            }
            _ => vec![tool.clone()],
        })
        .collect()
}

fn input(value: &Value) -> Value {
    let mut input = value.get("input").cloned().unwrap_or(Value::Null);
    for item in input.as_array_mut().into_iter().flatten() {
        if item["type"] == "additional_tools"
            && let Some(tools) = item.get("tools").and_then(Value::as_array)
        {
            item["tools"] = Value::Array(flattened(tools));
        }
    }
    input
}

pub async fn check(
    log: &std::sync::Arc<nanocode::turnlog::OneLog>,
    state: &str,
    bodies: &[(String, bool)],
    drop_decision: bool,
) -> Result<Checked, String> {
    let store = log.store();
    let logs = nanocode_durability_core::StateStore::facts(&store)
        .ok_or("a log store serves no fact logs")?;
    let replay = logs
        .scoped(state, true)
        .replay()
        .await
        .map_err(|error| format!("codex_facts_unreadable: {error}"))?;
    let mut facts: Vec<(u64, StoredFact)> = replay.facts;
    if drop_decision
        && let Some(at) = facts
            .iter()
            .position(|(_, fact)| matches!(fact, Fact::Decision(Decision::Backend(decision)) if AUTHORED.iter().any(|kind| decision.get().starts_with(kind))))
    {
        facts.remove(at);
    }
    let digests = facts
        .iter()
        .filter_map(|(_, fact)| match fact {
            Fact::Marker(Marker::Blob { digest, .. }) => Some(*digest),
            _ => None,
        })
        .collect::<Vec<Digest>>();
    let blobs = &replay.blobs;
    let lookup = |digest: &Digest| {
        blobs
            .get(digest)
            .map(|json| &**json)
            .ok_or(FoldError::MissingBlob { digest: *digest })
    };
    let mut checked = Checked {
        calls: 0,
        matched: 0,
    };
    for at in 0..facts.len() {
        let Fact::ModelOut(out) = &facts[at].1 else {
            continue;
        };
        let response = crate::claudefacts::json(&out.response, &digests, blobs)
            .ok_or("codex_facts_blob_missing: a model output names a blob the log lacks")?;
        let response: Value = serde_json::from_str(response.get())
            .map_err(|error| format!("codex_facts_response_unreadable: {error}"))?;
        let Some(number) = response["id"]
            .as_str()
            .and_then(|id| id.strip_prefix(RESPONSE_PREFIX))
            .and_then(|number| number.parse::<usize>().ok())
        else {
            return Err(format!("codex_facts_response_unknown: {}", response["id"]));
        };
        let Some((body, true)) = number.checked_sub(1).and_then(|index| bodies.get(index)) else {
            return Err(format!(
                "codex_facts_request_missing: no answered request produced response {number}"
            ));
        };
        let sent: Value = serde_json::from_str(body)
            .map_err(|error| format!("codex_facts_request_unreadable: {error}"))?;
        let (folded, _) =
            Folded::<CodexFold>::open(None, facts[..at].to_vec(), lookup).map_err(|error| {
                format!(
                    "codex_facts_refold_failed before fact {}: {error}",
                    facts[at].0
                )
            })?;
        let request = folded
            .request()
            .ok()
            .and_then(|request| serde_json::from_slice::<Value>(&request).ok())
            .ok_or_else(|| {
                format!(
                    "codex_facts_request_unencodable before fact {}",
                    facts[at].0
                )
            })?;
        checked.calls += 1;
        let (folded, received) = (input(&request), input(&sent));
        if folded != received {
            return Err(format!(
                "codex_facts_request_differs: the fold of the log before response {number} (fact {}) builds input {folded} where the model received {received}",
                facts[at].0
            ));
        }
        checked.matched += 1;
    }
    Ok(checked)
}
