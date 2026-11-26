use std::sync::Arc;

use nanocode_sync::{Atom, OrdSet};
use nanocode_tool_host::embedded::HostFuture;
use nanocode_turnlog::{Phase, Record};
use serde_json::{Map, Value};

use super::{Staged, ThreadLog, append, close, facts::refused, record_in, sim, start};
use crate::{
    durability::{
        StoreError,
        facts::{EffectResult, Fact, Marker, OwnedFact, TurnStatus},
    },
    one_log::{Owners, Retrying, encoded, logged, raw, record_of, unknown_outcome},
    session_log::{
        LogEntry, LoggedEvent, Started, Submission, Submitted, TurnRecord, TurnState, now,
    },
    turn_log::{EventLog, Logged, Recording, TurnLog},
};

type Answered = Option<(TurnRecord, Option<LoggedEvent>)>;

pub struct ThreadTurns {
    log: Arc<ThreadLog>,
    recovering: Atom<OrdSet<Arc<str>>>,
    restarts: u64,
}

fn settle(staged: &mut Staged, turn: &str, owners: Owners) -> Result<Vec<String>, StoreError> {
    let dangling = staged
        .state()
        .intents()
        .filter(|(_, _, call)| match (owners, call.turn.as_deref()) {
            (_, Some(owner)) => owner == turn,
            (Owners::TurnOrNone, None) => true,
            (Owners::Turn, None) => false,
        })
        .map(|(call_id, state, _)| (call_id.to_owned(), state.map(str::to_owned)))
        .collect::<Vec<_>>();
    if dangling.is_empty() {
        return Ok(Vec::new());
    }
    let output = unknown_outcome()?;
    for (call_id, state) in &dangling {
        let result: OwnedFact<_> = Fact::EffectResult(EffectResult {
            call_id: call_id.clone(),
            output: output.clone(),
        });
        staged.fact(&result, state.clone())?;
    }
    Ok(dangling.into_iter().map(|(call_id, _)| call_id).collect())
}

fn status(staged: &mut Staged, turn: &str, status: TurnStatus) -> Result<u64, StoreError> {
    let fact = Fact::Marker(Marker::Status {
        turn: turn.to_owned(),
        status,
    });
    staged.fact(&fact, None)
}

fn closed(
    staged: &mut Staged,
    id: &str,
    state: TurnState,
    fields: &Map<String, Value>,
    at: u64,
) -> Result<(TurnRecord, LoggedEvent), StoreError> {
    settle(staged, id, Owners::Turn)?;
    let last = staged.seq(1)?;
    let (records, record, event) = close(staged.state(), id, state, fields, at, last)?;
    staged.extend(records)?;
    Ok((record, event))
}

fn recovered(staged: &mut Staged, id: &str, restarts: u64, at: u64) -> Result<bool, StoreError> {
    let attempt = staged.state().next_attempt(id);
    if attempt > restarts {
        let fields = Map::from_iter([(
            "error".to_owned(),
            Value::String(format!(
                "the turn restarted {} times without progress",
                attempt - 1
            )),
        )]);
        closed(staged, id, TurnState::Failed, &fields, at)?;
        return Ok(false);
    }
    let state = staged.state();
    let unknown = state
        .unknown()
        .into_iter()
        .filter(|call_id| state.intent(call_id).is_none())
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        let marker = Fact::Marker(Marker::Recovery {
            turn: id.to_owned(),
            attempt,
            at: None,
        });
        staged.fact(&marker, None)?;
    } else {
        staged.push(Record::Recovery {
            at: None,
            attempt,
            turn: id.to_owned(),
            unknown,
        })?;
    }
    Ok(true)
}

fn started(
    staged: &mut Staged,
    id: &str,
    recovering: bool,
    restarts: u64,
    at: u64,
) -> Result<Option<Started>, StoreError> {
    let Some(turn) = staged.state().turn(id) else {
        return Ok(None);
    };
    if matches!(turn.phase, Phase::Finished { .. }) {
        return Ok(None);
    }
    if recovering
        && !sim::mutated(sim::Mutation::RecoveryUncounted)
        && !recovered(staged, id, restarts, at)?
    {
        return Ok(None);
    }
    let seq = staged.seq(1)?;
    let (records, started) = start(staged.state(), id, at, seq)?;
    staged.extend(records)?;
    Ok(started)
}

