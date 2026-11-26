use std::{collections::BTreeMap, sync::Arc};

use nanocode_scope::Part;
use nanocode_sync::Vector;
use nanocode_turnlog::{ArmedWake, Checkpoint, Event, Hold, OpenCall, Turn};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;

use super::{
    sim::{self, Mutation},
    state::{KeptFact, ThreadState, invalid},
};
use crate::durability::StoreError;

#[derive(Deserialize, Serialize)]
struct Meta {
    generation: u64,
}

fn json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("a resident thread's values encode as JSON")
}

fn parsed<T: DeserializeOwned>(name: &str, body: &str) -> Result<T, StoreError> {
    serde_json::from_str(body).map_err(|error| invalid(format!("part {name}: {error}")))
}

fn chunked<T: Serialize>(
    parts: &mut Vec<Part>,
    kind: &str,
    rows: usize,
    values: impl Iterator<Item = T>,
) {
    let mut chunk = Vec::with_capacity(rows);
    let mut at = 0_usize;
    for value in values {
        chunk.push(value);
        if chunk.len() == rows {
            parts.push(Part {
                name: format!("{kind}/{at}"),
                body: json(&chunk),
            });
            chunk.clear();
            at += 1;
        }
    }
    if !chunk.is_empty() {
        parts.push(Part {
            name: format!("{kind}/{at}"),
            body: json(&chunk),
        });
    }
}

pub(crate) fn encode(state: &ThreadState, rows: usize) -> Vec<Part> {
    let mut parts = vec![Part {
        name: "meta".to_owned(),
        body: json(&Meta {
            generation: state.generation,
        }),
    }];
    assert_eq!(
        state.admitted.len(),
        state.turns.len(),
        "every resident turn is admitted at one event"
    );
    chunked(
        &mut parts,
        "turns",
        rows,
        state
            .admitted
            .values()
            .map(|id| state.turns.get(id).expect("an admitted turn is resident"))
            .map(AsRef::as_ref),
    );
    chunked(&mut parts, "events", rows, state.events.iter().map(AsRef::as_ref));
    chunked(&mut parts, "facts", rows, state.facts.iter().map(AsRef::as_ref));
    for ((session, cell), updates) in state.cells.iter() {
        parts.push(Part {
            name: format!("cell/{cell}/{session}"),
            body: json(updates),
        });
    }
    keyed(&mut parts, "checkpoint", state.checkpoints.iter());
    for (key, value) in state.stored.iter() {
        parts.push(Part {
            name: format!("stored/{key}"),
            body: value.as_ref().to_owned(),
        });
    }
    keyed(&mut parts, "host", state.hosts.iter());
    keyed(&mut parts, "hold", state.holds.iter());
    keyed(&mut parts, "call", state.calls.iter());
    for (turn, responses) in state.models.iter() {
        let mut models = Vec::new();
        chunked(&mut models, "model", rows, responses.iter());
        parts.extend(models.into_iter().map(|part| Part {
            name: format!("{}/{turn}", part.name),
            body: part.body,
        }));
    }
    keyed(&mut parts, "wake", state.wakes.iter());
    if sim::mutated(Mutation::PartDropped) {
        parts.pop();
    }
    parts
}

fn keyed<'a, V: Serialize + 'a>(
    parts: &mut Vec<Part>,
    kind: &str,
    values: impl Iterator<Item = (&'a Arc<str>, &'a V)>,
) {
    for (key, value) in values {
        parts.push(Part {
            name: format!("{kind}/{key}"),
            body: json(value),
        });
    }
}

#[derive(Default)]
struct Pieces {
    events: BTreeMap<u64, Vec<Event>>,
    facts: BTreeMap<u64, Vec<KeptFact>>,
    models: BTreeMap<(Arc<str>, u64), Vec<Box<RawValue>>>,
}

fn index(name: &str, at: &str) -> Result<u64, StoreError> {
    at.parse()
        .map_err(|_| invalid(format!("part {name} names no chunk")))
}

fn ordered<T>(kind: &str, chunks: BTreeMap<u64, Vec<T>>) -> Result<Vec<T>, StoreError> {
    let mut values = Vec::new();
    for (expected, (at, chunk)) in (0_u64..).zip(chunks) {
        if at != expected {
            return Err(invalid(format!("{kind} chunk {expected} is missing")));
        }
        values.extend(chunk);
    }
    Ok(values)
}

