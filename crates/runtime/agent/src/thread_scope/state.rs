use std::sync::Arc;

use nanocode_sync::{OrdMap, Vector};
use nanocode_turnlog::{
    ArmedWake, Checkpoint, EffectClass, Event, Hold, OpenCall, Outcome, Phase, Record, Recovered,
    Retry, Status, Turn, apply_patch, input_digest, internal_host,
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::durability::{
    StoreError,
    facts::{Decision, Fact, Input, InputKind, Marker, StoredFact, TurnStatus, Value},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct KeptFact {
    pub fact: StoredFact,
    pub state: Option<String>,
}

#[derive(Clone, Default)]
pub struct ThreadState {
    pub(crate) turns: OrdMap<Arc<str>, Arc<Turn>>,
    pub(crate) admitted: OrdMap<u64, Arc<str>>,
    pub(crate) events: Vector<Arc<Event>>,
    pub(crate) facts: Vector<Arc<KeptFact>>,
    pub(crate) cells: OrdMap<(Arc<str>, u64), Vector<Box<RawValue>>>,
    pub(crate) checkpoints: OrdMap<Arc<str>, Arc<Checkpoint>>,
    pub(crate) stored: OrdMap<Arc<str>, Arc<str>>,
    pub(crate) hosts: OrdMap<Arc<str>, Box<RawValue>>,
    pub(crate) holds: OrdMap<Arc<str>, Arc<Hold>>,
    pub(crate) calls: OrdMap<Arc<str>, Arc<OpenCall>>,
    pub(crate) models: OrdMap<Arc<str>, Vector<Box<RawValue>>>,
    pub(crate) wakes: OrdMap<Arc<str>, Arc<ArmedWake>>,
    pub(crate) generation: u64,
    pub(crate) bytes: usize,
}

#[derive(serde::Deserialize)]
struct CellHeader {
    origin_call_id: String,
}

#[derive(serde::Deserialize)]
struct CellOrigin {
    header: Option<CellHeader>,
}

pub(crate) fn invalid(detail: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(format!("thread scope row is invalid: {detail}"))
}

impl ThreadState {
    #[must_use]
    pub fn turn(&self, id: &str) -> Option<&Turn> {
        self.turns.get(id).map(AsRef::as_ref)
    }

    pub fn turns(&self) -> impl Iterator<Item = &Turn> {
        self.admitted
            .values()
            .filter_map(|id| self.turns.get(id.as_ref()).map(AsRef::as_ref))
    }

    pub fn pending(&self) -> impl Iterator<Item = &Turn> {
        self.turns()
            .filter(|turn| !matches!(turn.phase, Phase::Finished { .. }))
    }

    pub fn events(&self) -> impl Iterator<Item = &Event> {
        self.events.iter().map(AsRef::as_ref)
    }

    pub fn events_after(&self, after: u64) -> impl Iterator<Item = &Event> {
        self.events().filter(move |event| event.seq > after)
    }

    #[must_use]
    pub fn next_event(&self) -> u64 {
        self.events.last().map_or(1, |event| event.seq + 1)
    }

    #[must_use]
    pub const fn next_fact(&self) -> u64 {
        self.facts.len() as u64 + 1
    }

    pub fn facts(&self) -> impl Iterator<Item = (u64, &KeptFact)> {
        (1..).zip(self.facts.iter().map(AsRef::as_ref))
    }

    pub fn cells(&self) -> impl Iterator<Item = (&str, u64, &Vector<Box<RawValue>>)> {
        self.cells
            .iter()
            .map(|((session, cell), updates)| (session.as_ref(), *cell, updates))
    }

    pub fn cells_of<'a>(
        &'a self,
        session: &'a str,
    ) -> impl Iterator<Item = (u64, &'a Vector<Box<RawValue>>)> + 'a {
        self.cells
            .iter()
            .filter(move |((owner, _), _)| owner.as_ref() == session)
            .map(|((_, cell), updates)| (*cell, updates))
    }

    #[must_use]
    pub fn checkpoint(&self, state: &str) -> Option<&Checkpoint> {
        self.checkpoints.get(state).map(AsRef::as_ref)
    }

    #[must_use]
    pub fn stored(&self, key: &str) -> Option<&str> {
        self.stored.get(key).map(AsRef::as_ref)
    }

    #[must_use]
    pub fn host(&self, key: &str) -> Option<&RawValue> {
        self.hosts.get(key).map(AsRef::as_ref)
    }

    #[must_use]
    pub fn hold(&self, key: &str) -> Option<&Hold> {
        self.holds.get(key).map(AsRef::as_ref)
    }

    pub fn holds(&self) -> impl Iterator<Item = (&str, &Hold)> {
        self.holds
            .iter()
            .map(|(key, hold)| (key.as_ref(), hold.as_ref()))
    }

    pub fn open_calls(&self) -> impl Iterator<Item = (&str, &OpenCall)> {
        self.calls
            .iter()
            .map(|(id, call)| (id.as_ref(), call.as_ref()))
    }

    #[must_use]
    pub fn models(&self, turn: &str) -> Option<&Vector<Box<RawValue>>> {
        self.models.get(turn)
    }

    pub fn wakes(&self) -> impl Iterator<Item = (&str, &ArmedWake)> {
        self.wakes
            .iter()
            .map(|(key, wake)| (key.as_ref(), wake.as_ref()))
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn finished(&self) -> usize {
        self.turns
            .values()
            .filter(|turn| matches!(turn.phase, Phase::Finished { .. }))
            .count()
    }

    #[must_use]
    pub fn digest(&self) -> String {
        let mut text = String::new();
        let mut put = |kind: &str, value: &dyn erased::Encode| {
            text.push_str(kind);
            text.push(' ');
            text.push_str(&value.json());
            text.push('\n');
        };
        put("generation", &self.generation);
        for (id, turn) in self.turns.iter() {
            put("turn", &(id.as_ref(), turn.as_ref()));
        }
        for (first, id) in self.admitted.iter() {
            put("admitted", &(first, id.as_ref()));
        }
        for event in self.events.iter() {
            put("event", event.as_ref());
        }
        for fact in self.facts.iter() {
            put("fact", fact.as_ref());
        }
        for ((session, cell), updates) in self.cells.iter() {
            put("cell", &(session.as_ref(), cell, updates));
        }
        for (state, checkpoint) in self.checkpoints.iter() {
            put("checkpoint", &(state.as_ref(), checkpoint.as_ref()));
        }
        for (key, value) in self.stored.iter() {
            put("stored", &(key.as_ref(), value.as_ref()));
        }
        for (key, value) in self.hosts.iter() {
            put("host", &(key.as_ref(), value));
        }
        for (key, hold) in self.holds.iter() {
            put("hold", &(key.as_ref(), hold.as_ref()));
        }
        for (id, call) in self.calls.iter() {
            put("call", &(id.as_ref(), call.as_ref()));
        }
        for (turn, responses) in self.models.iter() {
            put("model", &(turn.as_ref(), responses));
        }
        for (key, wake) in self.wakes.iter() {
            put("wake", &(key.as_ref(), wake.as_ref()));
        }
        nanocode_digest::blake3(text.as_bytes())
            .to_hex()
            .as_str()
            .to_owned()
    }

    pub fn load(&mut self, seq: u64, body: &str) -> Result<(), StoreError> {
        for record in nanocode_turnlog::decode(body)? {
            self.take(&record, Some(seq))?;
        }
        self.bytes += body.len();
        Ok(())
    }

    pub fn take(&mut self, record: &Record, row: Option<u64>) -> Result<(), StoreError> {
        match record {
            Record::Admitted {
                digest,
                first,
                hand,
                metadata,
                prompt,
                turn,
            } => self.admit(Turn {
                digest: digest.clone(),
                edits: std::collections::BTreeMap::new(),
                first: *first,
                hand: hand.clone(),
                id: turn.clone(),
                metadata: metadata.clone(),
                phase: Phase::Queued { retry: None },
                prompt: prompt.clone(),
                recovered: None,
            }),
            Record::Fact { fact, seq, state } => self.fact(fact, *seq, state.as_deref()),
            Record::Event {
                at,
                event,
                seq,
                turn,
            } => self.event(Event {
                at: *at,
                event: event.clone(),
                seq: *seq,
                turn: turn.clone(),
            }),
            Record::Status { status, turn } => self.status(turn, status),
            Record::Recovery {
                at,
                attempt,
                turn,
                unknown,
            } => self.recovery(turn, at.or(row), *attempt, unknown),
            record => self.keep(record),
        }
    }

    fn keep(&mut self, record: &Record) -> Result<(), StoreError> {
        match record {
            Record::Cell {
                cell,
                fresh,
                session,
                update,
            } => {
                let key = (Arc::from(session.as_str()), *cell);
                let mut updates = self.cells.get(&key).cloned().unwrap_or_default();
                if *fresh {
                    updates.clear();
                }
                updates.push_back(update.clone());
                self.cells.insert(key, updates);
                Ok(())
            }
            Record::Forget { cell, session } => {
                self.forget(session, *cell);
                Ok(())
            }
            Record::Checkpoint {
                payload,
                revision,
                state,
            } => {
                self.checkpoints.insert(
                    Arc::from(state.as_str()),
                    Arc::new(Checkpoint {
                        payload: payload.clone(),
                        revision: *revision,
                    }),
                );
                Ok(())
            }
            Record::Patched {
                base,
                edits,
                revision,
                state,
            } => self.patched(state, *base, edits, *revision),
            Record::Stored { key, value } => {
                self.stored
                    .insert(Arc::from(key.as_str()), Arc::from(value.as_str()));
                Ok(())
            }
            Record::Host { key, value } => self.host_field(key, value),
            Record::Hold { hold, key } => self.held(key, hold),
            record => self.call(record),
        }
    }

    fn call(&mut self, record: &Record) -> Result<(), StoreError> {
        match record {
            Record::Call {
                call_id,
                effect,
                input,
                tool,
                turn,
            } => {
                self.open(call_id, *effect, input.clone(), tool, turn.clone());
                Ok(())
            }
            Record::Intended {
                call_id,
                input,
                tool,
                turn,
                ..
            } => {
                self.open(
                    call_id,
                    EffectClass::External,
                    input.clone(),
                    tool,
                    turn.clone(),
                );
                Ok(())
            }
            Record::Result { call_id, .. } => {
                self.calls.remove(call_id.as_str());
                Ok(())
            }
            Record::Model { response, turn } => {
                let mut responses = self.models.get(turn.as_str()).cloned().unwrap_or_default();
                responses.push_back(response.clone());
                self.models.insert(Arc::from(turn.as_str()), responses);
                Ok(())
            }
            record => self.wake(record),
        }
    }

    fn open(
        &mut self,
        call_id: &str,
        effect: EffectClass,
        input: Box<RawValue>,
        tool: &str,
        turn: Option<String>,
    ) {
        self.calls.insert(
            Arc::from(call_id),
            Arc::new(OpenCall {
                effect,
                input,
                tool: tool.to_owned(),
                turn,
            }),
        );
    }

    fn admit(&mut self, turn: Turn) -> Result<(), StoreError> {
        if let Some(existing) = self.turns.get(turn.id.as_str()) {
            if !matches!(existing.phase, Phase::Finished { .. }) {
                return Err(invalid(format!("turn {} was admitted twice", turn.id)));
            }
            let first = existing.first;
            self.admitted.remove(&first);
        }
        if self.admitted.contains_key(&turn.first) {
            return Err(invalid(format!(
                "turn {} is admitted at event {}, where another turn starts",
                turn.id, turn.first
            )));
        }
        let id: Arc<str> = Arc::from(turn.id.as_str());
        self.admitted.insert(turn.first, Arc::clone(&id));
        self.turns.insert(id, Arc::new(turn));
        Ok(())
    }

    fn event(&mut self, event: Event) -> Result<(), StoreError> {
        if let Some(last) = self.events.last()
            && event.seq <= last.seq
        {
            return Err(invalid(format!(
                "event {} does not follow event {}",
                event.seq, last.seq
            )));
        }
        self.events.push_back(Arc::new(event));
        Ok(())
    }

    fn fact(&mut self, fact: &StoredFact, seq: u64, state: Option<&str>) -> Result<(), StoreError> {
        let next = self.next_fact();
        if seq != next {
            return Err(invalid(format!(
                "fact {seq} does not follow fact {}",
                next - 1
            )));
        }
        if state.is_none() {
            self.lower(seq, fact)?;
        }
        self.facts.push_back(Arc::new(KeptFact {
            fact: fact.clone(),
            state: state.map(str::to_owned),
        }));
        Ok(())
    }

    fn lower(&mut self, seq: u64, fact: &StoredFact) -> Result<(), StoreError> {
        match fact {
            Fact::Input(input) => self.lower_input(seq, input),
            Fact::Marker(Marker::Status { turn, status }) => self.status(turn, &lowered(status)),
            Fact::Marker(Marker::Recovery { turn, attempt, at }) => {
                self.recovery(turn, *at, *attempt, &[])
            }
            Fact::Intent(intent) => {
                let input = serde_json::value::to_raw_value(&intent.target).map_err(invalid)?;
                self.open(
                    &intent.call_id,
                    EffectClass::External,
                    input,
                    &intent.tool,
                    intent.turn.clone(),
                );
                Ok(())
            }
            Fact::EffectResult(result) => {
                self.calls.remove(result.call_id.as_str());
                Ok(())
            }
            Fact::Decision(Decision::Host { key, value }) => {
                if internal_host(key) {
                    return Err(invalid(format!("host field {key} names a turn log key")));
                }
                self.hosts.insert(Arc::from(key.as_str()), value.clone());
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn lower_input(&mut self, seq: u64, input: &Input<Value>) -> Result<(), StoreError> {
        let prompt = inline(&input.value)?.to_owned();
        let metadata = inline(&input.metadata)?.to_owned();
        match &input.kind {
            InputKind::Prompt => self.admit(Turn {
                digest: input_digest(&prompt, input.hand.as_deref()),
                edits: std::collections::BTreeMap::new(),
                first: input.first.unwrap_or(seq),
                hand: input.hand.clone(),
                id: input.turn.clone(),
                metadata,
                phase: Phase::Queued { retry: None },
                prompt,
                recovered: None,
            }),
            InputKind::Edit { edit_id } => {
                let hand = self.turn(&input.turn).and_then(|turn| turn.hand.clone());
                self.status(
                    &input.turn,
                    &Status::Replaced {
                        digest: input_digest(&prompt, hand.as_deref()),
                        edit: input_digest(&metadata, None),
                        edit_id: edit_id.clone(),
                        prompt,
                    },
                )
            }
            InputKind::Steer { .. } => Ok(()),
        }
    }

    fn status(&mut self, id: &str, status: &Status) -> Result<(), StoreError> {
        let Some(turn) = self.turns.get(id) else {
            return Err(invalid(format!("status of unknown turn {id}")));
        };
        if let Phase::Finished { .. } = turn.phase {
            return Err(invalid(format!("status of finished turn {id}")));
        }
        let mut turn = turn.as_ref().clone();
        moved(&mut turn, status);
        if matches!(status, Status::Finished { .. }) {
            let closed: Vec<Arc<str>> = self
                .calls
                .iter()
                .filter(|(_, call)| call.turn.as_deref() == Some(id))
                .map(|(call_id, _)| Arc::clone(call_id))
                .collect();
            for call_id in closed {
                self.calls.remove(&call_id);
            }
        }
        self.turns.insert(Arc::from(id), Arc::new(turn));
        Ok(())
    }

    fn recovery(
        &mut self,
        id: &str,
        at: Option<u64>,
        attempt: u64,
        unknown: &[String],
    ) -> Result<(), StoreError> {
        for call_id in unknown {
            self.calls.remove(call_id.as_str());
        }
        let Some(turn) = self.turns.get(id) else {
            return Err(invalid(format!("recovery of unknown turn {id}")));
        };
        let mut turn = turn.as_ref().clone();
        turn.recovered = Some(Recovered { at, attempt });
        self.turns.insert(Arc::from(id), Arc::new(turn));
        Ok(())
    }

    fn forget(&mut self, session: &str, cell: u64) {
        let origin = self
            .cells
            .remove(&(Arc::from(session), cell))
            .and_then(|updates| updates.front().cloned())
            .and_then(|first| {
                serde_json::from_str::<CellOrigin>(first.get())
                    .ok()
                    .and_then(|update| update.header)
                    .map(|header| format!("{}/", header.origin_call_id))
            });
        let Some(origin) = origin else {
            return;
        };
        let forgotten: Vec<Arc<str>> = self
            .calls
            .iter()
            .map(|(id, _)| id)
            .filter(|id| id.starts_with(&origin))
            .cloned()
            .collect();
        for id in forgotten {
            self.calls.remove(&id);
        }
    }

    fn patched(
        &mut self,
        state: &str,
        base: u64,
        edits: &RawValue,
        revision: u64,
    ) -> Result<(), StoreError> {
        let payload = self
            .checkpoints
            .get(state)
            .filter(|held| held.revision == base)
            .map(|held| apply_patch(&held.payload, edits))
            .ok_or_else(|| {
                invalid(format!(
                    "checkpoint {state} patches revision {base}, which the scope does not hold"
                ))
            })??;
        self.checkpoints
            .insert(Arc::from(state), Arc::new(Checkpoint { payload, revision }));
        Ok(())
    }

    fn host_field(&mut self, key: &str, value: &RawValue) -> Result<(), StoreError> {
        if key == nanocode_turnlog::RETIRED_KEY {
            return Err(invalid(
                "the row retires turns into older rows; a resident thread keeps its turns",
            ));
        }
        self.hosts.insert(Arc::from(key), value.to_owned());
        Ok(())
    }

    fn held(&mut self, key: &str, hold: &Hold) -> Result<(), StoreError> {
        let previous = self.holds.get(key).map(AsRef::as_ref);
        if !hold.follows(previous) {
            return Err(invalid(format!(
                "hold {key} moves from {} to {}",
                previous.map_or("nothing", Hold::phase),
                hold.phase()
            )));
        }
        self.holds.insert(Arc::from(key), Arc::new(hold.clone()));
        Ok(())
    }

    fn wake(&mut self, record: &Record) -> Result<(), StoreError> {
        match record {
            Record::Armed {
                generation,
                key,
                wake,
            } => {
                if *generation <= self.generation {
                    return Err(invalid(format!(
                        "wake {key} generation {generation} does not follow {}",
                        self.generation
                    )));
                }
                wake.fit()
                    .map_err(|unfit| invalid(format!("wake {key} is unfit: {unfit:?}")))?;
                let heard = self
                    .wakes
                    .remove(key.as_str())
                    .map(|held| held.heard.clone())
                    .unwrap_or_default();
                self.generation = *generation;
                self.wakes.insert(
                    Arc::from(key.as_str()),
                    Arc::new(ArmedWake {
                        generation: *generation,
                        heard,
                        wake: wake.clone(),
                    }),
                );
                Ok(())
            }
            Record::Heard {
                event,
                generation,
                key,
            } => {
                let mut held = self.armed(key, *generation)?.as_ref().clone();
                held.heard.push(event.clone());
                self.wakes.insert(Arc::from(key.as_str()), Arc::new(held));
                Ok(())
            }
            Record::Fired {
                generation,
                key,
                turn,
            } => {
                if self.turn(turn).is_none() {
                    return Err(invalid(format!(
                        "wake {key} fired turn {turn}, which the scope did not admit"
                    )));
                }
                self.armed(key, *generation)?;
                self.wakes.remove(key.as_str());
                Ok(())
            }
            Record::Disarmed { generation, key } => {
                self.armed(key, *generation)?;
                self.wakes.remove(key.as_str());
                Ok(())
            }
            Record::Woken { generation } => {
                if *generation < self.generation {
                    return Err(invalid(format!(
                        "wake floor {generation} is below generation {}",
                        self.generation
                    )));
                }
                self.generation = *generation;
                Ok(())
            }
            record => Err(paged(record)),
        }
    }

    fn armed(&self, key: &str, generation: u64) -> Result<&Arc<ArmedWake>, StoreError> {
        match self.wakes.get(key) {
            Some(held) if held.generation == generation => Ok(held),
            Some(held) => Err(invalid(format!(
                "wake {key} is at generation {}, not {generation}",
                held.generation
            ))),
            None => Err(invalid(format!("wake {key} is not armed"))),
        }
    }
}

mod erased {
    pub trait Encode {
        fn json(&self) -> String;
    }

    impl<T: serde::Serialize> Encode for T {
        fn json(&self) -> String {
            serde_json::to_string(self).expect("a resident thread's values encode as JSON")
        }
    }
}

fn paged(record: &Record) -> StoreError {
    let kind = serde_json::to_value(record)
        .ok()
        .and_then(|value| {
            value
                .as_object()
                .and_then(|object| object.keys().next().cloned())
        })
        .unwrap_or_default();
    invalid(format!(
        "a {kind} record points at older rows; a resident thread folds values, so its host moves paged history on its first claim"
    ))
}

fn inline(value: &Value) -> Result<&RawValue, StoreError> {
    match value {
        Value::Inline(json) => Ok(json),
        Value::Blob(blob) => Err(invalid(format!(
            "fact value {} is a blob reference; a resident thread keeps values inline",
            blob.index()
        ))),
    }
}

fn moved(turn: &mut Turn, status: &Status) {
    match status {
        Status::Cancelling => {
            if let Phase::Running { cancelling, .. } = &mut turn.phase {
                *cancelling = true;
            }
        }
        Status::Finished {
            last,
            outcome,
            result,
        } => {
            turn.phase = Phase::Finished {
                last: *last,
                outcome: *outcome,
                result: result.clone(),
            };
            turn.prompt = RawValue::NULL.to_owned();
        }
        Status::Interrupted { note: text } => {
            if let Phase::Running { note, .. } = &mut turn.phase {
                *note = Some(text.clone());
            }
        }
        Status::Replaced {
            digest,
            edit,
            edit_id,
            prompt,
        } => {
            turn.digest.clone_from(digest);
            turn.prompt = prompt.clone();
            turn.edits.insert(edit_id.clone(), edit.clone());
        }
        Status::Retrying {
            attempts,
            error,
            kind,
            retry_at,
        } => {
            let next = Some(Retry {
                attempts: *attempts,
                error: error.clone(),
                kind: kind.clone(),
                retry_at: *retry_at,
            });
            match &mut turn.phase {
                Phase::Queued { retry } | Phase::Running { retry, .. } => *retry = next,
                Phase::Finished { .. } => {}
            }
        }
        Status::Running => {
            turn.phase = Phase::Running {
                cancelling: false,
                note: None,
                retry: None,
            };
        }
    }
}

fn lowered(status: &TurnStatus) -> Status {
    match status {
        TurnStatus::Running => Status::Running,
        TurnStatus::Cancelling => Status::Cancelling,
        TurnStatus::Interrupted { note } => Status::Interrupted { note: note.clone() },
        TurnStatus::Retrying {
            attempts,
            error,
            kind,
            retry_at,
        } => Status::Retrying {
            attempts: *attempts,
            error: error.clone(),
            kind: kind.clone(),
            retry_at: *retry_at,
        },
        TurnStatus::Finished {
            last,
            outcome,
            result,
        } => Status::Finished {
            last: *last,
            outcome: match outcome {
                crate::durability::facts::Outcome::Cancelled => Outcome::Cancelled,
                crate::durability::facts::Outcome::Completed => Outcome::Completed,
                crate::durability::facts::Outcome::Failed => Outcome::Failed,
            },
            result: result.clone(),
        },
    }
}
