use std::{
    future::Future,
    sync::{Arc, Weak},
};

use nanocode_scope::ScopeError;
use nanocode_sync::Atom;
use nanocode_tool_host::{
    ToolOutput,
    effects::{Carried, EffectCall, EffectGate, RemoteBarrier},
    embedded::HostFuture,
    hold::{HoldCall, HoldLedger, LedgerError, Running},
};
use nanocode_turnlog::{Answer, ArmedWake, EffectClass, Resumption, Wake, WakeRider};
use serde_json::{Map, Value, value::RawValue};

use super::{Exported, Fire, Heard, Imported, OneLog, Policy, WakeError};
use crate::{
    durability::{StoreError, StoreRecord},
    session_log::{LogEntry, LoggedEvent, Started, Submission, Submitted, TurnRecord, TurnState},
    thread_scope::{
        ThreadHolds, ThreadJournals, ThreadLog, ThreadLogs, ThreadState, ThreadStore, ThreadTurns,
        ThreadWakes, sim, wakes,
    },
};

type Answered = Option<(TurnRecord, Option<LoggedEvent>)>;

pub struct OneLogOver {
    logs: Arc<ThreadLogs>,
    thread: String,
    bound: Atom<Arc<Bound>>,
    index: Atom<Option<Arc<dyn WakeRider>>>,
    this: Weak<Self>,
}

struct Bound {
    log: Arc<ThreadLog>,
    turns: ThreadTurns,
    holds: ThreadHolds,
    wakes: ThreadWakes,
    store: ThreadStore,
    journals: ThreadJournals,
}

trait Refusal: Sized {
    fn lost(&self) -> bool;

    fn of(error: StoreError) -> Self;
}

impl Refusal for StoreError {
    fn lost(&self) -> bool {
        matches!(self, Self::Fenced)
    }

    fn of(error: StoreError) -> Self {
        error
    }
}

impl Refusal for WakeError {
    fn lost(&self) -> bool {
        matches!(self, Self::Store(StoreError::Fenced))
    }

    fn of(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl Refusal for String {
    fn lost(&self) -> bool {
        *self == StoreError::Fenced.to_string()
    }

    fn of(error: StoreError) -> Self {
        error.to_string()
    }
}

impl Refusal for LedgerError {
    fn lost(&self) -> bool {
        matches!(self, Self::Journal(why) if why.lost())
    }

    fn of(error: StoreError) -> Self {
        Self::Journal(error.to_string())
    }
}

fn refused(error: ScopeError) -> StoreError {
    match error {
        ScopeError::Store(error) => error,
        ScopeError::Busy => StoreError::Busy,
        ScopeError::Lost => StoreError::Fenced,
        error => StoreError::NotCommitted(error.to_string()),
    }
}

fn local(log: &ThreadLog) -> Result<Arc<ThreadState>, StoreError> {
    log.state().ok_or_else(|| {
        refused(ScopeError::Routed {
            holder: log.holder().unwrap_or_default().to_owned(),
        })
    })
}

impl OneLog {
    pub async fn over(
        logs: Arc<ThreadLogs>,
        thread: &str,
        policy: &Policy,
    ) -> Result<Arc<OneLogOver>, StoreError> {
        let log = Arc::new(logs.get(thread).await.map_err(refused)?);
        let holds = ThreadHolds::new(Arc::clone(&log), policy.effects.clone())?;
        let bound = Bound {
            turns: ThreadTurns::new(Arc::clone(&log), policy.restarts)?,
            wakes: ThreadWakes::new(Arc::clone(&log), thread, policy.wakes_max)?,
            store: ThreadStore::new(Arc::clone(&log)),
            journals: ThreadJournals::new(Arc::clone(&log), holds.effects()),
            holds,
            log,
        };
        Ok(Arc::new_cyclic(|this| OneLogOver {
            logs,
            thread: thread.to_owned(),
            bound: Atom::new(Arc::new(bound)),
            index: Atom::new(None),
            this: Weak::clone(this),
        }))
    }
}

impl OneLogOver {
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.thread
    }

    fn current(&self) -> Arc<Bound> {
        self.bound.read(Arc::clone)
    }

    async fn rebound(&self, stale: &Bound) -> Result<Arc<Bound>, StoreError> {
        let log = Arc::new(self.logs.get(&self.thread).await.map_err(refused)?);
        let bound = Arc::new(Bound {
            turns: stale.turns.rebound(Arc::clone(&log))?,
            holds: stale.holds.rebound(Arc::clone(&log))?,
            wakes: stale.wakes.rebound(Arc::clone(&log))?,
            store: stale.store.rebound(Arc::clone(&log)),
            journals: stale.journals.rebound(Arc::clone(&log)),
            log,
        });
        if let Some(index) = self.index.read(Clone::clone) {
            bound.wakes.index_wakes(index).await?;
        }
        self.bound.set(Arc::clone(&bound));
        Ok(bound)
    }

    async fn retried<T, E, F, Fut>(&self, command: F) -> Result<T, E>
    where
        E: Refusal,
        F: Fn(Arc<Bound>) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let bound = self.current();
        match command(Arc::clone(&bound)).await {
            Err(error) if error.lost() && !sim::mutated(sim::Mutation::LostUnretried) => {
                let bound = self.rebound(&bound).await.map_err(E::of)?;
                command(bound).await
            }
            answered => answered,
        }
    }

