use std::sync::Arc;

use nanocode_scope::Carry;
use nanocode_sync::Atom;
use nanocode_turnlog::{ArmedWake, Record, Wake, WakeRider, WakeSummary};
use serde_json::{Map, Value, value::RawValue};

use super::{Staged, ThreadLog, ThreadState, event_at, facts::refused, record_in, sim};
use crate::{
    durability::StoreError,
    one_log::{Fire, Heard, Lifecycle, WakeError, logged, raw, record_of},
    session_log::{Submission, now, submission_digest},
};

pub struct ThreadWakes {
    log: Arc<ThreadLog>,
    thread: String,
    wakes_max: Option<usize>,
    index: Atom<Option<Arc<dyn WakeRider>>>,
}

#[must_use]
pub fn summary(state: &ThreadState) -> WakeSummary {
    let mut topics = state
        .wakes()
        .filter(|(_, armed)| !armed.full())
        .filter_map(|(_, armed)| armed.wake.on.as_ref().map(|on| on.topic.clone()))
        .collect::<Vec<_>>();
    topics.sort();
    topics.dedup();
    WakeSummary {
        next_due: state.wakes().filter_map(|(_, armed)| armed.due_at()).min(),
        topics,
    }
}

fn armed_at(state: &ThreadState, key: &str) -> Option<ArmedWake> {
    state
        .wakes()
        .find(|(held, _)| *held == key)
        .map(|(_, armed)| armed.clone())
}

fn armed(staged: &mut Staged, key: &str, wake: Wake) -> Result<u64, StoreError> {
    let generation = staged.state().generation() + 1;
    staged.push(Record::Armed {
        generation,
        key: key.to_owned(),
        wake,
    })?;
    assert_eq!(
        armed_at(staged.state(), key).map(|armed| armed.generation),
        Some(generation),
        "an armed wake holds the generation it was armed at"
    );
    Ok(generation)
}

impl ThreadWakes {
    pub fn new(
        log: Arc<ThreadLog>,
        thread: &str,
        wakes_max: Option<usize>,
    ) -> Result<Self, StoreError> {
        if log.state().is_none() {
            return Err(refused(nanocode_scope::ScopeError::Routed {
                holder: log.holder().unwrap_or_default().to_owned(),
            }));
        }
        Ok(Self {
            log,
            thread: thread.to_owned(),
            wakes_max,
            index: Atom::new(None),
        })
    }

    #[must_use]
    pub const fn log(&self) -> &Arc<ThreadLog> {
        &self.log
    }

    fn state(&self) -> Result<Arc<ThreadState>, StoreError> {
        self.log.state().ok_or_else(|| {
            refused(nanocode_scope::ScopeError::Routed {
                holder: self.log.holder().unwrap_or_default().to_owned(),
            })
        })
    }

    pub async fn index_wakes(&self, index: Arc<dyn WakeRider>) -> Result<(), StoreError> {
        index.prepare().await?;
        self.index.set(Some(Arc::clone(&index)));
        let held = self.log.job(|staged| Ok(summary(staged.state()))).await;
        index.write(&self.thread, &held.map_err(refused)?).await
    }

