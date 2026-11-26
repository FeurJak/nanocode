use std::{any::Any, sync::Arc};

use nanocode_scope::{Adopted, Part, Reached, Resident, ScopeError, Scopes, Wiring};
use nanocode_turnlog::{LogHost, Phase, Record, input_digest, tag};
use serde_json::{Map, Value, value::RawValue};

use crate::{
    durability::{
        StoreError, StoreFuture,
        facts::{Fact, Input, InputKind, Marker, OwnedFact, StoredFact, TurnStatus, Value as Stored},
    },
    one_log::{Finished, Lifecycle, fact_outcome, logged, outcome, raw, record_of},
    session_log::{
        LogEntry, LoggedEvent, Started, Submission, Submitted, TurnRecord, TurnState, now,
        submission_digest,
    },
};

mod adopt;
mod parts;
pub mod sim;
mod state;

pub use state::{KeptFact, ThreadState};

type Run = dyn Fn(&mut Staged) -> Result<Box<dyn Any + Send>, StoreError> + Send + Sync;

#[derive(Clone)]
pub struct Job(Arc<Run>);

impl Job {
    pub fn new<T: Send + 'static>(
        run: impl Fn(&mut Staged) -> Result<T, StoreError> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(move |staged| {
            run(staged).map(|value| Box::new(value) as Box<dyn Any + Send>)
        }))
    }
}

pub struct Done(Box<dyn Any + Send>);

impl std::fmt::Debug for Done {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Done")
    }
}

pub struct Staged {
    state: ThreadState,
    records: Vec<Record>,
}

impl Staged {
    #[must_use]
    pub const fn state(&self) -> &ThreadState {
        &self.state
    }

    #[must_use]
    pub const fn staged(&self) -> usize {
        self.records.len()
    }

    pub fn push(&mut self, record: Record) -> Result<(), StoreError> {
        self.state.take(&record, None)?;
        self.records.push(record);
        Ok(())
    }

    pub fn fact(
        &mut self,
        fact: &OwnedFact<Box<RawValue>>,
        state: Option<String>,
    ) -> Result<u64, StoreError> {
        let seq = self.state.next_fact();
        self.push(fact_at(seq, fact, state))?;
        Ok(seq)
    }

    pub fn event(
        &mut self,
        at: u64,
        turn: Option<&str>,
        event: Box<RawValue>,
    ) -> Result<u64, StoreError> {
        let seq = self.state.next_event();
        self.push(event_at(at, seq, turn, event))?;
        Ok(seq)
    }
}