fn cancelled(
    staged: &mut Staged,
    id: &str,
    fields: &Map<String, Value>,
    at: u64,
) -> Result<Answered, StoreError> {
    let Some(turn) = staged.turn(id)? else {
        return Ok(None);
    };
    match turn.phase {
        Phase::Running {
            cancelling: false, ..
        } => {
            settle(staged, id, Owners::Turn)?;
            status(staged, id, TurnStatus::Cancelling)?;
            Ok(Some((record_in(staged.state(), id)?, None)))
        }
        Phase::Queued { .. } => {
            let (record, event) = closed(staged, id, TurnState::Cancelled, fields, at)?;
            Ok(Some((record, Some(event))))
        }
        Phase::Running { .. } | Phase::Finished { .. } => Ok(Some((record_of(&turn)?, None))),
    }
}

struct Retry<'a> {
    id: &'a str,
    error: &'a str,
    kind: &'a str,
    retry_at: u64,
}

fn retried(staged: &mut Staged, retry: &Retry<'_>, at: u64) -> Result<Answered, StoreError> {
    let Some(turn) = staged.turn(retry.id)? else {
        return Ok(None);
    };
    let previous = match &turn.phase {
        Phase::Finished { .. } => return Ok(Some((record_of(&turn)?, None))),
        Phase::Queued { retry } | Phase::Running { retry, .. } => retry.clone(),
    };
    let attempts = previous.as_ref().map_or(0, |held| held.attempts) + 1;
    let changed = previous
        .as_ref()
        .is_none_or(|old| old.error != retry.error || old.kind != retry.kind);
    let event = if changed {
        let body = raw(&Retrying {
            tag: "retrying",
            request_id: retry.id,
            error: retry.error,
            kind: retry.kind,
            attempts,
            retry_at: retry.retry_at,
        })?;
        let seq = staged.event(at, Some(retry.id), body.clone())?;
        Some(logged(seq, Some(retry.id), at, body))
    } else {
        None
    };
    let retrying = TurnStatus::Retrying {
        attempts,
        error: retry.error.to_owned(),
        kind: retry.kind.to_owned(),
        retry_at: retry.retry_at,
    };
    status(staged, retry.id, retrying)?;
    Ok(Some((record_in(staged.state(), retry.id)?, event)))
}

impl ThreadTurns {
    pub fn new(log: Arc<ThreadLog>, restarts: u64) -> Result<Self, StoreError> {
        let Some(state) = log.state() else {
            return Err(refused(nanocode_scope::ScopeError::Routed {
                holder: log.holder().unwrap_or_default().to_owned(),
            }));
        };
        let recovering = state
            .pending()
            .map(|turn| Arc::from(turn.id.as_str()))
            .collect();
        Ok(Self {
            log,
            recovering: Atom::new(recovering),
            restarts,
        })
    }

    pub fn rebound(&self, log: Arc<ThreadLog>) -> Result<Self, StoreError> {
        if log.state().is_none() {
            return Err(refused(nanocode_scope::ScopeError::Routed {
                holder: log.holder().unwrap_or_default().to_owned(),
            }));
        }
        Ok(Self {
            log,
            recovering: Atom::new(self.recovering.read(Clone::clone)),
            restarts: self.restarts,
        })
    }

    #[must_use]
    pub const fn log(&self) -> &Arc<ThreadLog> {
        &self.log
    }

    fn recovered(&self, request_id: &str) {
        self.recovering.update(|turns| {
            turns.remove(request_id);
        });
    }

    pub async fn submit(&self, submission: &Submission) -> Result<Submitted, StoreError> {
        self.log.submit(submission).await.map_err(refused)
    }