    async fn committed<T: Send + 'static>(
        &self,
        run: impl Fn(&mut Staged) -> Result<T, StoreError> + Send + Sync + 'static,
    ) -> Result<T, StoreError> {
        let Some(index) = self.index.read(Clone::clone) else {
            return self.log.job(run).await.map_err(refused);
        };
        let (value, written) = self
            .log
            .job_staged(move |staged| {
                let value = run(staged)?;
                let written = (staged.staged() > 0).then(|| summary(staged.state()));
                Ok((value, written))
            })
            .await
            .map_err(refused)?;
        let Some(written) = written else {
            return Ok(value);
        };
        let carry = Carry {
            host: index.host(),
            statements: index.statements(&self.thread, &written),
        };
        let flushed = self.log.flush(Some(carry)).await.map_err(refused)?;
        if flushed.carried || sim::mutated(sim::Mutation::WakeIndexSkipped) {
            return Ok(value);
        }
        let published = summary(&*self.state()?);
        if let Err(error) = index.write(&self.thread, &published).await {
            nanocode_trace::warn!(
                %error,
                scope = self.thread.as_str(),
                "a thread committed its wakes, but its host did not carry the wake index's rider and writing the index on its own failed; it stays stale until index_wakes"
            );
        }
        Ok(value)
    }

    pub async fn arm(&self, key: &str, wake: Wake) -> Result<u64, WakeError> {
        wake.fit().map_err(WakeError::Unfit)?;
        let key = key.to_owned();
        let wakes_max = self.wakes_max;
        self.committed(move |staged| {
            let held = staged.state().wakes().count();
            let prior = armed_at(staged.state(), &key);
            if let Some(max) = wakes_max
                && prior.is_none()
                && held >= max
                && !sim::mutated(sim::Mutation::WakesUnbounded)
            {
                return Ok(Err(WakeError::Full { max }));
            }
            armed(staged, &key, wake.clone()).map(Ok)
        })
        .await?
    }

    pub async fn disarm(&self, key: &str) -> Result<Option<u64>, StoreError> {
        let key = key.to_owned();
        self.committed(move |staged| {
            let Some(prior) = armed_at(staged.state(), &key) else {
                return Ok(None);
            };
            staged.push(Record::Disarmed {
                generation: prior.generation,
                key: key.clone(),
            })?;
            assert!(
                armed_at(staged.state(), &key).is_none(),
                "a disarmed wake is gone"
            );
            Ok(Some(prior.generation))
        })
        .await
    }

    pub async fn hear(
        &self,
        topic: &str,
        fields: &Map<String, Value>,
        event: Box<RawValue>,
    ) -> Result<Heard, StoreError> {
        let topic = topic.to_owned();
        let fields = fields.clone();
        self.committed(move |staged| {
            let mut heard = Heard::default();
            for (key, armed) in staged.state().wakes() {
                if !armed.hears(&topic, &fields) {
                    continue;
                }
                assert!(armed.wake.on.is_some(), "a wake that hears has a trigger");
                let found = (key.to_owned(), armed.generation);
                if armed.full() {
                    heard.full.push(found);
                } else {
                    heard.heard.push(found);
                }
            }
            for (key, generation) in &heard.heard {
                staged.push(Record::Heard {
                    event: event.clone(),
                    generation: *generation,
                    key: key.clone(),
                })?;
            }
            Ok(heard)
        })
        .await
    }

    pub fn wake(&self, key: &str) -> Result<Option<ArmedWake>, StoreError> {
        Ok(armed_at(&*self.state()?, key))
    }

    pub fn due_wakes(&self, now_ms: u64) -> Result<Vec<(String, ArmedWake)>, StoreError> {
        Ok(self
            .state()?
            .wakes()
            .filter(|(_, armed)| armed.due(now_ms))
            .map(|(key, armed)| (key.to_owned(), armed.clone()))
            .collect())
    }

    pub fn next_due(&self) -> Result<Option<u64>, StoreError> {
        Ok(summary(&*self.state()?).next_due)
    }

    pub async fn fire(
        &self,
        key: &str,
        generation: u64,
        now_ms: u64,
        submission: &Submission,
        next: Option<Wake>,
    ) -> Result<Fire, WakeError> {
        if let Some(next) = &next {
            next.fit().map_err(WakeError::Unfit)?;
        }
        let fire = Firing {
            key: key.to_owned(),
            generation,
            now_ms,
            submission: submission.clone(),
            next,
            at: now(),
        };
        Ok(self.committed(move |staged| fired(staged, &fire)).await?)
    }
}

struct Firing {
    key: String,
    generation: u64,
    now_ms: u64,
    submission: Submission,
    next: Option<Wake>,
    at: u64,
}

fn fired(staged: &mut Staged, fire: &Firing) -> Result<Fire, StoreError> {
    let (key, generation) = (fire.key.as_str(), fire.generation);
    let id = fire.submission.request_id.as_str();
    match armed_at(staged.state(), key) {
        Some(armed) if armed.generation == generation && armed.due(fire.now_ms) => {}
        Some(armed) if armed.generation == generation => return Ok(Fire::NotDue),
        _ => return Ok(Fire::Stale),
    }
    if let Some(turn) = staged.state().turn(id) {
        return Ok(Fire::Taken(record_of(turn)?));
    }
    let submission = &fire.submission;
    let first = staged.state().next_event();
    let body = raw(&Lifecycle {
        kind: "queued",
        request_id: id,
    })?;
    staged.push(Record::Admitted {
        digest: submission_digest(&submission.prompt, submission.hand.as_deref())?,
        first,
        hand: submission.hand.clone(),
        metadata: submission.metadata.clone(),
        prompt: raw(&submission.prompt)?,
        turn: id.to_owned(),
    })?;
    staged.push(event_at(fire.at, first, Some(id), body.clone()))?;
    let event = logged(first, Some(id), fire.at, body);
    staged.push(Record::Fired {
        generation,
        key: key.to_owned(),
        turn: id.to_owned(),
    })?;
    let next = match &fire.next {
        Some(wake) => Some(armed(staged, key, wake.clone())?),
        None => None,
    };
    assert_eq!(
        armed_at(staged.state(), key).map(|armed| armed.generation),
        next,
        "a fired wake is gone or rearmed at its next generation"
    );
    Ok(Fire::Fired {
        event,
        next,
        record: record_in(staged.state(), id)?,
    })
}