pub(crate) fn decode(parts: Vec<Part>) -> Result<ThreadState, StoreError> {
    let mut state = ThreadState::default();
    let mut pieces = Pieces::default();
    let mut meta = None;
    for part in parts {
        state.bytes += part.body.len();
        if part.name == "meta" {
            meta = Some(parsed::<Meta>(&part.name, &part.body)?);
            continue;
        }
        let Some((kind, rest)) = part.name.split_once('/') else {
            return Err(invalid(format!("part {} names no kind", part.name)));
        };
        place(&mut state, &mut pieces, kind, rest, &part)?;
    }
    let Some(meta) = meta else {
        return Err(invalid("the checkpoint has no meta part"));
    };
    state.generation = meta.generation;
    for event in ordered("events", pieces.events)? {
        state.events.push_back(Arc::new(event));
    }
    for fact in ordered("facts", pieces.facts)? {
        state.facts.push_back(Arc::new(fact));
    }
    let mut turns: BTreeMap<Arc<str>, BTreeMap<u64, Vec<Box<RawValue>>>> = BTreeMap::new();
    for ((turn, at), chunk) in pieces.models {
        turns.entry(turn).or_default().insert(at, chunk);
    }
    for (turn, chunks) in turns {
        let responses: Vector<Box<RawValue>> = ordered("model", chunks)?.into_iter().collect();
        state.models.insert(turn, responses);
    }
    for (id, turn) in state.turns.iter() {
        if state.admitted.contains_key(&turn.first) {
            return Err(invalid(format!("two turns start at event {}", turn.first)));
        }
        state.admitted.insert(turn.first, Arc::clone(id));
    }
    Ok(state)
}

fn place(
    state: &mut ThreadState,
    pieces: &mut Pieces,
    kind: &str,
    rest: &str,
    part: &Part,
) -> Result<(), StoreError> {
    let name = part.name.as_str();
    let body = part.body.as_str();
    let key: Arc<str> = Arc::from(rest);
    match kind {
        "turn" => {
            let turn: Turn = parsed(name, body)?;
            if turn.id.as_str() != rest {
                return Err(invalid(format!("part {name} holds turn {}", turn.id)));
            }
            state.turns.insert(key, Arc::new(turn));
        }
        "turns" => {
            index(name, rest)?;
            for turn in parsed::<Vec<Turn>>(name, body)? {
                let id: Arc<str> = Arc::from(turn.id.as_str());
                if state.turns.contains_key(&id) {
                    return Err(invalid(format!("part {name} holds turn {id} twice")));
                }
                state.turns.insert(id, Arc::new(turn));
            }
        }
        "events" => {
            pieces.events.insert(index(name, rest)?, parsed(name, body)?);
        }
        "facts" => {
            pieces.facts.insert(index(name, rest)?, parsed(name, body)?);
        }
        "cell" => {
            let Some((cell, session)) = rest.split_once('/') else {
                return Err(invalid(format!("part {name} names no session")));
            };
            let updates: Vector<Box<RawValue>> = parsed(name, body)?;
            state
                .cells
                .insert((Arc::from(session), index(name, cell)?), updates);
        }
        "model" => {
            let Some((at, turn)) = rest.split_once('/') else {
                return Err(invalid(format!("part {name} names no turn")));
            };
            pieces
                .models
                .insert((Arc::from(turn), index(name, at)?), parsed(name, body)?);
        }
        "stored" => {
            state.stored.insert(key, Arc::from(body));
        }
        kind => keep(state, kind, key, name, body)?,
    }
    Ok(())
}

fn keep(
    state: &mut ThreadState,
    kind: &str,
    key: Arc<str>,
    name: &str,
    body: &str,
) -> Result<(), StoreError> {
    match kind {
        "checkpoint" => {
            state
                .checkpoints
                .insert(key, Arc::new(parsed::<Checkpoint>(name, body)?));
        }
        "host" => {
            state.hosts.insert(key, parsed(name, body)?);
        }
        "hold" => {
            state.holds.insert(key, Arc::new(parsed::<Hold>(name, body)?));
        }
        "call" => {
            state
                .calls
                .insert(key, Arc::new(parsed::<OpenCall>(name, body)?));
        }
        "wake" => {
            state
                .wakes
                .insert(key, Arc::new(parsed::<ArmedWake>(name, body)?));
        }
        kind => return Err(invalid(format!("part {name} has unknown kind {kind}"))),
    }
    Ok(())
}