    pub async fn submit_buffered(&self, submission: &Submission) -> Result<Submitted, StoreError> {
        self.log.submit_staged(submission).await.map_err(refused)
    }

    pub async fn append(&self, entries: &[LogEntry]) -> Result<Vec<LoggedEvent>, StoreError> {
        let entries = entries.to_vec();
        let at = now();
        self.log
            .job_staged(move |staged| {
                let first = staged.seq(entries.len())?;
                let (records, events) = append(staged.state(), &entries, at, first);
                staged.extend(records)?;
                Ok(events)
            })
            .await
            .map_err(refused)
    }

    pub async fn turn(&self, request_id: &str) -> Result<Option<TurnRecord>, StoreError> {
        let id = request_id.to_owned();
        self.log
            .job_staged(move |staged| staged.turn(&id)?.as_deref().map(record_of).transpose())
            .await
            .map_err(refused)
    }

    pub async fn pending(&self) -> Result<Vec<TurnRecord>, StoreError> {
        self.log
            .job_staged(|staged| staged.state().pending().map(record_of).collect())
            .await
            .map_err(refused)
    }

    pub async fn settle_intents(&self, turn: &str) -> Result<Vec<String>, StoreError> {
        let turn = turn.to_owned();
        let settled = self
            .log
            .job_staged(move |staged| settle(staged, &turn, Owners::TurnOrNone))
            .await
            .map_err(refused)?;
        if !settled.is_empty() {
            self.log.flush(None).await.map_err(refused)?;
        }
        Ok(settled)
    }

    pub async fn start(&self, request_id: &str) -> Result<Option<Started>, StoreError> {
        let recovering = self.recovering.read(|turns| turns.contains(request_id));
        if recovering {
            self.settle_intents(request_id).await?;
        }
        let id = request_id.to_owned();
        let restarts = self.restarts;
        let at = now();
        let job = move |staged: &mut Staged| started(staged, &id, recovering, restarts, at);
        let started = if recovering {
            self.log.job(job).await
        } else {
            self.log.job_staged(job).await
        }
        .map_err(refused)?;
        if recovering {
            self.recovered(request_id);
        }
        Ok(started)
    }

    pub async fn finish(
        &self,
        request_id: &str,
        state: TurnState,
        fields: &Map<String, Value>,
    ) -> Result<Option<(TurnRecord, LoggedEvent)>, StoreError> {
        let id = request_id.to_owned();
        let fields = fields.clone();
        let at = now();
        let finished = self
            .log
            .job(
                move |staged| match staged.state().turn(&id).map(|turn| &turn.phase) {
                    None | Some(Phase::Finished { .. }) => Ok(None),
                    Some(_) => closed(staged, &id, state, &fields, at).map(Some),
                },
            )
            .await
            .map_err(refused)?;
        if finished.is_some() {
            self.recovered(request_id);
        }
        Ok(finished)
    }

    pub async fn cancel(
        &self,
        request_id: &str,
        fields: &Map<String, Value>,
    ) -> Result<Answered, StoreError> {
        let id = request_id.to_owned();
        let fields = fields.clone();
        let at = now();
        let cancelled = self
            .log
            .job(move |staged| cancelled(staged, &id, &fields, at))
            .await
            .map_err(refused)?;
        if matches!(cancelled, Some((_, Some(_)))) {
            self.recovered(request_id);
        }
        Ok(cancelled)
    }

    pub async fn withdraw_steered(
        &self,
        request_id: &str,
        digest: &str,
    ) -> Result<Option<(TurnRecord, LoggedEvent)>, StoreError> {
        let id = request_id.to_owned();
        let digest = digest.to_owned();
        let at = now();
        let withdrawn = self
            .log
            .job(move |staged| {
                let queued = staged.state().turn(&id).is_some_and(|turn| {
                    matches!(turn.phase, Phase::Queued { .. }) && turn.digest == digest
                });
                if !queued {
                    return Ok(None);
                }
                let fields = Map::from_iter([
                    ("cancelled".to_owned(), Value::Bool(true)),
                    ("steered".to_owned(), Value::Bool(true)),
                ]);
                closed(staged, &id, TurnState::Cancelled, &fields, at).map(Some)
            })
            .await
            .map_err(refused)?;
        if withdrawn.is_some() {
            self.recovered(request_id);
        }
        Ok(withdrawn)
    }