    async fn published(&self) -> Result<Arc<ThreadState>, StoreError> {
        local(&self.logs.get(&self.thread).await.map_err(refused)?)
    }

    pub async fn read<R>(&self, read: impl FnOnce(&ThreadState) -> R) -> Result<R, StoreError> {
        Ok(read(&*self.published().await?))
    }

    #[must_use]
    pub fn remembered_bytes(&self) -> usize {
        self.current().log.state().map_or(0, |state| state.weighed())
    }

    pub async fn reclaim(&self) -> Result<(), StoreError> {
        self.published().await.map(drop)
    }

    pub async fn submit(&self, submission: &Submission) -> Result<Submitted, StoreError> {
        self.retried(|bound| async move { bound.turns.submit(submission).await })
            .await
    }

    pub async fn submit_buffered(&self, submission: &Submission) -> Result<Submitted, StoreError> {
        self.retried(|bound| async move { bound.turns.submit_buffered(submission).await })
            .await
    }

    pub async fn append(&self, entries: &[LogEntry]) -> Result<Vec<LoggedEvent>, StoreError> {
        self.retried(|bound| async move { bound.turns.append(entries).await })
            .await
    }

    pub async fn turn(&self, request_id: &str) -> Result<Option<TurnRecord>, StoreError> {
        self.retried(|bound| async move { bound.turns.turn(request_id).await })
            .await
    }

    pub async fn pending(&self) -> Result<Vec<TurnRecord>, StoreError> {
        self.retried(|bound| async move { bound.turns.pending().await })
            .await
    }

    pub async fn start(&self, request_id: &str) -> Result<Option<Started>, StoreError> {
        self.retried(|bound| async move { bound.turns.start(request_id).await })
            .await
    }

    pub async fn finish(
        &self,
        request_id: &str,
        state: TurnState,
        fields: &Map<String, Value>,
    ) -> Result<Option<(TurnRecord, LoggedEvent)>, StoreError> {
        self.retried(|bound| async move { bound.turns.finish(request_id, state, fields).await })
            .await
    }

    pub async fn cancel(
        &self,
        request_id: &str,
        fields: &Map<String, Value>,
    ) -> Result<Answered, StoreError> {
        self.retried(|bound| async move { bound.turns.cancel(request_id, fields).await })
            .await
    }

    pub async fn withdraw_steered(
        &self,
        request_id: &str,
        digest: &str,
    ) -> Result<Option<(TurnRecord, LoggedEvent)>, StoreError> {
        self.retried(|bound| async move { bound.turns.withdraw_steered(request_id, digest).await })
            .await
    }

    pub async fn interrupted(&self, request_id: &str, note: &str) -> Result<(), StoreError> {
        self.retried(|bound| async move { bound.turns.interrupted(request_id, note).await })
            .await
    }

    pub async fn retry(
        &self,
        request_id: &str,
        error: &str,
        kind: &str,
        retry_at: u64,
    ) -> Result<Answered, StoreError> {
        self.retried(|bound| async move {
            bound.turns.retry(request_id, error, kind, retry_at).await
        })
        .await
    }

    #[must_use]
    pub fn store(&self) -> ThreadStore {
        self.current().store.clone()
    }

    #[must_use]
    pub fn journals(&self) -> ThreadJournals {
        self.current().journals.clone()
    }

    pub fn settle_durably(&self) {
        self.current().store.settle_durably();
    }

    pub async fn import_state(
        &self,
        state_id: &str,
        revision: u64,
        payload: &str,
        records: &[StoreRecord],
    ) -> Result<Imported, StoreError> {
        self.retried(|bound| async move {
            bound
                .store
                .import_state(state_id, revision, payload, records)
                .await
        })
        .await
    }

    pub async fn export_state(&self, state_id: &str) -> Result<Option<Exported>, StoreError> {
        self.retried(|bound| async move { bound.store.export_state(state_id).await })
            .await
    }

    pub fn declare(&self, tool: impl Into<String>, class: EffectClass) {
        self.current().holds.declare(tool, class);
    }

    #[must_use]
    pub fn effect_of(&self, tool: &str) -> EffectClass {
        self.current().holds.effect_of(tool)
    }

    #[must_use]
    pub fn uncertain_holds(&self) -> Vec<String> {
        self.current().holds.uncertain_holds()
    }

    fn this(&self) -> Arc<Self> {
        self.this
            .upgrade()
            .expect("a OneLog over a thread is reached through its Arc")
    }

    #[must_use]
    pub fn gate(&self) -> Arc<dyn EffectGate> {
        self.this()
    }

    #[must_use]
    pub fn ledger(&self) -> Arc<dyn HoldLedger> {
        self.this()
    }

    #[must_use]
    pub fn barrier(&self) -> Arc<dyn RemoteBarrier> {
        self.this()
    }

