use std::{collections::BTreeSet, sync::Arc};

use nanocode_scope::Carry;
use nanocode_sync::Atom;
use nanocode_tool_host::{
    ToolOutput,
    effects::{Carried, EffectCall, EffectGate, RemoteBarrier},
    embedded::HostFuture,
    hold::{HoldCall, HoldLedger, LedgerError, Running},
};
use nanocode_turnlog::{
    Answer, EffectClass, Effects, HeldCall, Hold, HoldError, Record, Resumption,
};
use serde_json::{json, value::RawValue};

use super::{Staged, ThreadLog, facts::refused, sim};
use crate::{
    durability::{
        StoreError,
        facts::{EffectResult, Fact, OwnedFact},
    },
    one_log::{UNKNOWN_OUTCOME, raw, tool_input, unknown_outcome},
};

type Decided<T> = Result<(Option<Hold>, T), HoldError>;

pub struct ThreadHolds {
    log: Arc<ThreadLog>,
    effects: Arc<Atom<Effects>>,
    resolved: Atom<BTreeSet<String>>,
    owed: Atom<bool>,
}

struct Gated {
    call_id: String,
    effect: EffectClass,
    input: Box<RawValue>,
    session_id: String,
    tool: String,
    turn: Option<String>,
}

fn gated(staged: &mut Staged, call: &Gated) -> Result<Result<(), String>, StoreError> {
    let state = staged.state();
    match state.intent(&call.call_id).map(|open| open.tool.as_str()) {
        Some(tool) if tool != call.tool => {
            return Ok(Err(format!(
                "call {} was intended for tool {tool}, not {}",
                call.call_id, call.tool
            )));
        }
        Some(_) => return Ok(Ok(())),
        None if state.logs_facts(&call.session_id) => {
            return Ok(Err(format!(
                "call {} to {} has no durable intent, and session {} commits one before each external call",
                call.call_id, call.tool, call.session_id
            )));
        }
        None => {}
    }
    staged.push(Record::Call {
        call_id: call.call_id.clone(),
        effect: call.effect,
        input: call.input.clone(),
        tool: call.tool.clone(),
        turn: call.turn.clone(),
    })?;
    Ok(Ok(()))
}

fn abandoned(staged: &mut Staged, call_id: &str) -> Result<Option<ToolOutput>, StoreError> {
    let Some(state) = staged
        .state()
        .intents()
        .find(|(open, _, _)| *open == call_id)
        .map(|(_, state, _)| state.map(str::to_owned))
    else {
        return Ok(None);
    };
    let result: OwnedFact<Box<RawValue>> = Fact::EffectResult(EffectResult {
        call_id: call_id.to_owned(),
        output: unknown_outcome()?,
    });
    staged.fact(&result, state)?;
    Ok(Some(ToolOutput::error(UNKNOWN_OUTCOME)))
}

impl ThreadHolds {
    pub fn new(log: Arc<ThreadLog>, effects: Effects) -> Result<Self, StoreError> {
        if log.state().is_none() {
            return Err(refused(nanocode_scope::ScopeError::Routed {
                holder: log.holder().unwrap_or_default().to_owned(),
            }));
        }
        Ok(Self {
            log,
            effects: Arc::new(Atom::new(Effects::builtin().merged(effects))),
            resolved: Atom::default(),
            owed: Atom::new(false),
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
            effects: Arc::clone(&self.effects),
            resolved: Atom::new(self.resolved.read(Clone::clone)),
            owed: Atom::new(false),
        })
    }

    #[must_use]
    pub const fn log(&self) -> &Arc<ThreadLog> {
        &self.log
    }

    #[must_use]
    pub fn effects(&self) -> Arc<Atom<Effects>> {
        Arc::clone(&self.effects)
    }

    pub fn declare(&self, tool: impl Into<String>, class: EffectClass) {
        let tool = tool.into();
        self.effects
            .update(|effects| effects.insert(tool.clone(), class));
    }

    #[must_use]
    pub fn effect_of(&self, tool: &str) -> EffectClass {
        self.effects.read(|effects| effects.of(tool))
    }

    pub fn owe(&self) {
        self.owed.set(true);
    }

    #[must_use]
    pub fn uncertain_holds(&self) -> Vec<String> {
        let resolved = self.resolved.get();
        self.log.state().map_or_else(Vec::new, |state| {
            state
                .holds()
                .filter(|(key, hold)| {
                    matches!(hold, Hold::Running { .. }) && !resolved.contains(*key)
                })
                .map(|(key, _)| key.to_owned())
                .collect()
        })
    }