    pub async fn interrupted(&self, request_id: &str, note: &str) -> Result<(), StoreError> {
        let id = request_id.to_owned();
        let note = note.to_owned();
        self.log
            .job_staged(move |staged| {
                let running = staged
                    .state()
                    .turn(&id)
                    .is_some_and(|turn| matches!(turn.phase, Phase::Running { .. }));
                if !running {
                    return Ok(());
                }
                settle(staged, &id, Owners::Turn)?;
                let interrupted = TurnStatus::Interrupted { note: note.clone() };
                status(staged, &id, interrupted).map(drop)
            })
            .await
            .map_err(refused)
    }

    pub async fn retry(
        &self,
        request_id: &str,
        error: &str,
        kind: &str,
        retry_at: u64,
    ) -> Result<Answered, StoreError> {
        let id = request_id.to_owned();
        let error = error.to_owned();
        let kind = kind.to_owned();
        let at = now();
        self.log
            .job_staged(move |staged| {
                let retry = Retry {
                    id: &id,
                    error: &error,
                    kind: &kind,
                    retry_at,
                };
                retried(staged, &retry, at)
            })
            .await
            .map_err(refused)
    }

    async fn recorded(&self, recording: Recording<'_>) -> Result<Vec<LoggedEvent>, String> {
        let encoded = encoded(&recording)?;
        let at = now();
        self.log
            .job_staged(move |staged| {
                let mut events = Vec::with_capacity(encoded.len());
                for (stored, live, request_id) in &encoded {
                    let request_id = request_id.as_deref();
                    let Some(stored) = stored else {
                        events.push(logged(0, request_id, at, live.clone()));
                        continue;
                    };
                    let seq = staged.event(at, request_id, stored.clone())?;
                    events.push(logged(seq, request_id, at, live.clone()));
                }
                Ok(events)
            })
            .await
            .map_err(|error| error.to_string())
    }

    async fn flushed(&self) -> Result<(), StoreError> {
        self.log.flush(None).await.map(drop).map_err(refused)
    }
}

impl EventLog for ThreadTurns {
    fn record<'a>(
        &'a self,
        recording: Recording<'a>,
    ) -> HostFuture<'a, Result<Vec<LoggedEvent>, String>> {
        Box::pin(self.recorded(recording))
    }

    fn flush(&self) -> Logged<'_, ()> {
        Box::pin(self.flushed())
    }
}

impl TurnLog for ThreadTurns {
    fn turn<'a>(&'a self, request_id: &'a str) -> Logged<'a, Option<TurnRecord>> {
        Box::pin(Self::turn(self, request_id))
    }

    fn pending(&self) -> Logged<'_, Vec<TurnRecord>> {
        Box::pin(Self::pending(self))
    }

    fn start<'a>(&'a self, request_id: &'a str) -> Logged<'a, Option<Started>> {
        Box::pin(Self::start(self, request_id))
    }

    fn finish<'a>(
        &'a self,
        request_id: &'a str,
        state: TurnState,
        fields: &'a Map<String, Value>,
    ) -> Logged<'a, Option<(TurnRecord, LoggedEvent)>> {
        Box::pin(Self::finish(self, request_id, state, fields))
    }

    fn cancel<'a>(
        &'a self,
        request_id: &'a str,
        fields: &'a Map<String, Value>,
    ) -> Logged<'a, Answered> {
        Box::pin(Self::cancel(self, request_id, fields))
    }

    fn withdraw_steered<'a>(
        &'a self,
        request_id: &'a str,
        digest: &'a str,
    ) -> Logged<'a, Option<(TurnRecord, LoggedEvent)>> {
        Box::pin(Self::withdraw_steered(self, request_id, digest))
    }
}
