use std::collections::BTreeMap;

use nanocode::claude::durability::ClaudeFold;
use nanocode_durability_core::{
    Decision, Digest, Fact, FactLog, FoldError, Folded, StoredFact, Value as FactValue,
};
use serde_json::{Value, value::RawValue};

use crate::{httpd::WireMutation, world::World};

const WIRE_ENVELOPE: [&str; 3] = ["stream", "metadata", "effort_updates"];

pub fn normalized(mut value: Value) -> Value {
    if let Value::Object(fields) = &mut value {
        for field in WIRE_ENVELOPE {
            fields.remove(field);
        }
    }
    let mut stack = vec![&mut value];
    while let Some(value) = stack.pop() {
        match value {
            Value::Object(fields) => {
                fields.remove("cache_control");
                stack.extend(fields.values_mut());
            }
            Value::Array(items) => stack.extend(items.iter_mut()),
            _ => {}
        }
    }
    value
}

fn json<'a>(
    value: &'a FactValue,
    digests: &[Digest],
    blobs: &'a BTreeMap<Digest, Box<RawValue>>,
) -> Option<&'a RawValue> {
    match value {
        FactValue::Inline(json) => Some(json),
        FactValue::Blob(blob) => {
            let digest = digests.get(usize::try_from(blob.index()).ok()?)?;
            blobs.get(digest).map(|json| &**json)
        }
    }
}

fn authored(fact: &StoredFact) -> bool {
    matches!(fact, Fact::Decision(Decision::Backend(decision))
        if decision.get() == "\"admitted\""
            || decision.get().starts_with("{\"message\"")
            || decision.get().starts_with("{\"splice\""))
}

pub struct Checked {
    pub calls: u64,
    pub matched: u64,
    pub kinds: BTreeMap<String, (u64, u64)>,
}

fn kind(fact: &StoredFact) -> String {
    match fact {
        Fact::Decision(Decision::Backend(decision)) => {
            let text = decision.get();
            let end = text[2..].find('"').map_or(text.len(), |end| end + 2);
            format!(
                "decision:{}",
                text.get(..end).unwrap_or(text).trim_matches(['{', '"'])
            )
        }
        Fact::Decision(_) => "decision:runtime".to_owned(),
        Fact::Base(_) => "base".to_owned(),
        Fact::Input(_) => "input".to_owned(),
        Fact::ModelOut(_) => "model_out".to_owned(),
        Fact::Intent(_) => "intent".to_owned(),
        Fact::EffectResult(_) => "effect_result".to_owned(),
        Fact::Marker(_) => "marker".to_owned(),
    }
}

pub async fn check(
    log: &std::sync::Arc<nanocode::turnlog::OneLog>,
    world: &World,
    mutation: WireMutation,
) -> Checked {
    let mut checked = Checked {
        calls: 0,
        matched: 0,
        kinds: BTreeMap::new(),
    };
    let states = log
        .read(|fold| {
            fold.states()
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .await;
    let store = log.store();
    let Some(logs) = nanocode_durability_core::StateStore::facts(&store) else {
        return checked;
    };
    for state in states {
        let scoped = logs.scoped(&state, state.starts_with("sim-root-"));
        check_state(&*scoped, world, mutation, &mut checked).await;
    }
    checked
}

async fn check_state(
    log: &dyn FactLog,
    world: &World,
    mutation: WireMutation,
    checked: &mut Checked,
) {
    let replay = match log.replay().await {
        Ok(replay) => replay,
        Err(error) => {
            world.wrong(format!("claude_facts_unreadable: {error}"));
            return;
        }
    };
    let mut facts = replay.facts;
    for (_, fact) in &facts {
        let bytes = serde_json::to_string(fact).map_or(0, |json| json.len() as u64);
        let entry = checked.kinds.entry(kind(fact)).or_default();
        entry.0 += 1;
        entry.1 += bytes;
    }
    if mutation == WireMutation::DropFactDecision
        && let Some(at) = facts.iter().position(|(_, fact)| authored(fact))
    {
        facts.remove(at);
    }
    let digests = facts
        .iter()
        .filter_map(|(_, fact)| match fact {
            Fact::Marker(nanocode_durability_core::Marker::Blob { digest, .. }) => Some(*digest),
            _ => None,
        })
        .collect::<Vec<_>>();
    let blobs = &replay.blobs;
    let lookup = |digest: &Digest| {
        blobs
            .get(digest)
            .map(|json| &**json)
            .ok_or(FoldError::MissingBlob { digest: *digest })
    };
    for at in 0..facts.len() {
        let Fact::ModelOut(out) = &facts[at].1 else {
            continue;
        };
        if at > 0
            && matches!(&facts[at - 1].1, Fact::Decision(Decision::Backend(decision)) if decision.get().starts_with("{\"compacting\""))
        {
            continue;
        }
        let Some(response) = json(&out.response, &digests, blobs) else {
            world.wrong(
                "claude_facts_blob_missing: a model output names a blob the log lacks".to_owned(),
            );
            continue;
        };
        let Some(id) = serde_json::from_str::<Value>(response.get())
            .ok()
            .and_then(|response| response["id"].as_str().map(str::to_owned))
        else {
            continue;
        };
        let Some(sent) = world.request_for(&id) else {
            continue;
        };
        checked.calls += 1;
        let folded = match Folded::<ClaudeFold>::open(None, facts[..at].to_vec(), lookup) {
            Ok((folded, _)) => folded,
            Err(error) => {
                world.wrong(format!(
                    "claude_facts_refold_failed before fact {}: {error}",
                    facts[at].0
                ));
                continue;
            }
        };
        let request = folded
            .request()
            .ok()
            .and_then(|request| serde_json::from_slice::<Value>(&request).ok())
            .map(normalized);
        let Some(request) = request else {
            world.wrong(format!(
                "claude_facts_request_unencodable before fact {}",
                facts[at].0
            ));
            continue;
        };
        if request == sent {
            checked.matched += 1;
            continue;
        }
        let fields = request
            .as_object()
            .into_iter()
            .chain(sent.as_object())
            .flat_map(serde_json::Map::keys)
            .filter(|field| request.get(field) != sent.get(field))
            .collect::<std::collections::BTreeSet<_>>();
        world.wrong(format!(
            "claude_facts_request_differs: the fold of the log before model output {id} (fact {}) builds a request whose {fields:?} differ from what the model received: {} where the model received {}",
            facts[at].0, request, sent
        ));
    }
}