    async fn journal<T: Send + 'static>(
        &self,
        key: &str,
        next: impl Fn(Option<&Hold>) -> Decided<T> + Send + Sync + 'static,
    ) -> Result<T, LedgerError> {
        let key = key.to_owned();
        let job = move |staged: &mut Staged| {
            let (hold, decided) = match next(staged.state().hold(&key)) {
                Ok(decided) => decided,
                Err(error) => return Ok(Err(error)),
            };
            if let Some(hold) = hold {
                staged.push(Record::Hold {
                    hold,
                    key: key.clone(),
                })?;
            }
            Ok(Ok(decided))
        };
        let journaled = if sim::mutated(sim::Mutation::HoldStaged) {
            self.log.job_staged(job).await
        } else {
            self.log.job(job).await
        };
        journaled
            .map_err(|error| LedgerError::Journal(refused(error).to_string()))?
            .map_err(LedgerError::Hold)
    }

    async fn settled(&self) -> Result<(), String> {
        if *self.owed.get() {
            self.log
                .flush(None)
                .await
                .map_err(|error| refused(error).to_string())?;
        }
        self.owed.set(false);
        Ok(())
    }

    async fn carried(&self, carried: Carried) -> Result<bool, String> {
        if !*self.owed.get() {
            return Ok(false);
        }
        let carry = Carry {
            host: carried.host,
            statements: carried.statements,
        };
        let flushed = self
            .log
            .flush(Some(carry))
            .await
            .map_err(|error| refused(error).to_string())?;
        self.owed.set(false);
        Ok(flushed.carried)
    }

    async fn before_call(&self, call: EffectCall<'_>) -> Result<(), String> {
        let gated_call = Gated {
            call_id: call.call_id.to_owned(),
            effect: call.effect,
            input: tool_input(call.input).map_err(|error| error.to_string())?,
            session_id: call.session_id.to_owned(),
            tool: call.tool.to_owned(),
            turn: call.turn_id.map(str::to_owned),
        };
        self.log
            .job(move |staged| gated(staged, &gated_call))
            .await
            .map_err(|error| refused(error).to_string())??;
        self.owed.set(false);
        Ok(())
    }

    async fn after_call(&self, call_id: &str, output: &ToolOutput) {
        let Ok(output) = raw(&json!({"success": output.success})) else {
            return;
        };
        let id = call_id.to_owned();
        let recorded = self
            .log
            .job_staged(move |staged| {
                if staged.state().intent(&id).is_some() {
                    return Ok(());
                }
                staged.push(Record::Result {
                    call_id: id.clone(),
                    output: output.clone(),
                })
            })
            .await;
        if let Err(error) = recorded {
            nanocode_trace::warn!(%error, call_id, "a thread could not record a tool result");
        }
    }
}

impl EffectGate for ThreadHolds {
    fn effect(&self, tool: &str) -> EffectClass {
        self.effect_of(tool)
    }

    fn before<'a>(&'a self, call: EffectCall<'a>) -> HostFuture<'a, Result<(), String>> {
        Box::pin(self.before_call(call))
    }

    fn settle(&self) -> HostFuture<'_, Result<(), String>> {
        Box::pin(self.settled())
    }

    fn abandon<'a>(
        &'a self,
        call_id: &'a str,
    ) -> HostFuture<'a, Result<Option<ToolOutput>, String>> {
        let id = call_id.to_owned();
        Box::pin(async move {
            self.log
                .job(move |staged| abandoned(staged, &id))
                .await
                .map_err(|error| refused(error).to_string())
        })
    }

    fn after<'a>(&'a self, call_id: &'a str, output: &'a ToolOutput) -> HostFuture<'a, ()> {
        Box::pin(self.after_call(call_id, output))
    }
}

impl HoldLedger for ThreadHolds {
    fn hold<'a>(
        &'a self,
        key: &'a str,
        call: HoldCall<'a>,
        card: &'a RawValue,
    ) -> HostFuture<'a, Result<(), LedgerError>> {
        let owned = key.to_owned();
        let call_id = call.call_id.to_owned();
        let card = card.to_owned();
        let input = call.input.to_owned();
        let tool = call.tool.to_owned();
        let turn = call.turn_id.map(str::to_owned);
        Box::pin(self.journal(key, move |existing| {
            let held = HeldCall {
                call_id: &call_id,
                card: &card,
                input: &input,
                tool: &tool,
                turn: turn.as_deref(),
            };
            Hold::hold(existing, &owned, &held).map(|hold| (hold, ()))
        }))
    }

    fn resolve<'a>(
        &'a self,
        key: &'a str,
        answer: Answer,
    ) -> HostFuture<'a, Result<Resumption, LedgerError>> {
        Box::pin(async move {
            let owned = key.to_owned();
            let resumption = self
                .journal(key, move |existing| {
                    let (hold, resumed) = Hold::resolve(existing, &owned, answer.clone())?;
                    Ok((Some(hold), resumed))
                })
                .await?;
            if matches!(resumption, Resumption::Run { .. }) {
                self.resolved.update(|resolved| {
                    resolved.insert(key.to_owned());
                });
            }
            Ok(resumption)
        })
    }

    fn running<'a>(&'a self, call_id: &'a str) -> HostFuture<'a, Option<Running>> {
        let resolved = self.resolved.get();
        let state = self.log.state();
        Box::pin(async move {
            let state = state?;
            let (key, hold) = state.holds().find(|(_, hold)| {
                matches!(hold, Hold::Running { .. }) && hold.call_id() == call_id
            })?;
            let Hold::Running { input, .. } = hold else {
                unreachable!("only a running hold is found");
            };
            resolved.contains(key).then(|| Running {
                input: input.clone(),
                key: key.to_owned(),
            })
        })
    }

    fn claim<'a>(&'a self, key: &'a str) -> HostFuture<'a, bool> {
        Box::pin(async move { self.resolved.update(|resolved| resolved.remove(key)) })
    }

    fn release<'a>(&'a self, key: &'a str) -> HostFuture<'a, Result<(), LedgerError>> {
        let owned = key.to_owned();
        Box::pin(self.journal(key, move |existing| {
            Hold::release(existing, &owned).map(|hold| (Some(hold), ()))
        }))
    }
}

impl RemoteBarrier for ThreadHolds {
    fn settle(&self) -> HostFuture<'_, Result<(), String>> {
        Box::pin(self.settled())
    }

    fn carry(&self, carried: Carried) -> HostFuture<'_, Result<bool, String>> {
        Box::pin(self.carried(carried))
    }
}