fn run(state: &ThreadState, job: &Job) -> (Vec<Record>, ThreadReply) {
    let mut staged = Staged {
        state: state.clone(),
        records: Vec::new(),
    };
    match (job.0)(&mut staged) {
        Ok(done) => {
            let mut records = staged.records;
            if sim::mutated(sim::Mutation::JobRowsDropped) {
                records.pop();
            }
            (records, ThreadReply::Job(Ok(Done(done))))
        }
        Err(error) => (Vec::new(), ThreadReply::Job(Err(error))),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadPolicy {
    pub part_rows: usize,
    pub finished_max: Option<usize>,
    pub row_bytes_max: Option<usize>,
}

pub struct ThreadFold {
    policy: ThreadPolicy,
}

impl ThreadFold {
    #[must_use]
    pub fn new(policy: ThreadPolicy) -> Self {
        assert!(policy.part_rows > 0, "a part holds at least one row");
        Self { policy }
    }
}

pub enum ThreadCommand {
    Submit {
        submission: Submission,
        at: u64,
    },
    Start {
        request_id: String,
        at: u64,
    },
    Finish {
        request_id: String,
        state: TurnState,
        fields: Map<String, Value>,
        at: u64,
    },
    Cancel {
        request_id: String,
        fields: Map<String, Value>,
        at: u64,
    },
    Append {
        entries: Vec<LogEntry>,
        at: u64,
    },
    Facts {
        facts: Vec<(OwnedFact<Box<RawValue>>, Option<String>)>,
    },
    Write {
        records: Vec<Record>,
    },
    Turn {
        request_id: String,
    },
    Pending,
    Job(Job),
}

#[derive(Debug)]
pub enum ThreadReply {
    Submitted(Submitted),
    Started(Option<Started>),
    Finished(Option<(TurnRecord, LoggedEvent)>),
    Cancelled(Option<(TurnRecord, Option<LoggedEvent>)>),
    Appended(Vec<LoggedEvent>),
    Facts(Vec<u64>),
    Wrote,
    Turn(Option<TurnRecord>),
    Pending(Vec<TurnRecord>),
    Job(Result<Done, StoreError>),
    Refused(String),
}

type Decided = Result<(Vec<Record>, ThreadReply), StoreError>;

fn applied(state: &ThreadState, records: &[Record]) -> Result<ThreadState, StoreError> {
    let mut next = state.clone();
    for record in records {
        next.take(record, None)?;
    }
    Ok(next)
}

fn stored(fact: &OwnedFact<Box<RawValue>>) -> StoredFact {
    let mapped: Result<StoredFact, std::convert::Infallible> = fact.try_map(
        |value| Ok(Stored::Inline(value.clone())),
        |decision| Ok(decision.clone()),
    );
    let Ok(fact) = mapped;
    fact
}

fn fact_at(seq: u64, fact: &OwnedFact<Box<RawValue>>, state: Option<String>) -> Record {
    Record::Fact {
        fact: stored(fact),
        seq,
        state,
    }
}

fn status_at(seq: u64, turn: &str, status: TurnStatus) -> Record {
    fact_at(
        seq,
        &Fact::Marker(Marker::Status {
            turn: turn.to_owned(),
            status,
        }),
        None,
    )
}

fn event_at(at: u64, seq: u64, turn: Option<&str>, event: Box<RawValue>) -> Record {
    Record::Event {
        at,
        event,
        seq,
        turn: turn.map(str::to_owned),
    }
}

fn record_in(state: &ThreadState, id: &str) -> Result<TurnRecord, StoreError> {
    let turn = state
        .turn(id)
        .ok_or_else(|| state::invalid(format!("turn {id} is missing after its own rows")))?;
    record_of(turn)
}

impl ThreadFold {
    fn decided(&self, state: &ThreadState, command: &ThreadCommand) -> Decided {
        match command {
            ThreadCommand::Submit { submission, at } => self.submit(state, submission, *at),
            ThreadCommand::Start { request_id, at } => start(state, request_id, *at),
            ThreadCommand::Finish {
                request_id,
                state: finished,
                fields,
                at,
            } => match state.turn(request_id).map(|turn| &turn.phase) {
                None | Some(Phase::Finished { .. }) => Ok((Vec::new(), ThreadReply::Finished(None))),
                Some(_) => {
                    let (records, record, event) =
                        close(state, request_id, *finished, fields, *at)?;
                    Ok((records, ThreadReply::Finished(Some((record, event)))))
                }
            },
            ThreadCommand::Cancel {
                request_id,
                fields,
                at,
            } => cancel(state, request_id, fields, *at),
            ThreadCommand::Append { entries, at } => Ok(append(state, entries, *at)),
            ThreadCommand::Facts { facts } => {
                let first = state.next_fact();
                let records: Vec<Record> = (first..)
                    .zip(facts)
                    .map(|(seq, (fact, owner))| fact_at(seq, fact, owner.clone()))
                    .collect();
                applied(state, &records)?;
                let seqs = (first..first + records.len() as u64).collect();
                Ok((records, ThreadReply::Facts(seqs)))
            }
            ThreadCommand::Write { records } => {
                applied(state, records)?;
                Ok((records.clone(), ThreadReply::Wrote))
            }
            ThreadCommand::Turn { request_id } => Ok((
                Vec::new(),
                ThreadReply::Turn(state.turn(request_id).map(record_of).transpose()?),
            )),
            ThreadCommand::Pending => Ok((
                Vec::new(),
                ThreadReply::Pending(state.pending().map(record_of).collect::<Result<_, _>>()?),
            )),
            ThreadCommand::Job(job) => Ok(run(state, job)),
        }
    }

    fn submit(&self, state: &ThreadState, submission: &Submission, at: u64) -> Decided {
        let id = submission.request_id.as_str();
        let digest = submission_digest(&submission.prompt, submission.hand.as_deref())?;
        let prompt = raw(&submission.prompt)?;
        let derived = input_digest(&prompt, submission.hand.as_deref());
        if let Some(turn) = state.turn(id) {
            let record = record_of(turn)?;
            let same = match turn.digest.strip_prefix(adopt::RETIRED_DIGEST) {
                Some(retired) => {
                    *retired == format!("{:032x}", tag(&digest))
                        || *retired == format!("{:032x}", tag(&derived))
                }
                None => turn.digest == digest || turn.digest == derived,
            };
            let submitted = if same {
                Submitted::Existing(record)
            } else {
                Submitted::Conflict(record)
            };
            return Ok((Vec::new(), ThreadReply::Submitted(submitted)));
        }
        if let Some(max) = self.policy.finished_max {
            let finished = state.finished();
            if finished >= max {
                return Err(StoreError::NotCommitted(format!(
                    "turn {id} refused: this thread already holds {finished} finished turns, the most one thread keeps ({max}); start a new thread"
                )));
            }
        }
        let first = state.next_event();
        let body = raw(&Lifecycle {
            kind: "queued",
            request_id: id,
        })?;
        let input = Fact::Input(Input {
            turn: id.to_owned(),
            kind: InputKind::Prompt,
            value: prompt,
            metadata: submission.metadata.clone(),
            hand: submission.hand.clone(),
            first: Some(first),
        });
        let records = vec![
            fact_at(state.next_fact(), &input, None),
            event_at(at, first, Some(id), body.clone()),
        ];
        let record = record_in(&applied(state, &records)?, id)?;
        let event = logged(first, Some(id), at, body);
        Ok((
            records,
            ThreadReply::Submitted(Submitted::Accepted { record, event }),
        ))
    }
}

fn start(state: &ThreadState, id: &str, at: u64) -> Decided {
    let Some(turn) = state.turn(id) else {
        return Ok((Vec::new(), ThreadReply::Started(None)));
    };
    let previous = match turn.phase {
        Phase::Finished { .. } => return Ok((Vec::new(), ThreadReply::Started(None))),
        Phase::Queued { .. } => TurnState::Queued,
        Phase::Running { .. } => TurnState::Running,
    };
    let seq = state.next_event();
    let body = raw(&Lifecycle {
        kind: "running",
        request_id: id,
    })?;
    let records = vec![
        status_at(state.next_fact(), id, TurnStatus::Running),
        event_at(at, seq, Some(id), body.clone()),
    ];
    let record = record_in(&applied(state, &records)?, id)?;
    let started = Started {
        previous,
        record,
        event: logged(seq, Some(id), at, body),
    };
    Ok((records, ThreadReply::Started(Some(started))))
}

fn close(
    state: &ThreadState,
    id: &str,
    finished: TurnState,
    fields: &Map<String, Value>,
    at: u64,
) -> Result<(Vec<Record>, TurnRecord, LoggedEvent), StoreError> {
    let Some(ended) = outcome(finished) else {
        return Err(StoreError::NotCommitted(format!(
            "a turn cannot finish as {}",
            finished.as_str()
        )));
    };
    let body = raw(&Finished {
        kind: "result",
        request_id: id,
        status: finished.as_str(),
        fields,
    })?;
    let last = state.next_event();
    let records = vec![
        event_at(at, last, Some(id), body.clone()),
        status_at(
            state.next_fact(),
            id,
            TurnStatus::Finished {
                last,
                outcome: fact_outcome(ended),
                result: body.clone(),
            },
        ),
    ];
    let record = record_in(&applied(state, &records)?, id)?;
    Ok((records, record, logged(last, Some(id), at, body)))
}

fn cancel(state: &ThreadState, id: &str, fields: &Map<String, Value>, at: u64) -> Decided {
    let Some(turn) = state.turn(id) else {
        return Ok((Vec::new(), ThreadReply::Cancelled(None)));
    };
    match turn.phase {
        Phase::Running {
            cancelling: false, ..
        } => {
            let records = vec![status_at(state.next_fact(), id, TurnStatus::Cancelling)];
            let record = record_in(&applied(state, &records)?, id)?;
            Ok((records, ThreadReply::Cancelled(Some((record, None)))))
        }
        Phase::Queued { .. } => {
            let (records, record, event) = close(state, id, TurnState::Cancelled, fields, at)?;
            Ok((records, ThreadReply::Cancelled(Some((record, Some(event))))))
        }
        Phase::Running { .. } | Phase::Finished { .. } => Ok((
            Vec::new(),
            ThreadReply::Cancelled(Some((record_of(turn)?, None))),
        )),
    }
}

fn append(state: &ThreadState, entries: &[LogEntry], at: u64) -> (Vec<Record>, ThreadReply) {
    let first = state.next_event();
    let mut records = Vec::with_capacity(entries.len());
    let mut events = Vec::with_capacity(entries.len());
    for (seq, entry) in (first..).zip(entries) {
        let turn = entry.request_id.as_deref();
        records.push(event_at(at, seq, turn, entry.event.clone()));
        events.push(logged(seq, turn, at, entry.event.clone()));
    }
    (records, ThreadReply::Appended(events))
}

impl ThreadFold {
    fn too_large(command: &ThreadCommand, bytes: usize, limit: usize) -> ThreadReply {
        match command {
            ThreadCommand::Submit { .. } => {
                ThreadReply::Submitted(Submitted::TooLarge { bytes, limit })
            }
            _ => ThreadReply::Refused(format!(
                "the command's row is {bytes} bytes, more than the {limit} a row may hold"
            )),
        }
    }
}

impl Resident for ThreadFold {
    type State = ThreadState;
    type Command = ThreadCommand;
    type Reply = ThreadReply;

    fn empty(&self) -> ThreadState {
        ThreadState::default()
    }

    fn decide(&self, state: &ThreadState, command: &ThreadCommand) -> (Vec<String>, ThreadReply) {
        let (records, reply) = match self.decided(state, command) {
            Ok(decided) => decided,
            Err(error) => return (Vec::new(), ThreadReply::Refused(error.to_string())),
        };
        if records.is_empty() {
            return (Vec::new(), reply);
        }
        let body = match nanocode_turnlog::encode(&records) {
            Ok(body) => body,
            Err(error) => return (Vec::new(), ThreadReply::Refused(error.to_string())),
        };
        match self.policy.row_bytes_max {
            Some(limit) if body.len() > limit => {
                (Vec::new(), Self::too_large(command, body.len(), limit))
            }
            _ => (vec![body], reply),
        }
    }

    fn apply(&self, state: &mut ThreadState, seq: u64, row: &str) -> Result<(), StoreError> {
        state.load(seq, row)
    }

    fn encode(&self, state: &ThreadState) -> Vec<Part> {
        parts::encode(state, self.policy.part_rows)
    }

    fn decode(&self, parts: Vec<Part>) -> Result<ThreadState, StoreError> {
        parts::decode(parts)
    }

    fn weight(&self, state: &ThreadState) -> usize {
        state.bytes
    }

    fn routable(&self, command: &ThreadCommand) -> bool {
        !matches!(command, ThreadCommand::Job(_)) || sim::mutated(sim::Mutation::JobForwarded)
    }

    fn adopt<'a>(
        &'a self,
        log: &'a Arc<dyn LogHost>,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Option<Adopted<ThreadState>>, StoreError>> {
        Box::pin(adopt::adopt(log, scope, self.policy.part_rows))
    }
}

#[derive(Clone)]
pub struct ThreadLogs {
    scopes: Scopes<ThreadFold>,
}

impl ThreadLogs {
    #[must_use]
    pub fn new(wiring: Wiring<ThreadFold>) -> Self {
        Self {
            scopes: Scopes::new(wiring),
        }
    }

    #[must_use]
    pub const fn scopes(&self) -> &Scopes<ThreadFold> {
        &self.scopes
    }

    pub async fn get(&self, thread: &str) -> Result<ThreadLog, ScopeError> {
        Ok(ThreadLog {
            reached: self.scopes.get(thread).await?,
        })
    }
}

pub struct ThreadLog {
    reached: Reached<ThreadFold>,
}

fn unexpected(asked: &str, reply: &ThreadReply) -> ! {
    panic!("a thread scope answers a {asked} command with {reply:?}")
}

impl ThreadLog {
    #[must_use]
    pub fn state(&self) -> Option<Arc<ThreadState>> {
        match &self.reached {
            Reached::Local(scope) => Some(scope.state()),
            Reached::Routed(_) => None,
        }
    }

    #[must_use]
    pub fn next(&self) -> Option<u64> {
        match &self.reached {
            Reached::Local(scope) => Some(scope.next()),
            Reached::Routed(_) => None,
        }
    }

    #[must_use]
    pub fn holder(&self) -> Option<&str> {
        match &self.reached {
            Reached::Local(_) => None,
            Reached::Routed(routed) => Some(routed.holder()),
        }
    }

    pub async fn send(&self, command: ThreadCommand) -> Result<ThreadReply, ScopeError> {
        match self.reached.send(command).await?.reply {
            ThreadReply::Refused(why) => Err(ScopeError::Store(StoreError::NotCommitted(why))),
            reply => Ok(reply),
        }
    }

    pub async fn submit(&self, submission: &Submission) -> Result<Submitted, ScopeError> {
        let command = ThreadCommand::Submit {
            submission: submission.clone(),
            at: now(),
        };
        match self.send(command).await? {
            ThreadReply::Submitted(submitted) => Ok(submitted),
            reply => unexpected("submit", &reply),
        }
    }

    pub async fn start(&self, request_id: &str) -> Result<Option<Started>, ScopeError> {
        let command = ThreadCommand::Start {
            request_id: request_id.to_owned(),
            at: now(),
        };
        match self.send(command).await? {
            ThreadReply::Started(started) => Ok(started),
            reply => unexpected("start", &reply),
        }
    }

    pub async fn finish(
        &self,
        request_id: &str,
        state: TurnState,
        fields: &Map<String, Value>,
    ) -> Result<Option<(TurnRecord, LoggedEvent)>, ScopeError> {
        let command = ThreadCommand::Finish {
            request_id: request_id.to_owned(),
            state,
            fields: fields.clone(),
            at: now(),
        };
        match self.send(command).await? {
            ThreadReply::Finished(finished) => Ok(finished),
            reply => unexpected("finish", &reply),
        }
    }

    pub async fn cancel(
        &self,
        request_id: &str,
        fields: &Map<String, Value>,
    ) -> Result<Option<(TurnRecord, Option<LoggedEvent>)>, ScopeError> {
        let command = ThreadCommand::Cancel {
            request_id: request_id.to_owned(),
            fields: fields.clone(),
            at: now(),
        };
        match self.send(command).await? {
            ThreadReply::Cancelled(cancelled) => Ok(cancelled),
            reply => unexpected("cancel", &reply),
        }
    }

    pub async fn append(&self, entries: &[LogEntry]) -> Result<Vec<LoggedEvent>, ScopeError> {
        let command = ThreadCommand::Append {
            entries: entries.to_vec(),
            at: now(),
        };
        match self.send(command).await? {
            ThreadReply::Appended(events) => Ok(events),
            reply => unexpected("append", &reply),
        }
    }

    pub async fn facts(
        &self,
        facts: Vec<(OwnedFact<Box<RawValue>>, Option<String>)>,
    ) -> Result<Vec<u64>, ScopeError> {
        match self.send(ThreadCommand::Facts { facts }).await? {
            ThreadReply::Facts(seqs) => Ok(seqs),
            reply => unexpected("facts", &reply),
        }
    }

    pub async fn write(&self, records: Vec<Record>) -> Result<(), ScopeError> {
        match self.send(ThreadCommand::Write { records }).await? {
            ThreadReply::Wrote => Ok(()),
            reply => unexpected("write", &reply),
        }
    }

    pub async fn job<T: Send + 'static>(
        &self,
        run: impl Fn(&mut Staged) -> Result<T, StoreError> + Send + Sync + 'static,
    ) -> Result<T, ScopeError> {
        if let Reached::Routed(routed) = &self.reached
            && !sim::mutated(sim::Mutation::JobForwarded)
        {
            return Err(ScopeError::Routed {
                holder: routed.holder().to_owned(),
            });
        }
        match self.send(ThreadCommand::Job(Job::new(run))).await? {
            ThreadReply::Job(Ok(Done(done))) => match done.downcast::<T>() {
                Ok(value) => Ok(*value),
                Err(_) => panic!("a thread job answers with the type its closure returns"),
            },
            ThreadReply::Job(Err(error)) => Err(ScopeError::Store(error)),
            reply => unexpected("job", &reply),
        }
    }

    pub async fn turn(&self, request_id: &str) -> Result<Option<TurnRecord>, ScopeError> {
        if let Some(state) = self.state() {
            return Ok(state.turn(request_id).map(record_of).transpose()?);
        }
        let command = ThreadCommand::Turn {
            request_id: request_id.to_owned(),
        };
        match self.send(command).await? {
            ThreadReply::Turn(turn) => Ok(turn),
            reply => unexpected("turn", &reply),
        }
    }

    pub async fn pending(&self) -> Result<Vec<TurnRecord>, ScopeError> {
        if let Some(state) = self.state() {
            return Ok(state.pending().map(record_of).collect::<Result<_, _>>()?);
        }
        match self.send(ThreadCommand::Pending).await? {
            ThreadReply::Pending(pending) => Ok(pending),
            reply => unexpected("pending", &reply),
        }
    }
}