    pub async fn index_wakes(&self, index: Arc<dyn WakeRider>) -> Result<(), StoreError> {
        self.index.set(Some(Arc::clone(&index)));
        self.retried(|bound| {
            let index = Arc::clone(&index);
            async move { bound.wakes.index_wakes(index).await }
        })
        .await
    }

    pub async fn arm(&self, key: &str, wake: Wake) -> Result<u64, WakeError> {
        self.retried(|bound| {
            let wake = wake.clone();
            async move { bound.wakes.arm(key, wake).await }
        })
        .await
    }

    pub async fn disarm(&self, key: &str) -> Result<Option<u64>, StoreError> {
        self.retried(|bound| async move { bound.wakes.disarm(key).await })
            .await
    }

    pub async fn hear(
        &self,
        topic: &str,
        fields: &Map<String, Value>,
        event: Box<RawValue>,
    ) -> Result<Heard, StoreError> {
        self.retried(|bound| {
            let event = event.clone();
            async move { bound.wakes.hear(topic, fields, event).await }
        })
        .await
    }

    pub async fn fire(
        &self,
        key: &str,
        generation: u64,
        now_ms: u64,
        submission: &Submission,
        next: Option<Wake>,
    ) -> Result<Fire, WakeError> {
        self.retried(|bound| {
            let next = next.clone();
            async move {
                bound
                    .wakes
                    .fire(key, generation, now_ms, submission, next)
                    .await
            }
        })
        .await
    }

    pub async fn wake(&self, key: &str) -> Result<Option<ArmedWake>, StoreError> {
        self.read(|state| wakes::armed_at(state, key)).await
    }

    pub async fn due_wakes(&self, now_ms: u64) -> Result<Vec<(String, ArmedWake)>, StoreError> {
        self.read(|state| wakes::due(state, now_ms)).await
    }

    pub async fn next_due(&self) -> Result<Option<u64>, StoreError> {
        self.read(|state| wakes::summary(state).next_due).await
    }
}

impl EffectGate for OneLogOver {
    fn effect(&self, tool: &str) -> EffectClass {
        self.effect_of(tool)
    }

    fn before<'a>(&'a self, call: EffectCall<'a>) -> HostFuture<'a, Result<(), String>> {
        let EffectCall {
            call_id,
            effect,
            input,
            session_id,
            tool,
            turn_id,
        } = call;
        Box::pin(self.retried(move |bound| async move {
            let call = EffectCall {
                call_id,
                effect,
                input,
                session_id,
                tool,
                turn_id,
            };
            bound.holds.before(call).await
        }))
    }

    fn settle(&self) -> HostFuture<'_, Result<(), String>> {
        Box::pin(self.retried(|bound| async move { EffectGate::settle(&bound.holds).await }))
    }

    fn abandon<'a>(
        &'a self,
        call_id: &'a str,
    ) -> HostFuture<'a, Result<Option<ToolOutput>, String>> {
        Box::pin(self.retried(move |bound| async move { bound.holds.abandon(call_id).await }))
    }

    fn after<'a>(&'a self, call_id: &'a str, output: &'a ToolOutput) -> HostFuture<'a, ()> {
        Box::pin(async move { self.current().holds.after(call_id, output).await })
    }
}

impl HoldLedger for OneLogOver {
    fn hold<'a>(
        &'a self,
        key: &'a str,
        call: HoldCall<'a>,
        card: &'a RawValue,
    ) -> HostFuture<'a, Result<(), LedgerError>> {
        let HoldCall {
            call_id,
            input,
            session_id,
            tool,
            turn_id,
        } = call;
        Box::pin(self.retried(move |bound| async move {
            let call = HoldCall {
                call_id,
                input,
                session_id,
                tool,
                turn_id,
            };
            bound.holds.hold(key, call, card).await
        }))
    }

    fn resolve<'a>(
        &'a self,
        key: &'a str,
        answer: Answer,
    ) -> HostFuture<'a, Result<Resumption, LedgerError>> {
        Box::pin(self.retried(move |bound| {
            let answer = answer.clone();
            async move { bound.holds.resolve(key, answer).await }
        }))
    }

    fn running<'a>(&'a self, call_id: &'a str) -> HostFuture<'a, Option<Running>> {
        Box::pin(async move { self.current().holds.running(call_id).await })
    }

    fn claim<'a>(&'a self, key: &'a str) -> HostFuture<'a, bool> {
        Box::pin(async move { self.current().holds.claim(key).await })
    }

    fn release<'a>(&'a self, key: &'a str) -> HostFuture<'a, Result<(), LedgerError>> {
        Box::pin(self.retried(move |bound| async move { bound.holds.release(key).await }))
    }
}

impl RemoteBarrier for OneLogOver {
    fn settle(&self) -> HostFuture<'_, Result<(), String>> {
        Box::pin(self.retried(|bound| async move { RemoteBarrier::settle(&bound.holds).await }))
    }

    fn carry(&self, carried: Carried) -> HostFuture<'_, Result<bool, String>> {
        Box::pin(async move { self.current().holds.carry(carried).await })
    }
}
