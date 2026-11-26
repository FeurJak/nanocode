use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use nanocode_durability_core::StoreError;
use nanocode_partitions::Partition;
use nanocode_sync::{Atom, Slot, Vector, oneshot, watch};

use crate::{
    Committed, Resident, ScopeError,
    checkpoint::{Fence, Listing, Manifest, Written, digest},
    scopes::Inner,
    sim::{self, Mutation},
};

pub(crate) enum Ask<F: Resident> {
    Send(F::Command),
    #[cfg_attr(target_family = "wasm", allow(dead_code))]
    Read,
    Checkpoint,
    Drain(Why),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    Release,
    Evict,
}

pub(crate) enum Turn<F: Resident> {
    Committed(Result<Committed<F::Reply>, ScopeError>),
    Moved(Option<F::Command>),
    #[cfg_attr(target_family = "wasm", allow(dead_code))]
    Read(Arc<F::State>),
    Checkpointed(Result<(), ScopeError>),
    Drained(Result<u64, ScopeError>),
}

struct Job<F: Resident> {
    ask: Ask<F>,
    reply: oneshot::Sender<Turn<F>>,
}

struct Queued<F: Resident>(Arc<Slot<Job<F>>>);

impl<F: Resident> Clone for Queued<F> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<F: Resident> Queued<F> {
    fn take(&self) -> Job<F> {
        let Some(job) = self.0.take() else {
            panic!("a queued scope job leaves its queue once");
        };
        job
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Life {
    Serving,
    Draining,
    Released { seq: u64 },
    Lost,
}

struct Queue<F: Resident> {
    life: Life,
    leading: bool,
    waiting: Vector<Queued<F>>,
}

impl<F: Resident> Clone for Queue<F> {
    fn clone(&self) -> Self {
        Self {
            life: self.life,
            leading: self.leading,
            waiting: self.waiting.clone(),
        }
    }
}

struct Published<F: Resident> {
    state: Arc<F::State>,
    next: u64,
}

impl<F: Resident> Clone for Published<F> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            next: self.next,
        }
    }
}

pub(crate) struct Checkpointed {
    pub(crate) next: u64,
    pub(crate) at_ms: u64,
    pub(crate) parts: BTreeMap<String, String>,
    pub(crate) chain: Option<Chain>,
}

#[derive(Clone, Copy)]
pub(crate) struct Chain {
    pub(crate) base: u64,
    pub(crate) whole_bytes: usize,
    pub(crate) changed_bytes: usize,
}

impl Checkpointed {
    pub(crate) fn empty(at_ms: u64) -> Self {
        Self {
            next: 0,
            at_ms,
            parts: BTreeMap::new(),
            chain: None,
        }
    }

    fn listing(&self, named: &[(String, String)], next: u64) -> (Listing, Chain) {
        let whole = Listing::Whole(named.to_vec());
        let whole_bytes = whole.encoded().len();
        let fresh = Chain {
            base: next,
            whole_bytes,
            changed_bytes: 0,
        };
        let Some(chain) = self.chain else {
            return (whole, fresh);
        };
        let put: Vec<(String, String)> = named
            .iter()
            .filter(|(name, at)| self.parts.get(name) != Some(at))
            .cloned()
            .collect();
        let kept: BTreeSet<&str> = named.iter().map(|(name, _)| name.as_str()).collect();
        let dropped: Vec<String> = if sim::mutated(Mutation::ListingSkipsDropped) {
            Vec::new()
        } else {
            self.parts
                .keys()
                .filter(|name| !kept.contains(name.as_str()))
                .cloned()
                .collect()
        };
        let changed = Listing::Changed {
            base: chain.base,
            put,
            dropped,
        };
        let changed_bytes = chain.changed_bytes + changed.encoded().len();
        let bounded = changed_bytes <= chain.whole_bytes
            || sim::mutated(Mutation::ListingChainUnbounded);
        if !bounded {
            return (whole, fresh);
        }
        (
            changed,
            Chain {
                base: next,
                whole_bytes: chain.whole_bytes,
                changed_bytes,
            },
        )
    }
}

pub(crate) struct Core<F: Resident> {
    pub(crate) state: Arc<F::State>,
    pub(crate) next: u64,
    pub(crate) since: usize,
    pub(crate) checkpointed: Checkpointed,
}

pub(crate) struct Owner<F: Resident> {
    pub(crate) scope: Arc<str>,
    pub(crate) part: Partition,
    pub(crate) epoch: u64,
    published: Atom<Published<F>>,
    watch: watch::Sender<Arc<F::State>>,
    queue: Atom<Queue<F>>,
    core: Slot<Core<F>>,
    pub(crate) used: AtomicU64,
    weight: AtomicUsize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Send,
    Read,
    Checkpoint,
    Drain,
}

const fn kind<F: Resident>(ask: &Ask<F>) -> Kind {
    match ask {
        Ask::Send(_) => Kind::Send,
        Ask::Read => Kind::Read,
        Ask::Checkpoint => Kind::Checkpoint,
        Ask::Drain(_) => Kind::Drain,
    }
}

const fn admits(life: Life, kind: Kind) -> bool {
    match life {
        Life::Serving => true,
        Life::Draining => matches!(kind, Kind::Read),
        Life::Released { .. } | Life::Lost => false,
    }
}

fn refused<F: Resident>(ask: Ask<F>) -> Turn<F> {
    match ask {
        Ask::Send(command) => Turn::Moved(Some(command)),
        Ask::Read | Ask::Checkpoint | Ask::Drain(_) => Turn::Moved(None),
    }
}

struct Leading<'a, F: Resident> {
    owner: &'a Owner<F>,
    core: Option<Core<F>>,
    in_flight: bool,
    done: bool,
}

impl<F: Resident> Drop for Leading<'_, F> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if let Some(core) = self.core.take() {
            self.owner.core.put(core);
        }
        let in_flight = self.in_flight;
        let waiting = self.owner.queue.update(|queue| {
            if in_flight {
                queue.life = Life::Lost;
            }
            queue.leading = false;
            std::mem::take(&mut queue.waiting)
        });
        for queued in &waiting {
            let job = queued.take();
            drop(job.reply.send(refused(job.ask)));
        }
    }
}

impl<F: Resident> Owner<F> {
    pub(crate) fn new(
        scope: Arc<str>,
        part: Partition,
        epoch: u64,
        core: Core<F>,
        weight: usize,
        used: u64,
    ) -> Self {
        let published = Published {
            state: Arc::clone(&core.state),
            next: core.next,
        };
        Self {
            scope,
            part,
            epoch,
            watch: watch::Sender::new(Arc::clone(&core.state)),
            published: Atom::new(published),
            queue: Atom::new(Queue {
                life: Life::Serving,
                leading: false,
                waiting: Vector::new(),
            }),
            core: Slot::new(Some(core)),
            used: AtomicU64::new(used),
            weight: AtomicUsize::new(weight),
        }
    }

    pub(crate) fn state(&self) -> Arc<F::State> {
        self.published
            .read(|published| Arc::clone(&published.state))
    }

    pub(crate) fn next(&self) -> u64 {
        self.published.read(|published| published.next)
    }

    pub(crate) fn watch(&self) -> watch::Receiver<Arc<F::State>> {
        self.watch.subscribe()
    }

    pub(crate) fn life(&self) -> Life {
        self.queue.read(|queue| queue.life)
    }

    pub(crate) fn weight(&self) -> usize {
        self.weight.load(Ordering::Relaxed)
    }

    pub(crate) fn idle(&self) -> bool {
        self.queue
            .read(|queue| !queue.leading && queue.waiting.is_empty())
    }

    pub(crate) fn lose(&self) {
        self.queue.update(|queue| queue.life = Life::Lost);
    }

    fn publish(&self, state: &Arc<F::State>, next: u64) {
        self.published.set(Published {
            state: Arc::clone(state),
            next,
        });
        self.watch.send_replace(Arc::clone(state));
    }

    pub(crate) async fn ask(&self, ask: Ask<F>, inner: &Inner<F>) -> Turn<F> {
        let asked = kind(&ask);
        let (reply, answer) = oneshot::channel();
        let queued = Queued(Arc::new(Slot::new(Some(Job { ask, reply }))));
        let admitted = self.queue.update(|queue| {
            if asked == Kind::Drain && queue.life == Life::Serving {
                queue.life = Life::Draining;
                queue.waiting.push_back(queued.clone());
                return Some(!std::mem::replace(&mut queue.leading, true));
            }
            if !admits(queue.life, asked) {
                return None;
            }
            queue.waiting.push_back(queued.clone());
            Some(!std::mem::replace(&mut queue.leading, true))
        });
        let Some(lead) = admitted else {
            return refused(queued.take().ask);
        };
        if lead {
            self.lead(inner).await;
        }
        match answer.await {
            Ok(turn) => turn,
            Err(_) if asked == Kind::Send => Turn::Committed(Err(ScopeError::Uncertain)),
            Err(_) => Turn::Moved(None),
        }
    }

    async fn lead(&self, inner: &Inner<F>) {
        let Some(core) = self.core.take() else {
            panic!(
                "the leader of scope {} finds its state in place",
                self.scope
            );
        };
        let mut leading = Leading {
            owner: self,
            core: Some(core),
            in_flight: false,
            done: false,
        };
        loop {
            if let Some(core) = leading.core.take() {
                self.core.put(core);
            }
            let waiting = self.queue.update(|queue| {
                let waiting = std::mem::take(&mut queue.waiting);
                if waiting.is_empty() {
                    queue.leading = false;
                }
                waiting
            });
            if waiting.is_empty() {
                leading.done = true;
                return;
            }
            leading.core = self.core.take();
            assert!(
                leading.core.is_some(),
                "a leader takes back the state it put down"
            );
            let jobs: Vec<Job<F>> = waiting.iter().map(Queued::take).collect();
            self.serve(&mut leading, jobs, inner).await;
        }
    }

    async fn serve(&self, leading: &mut Leading<'_, F>, jobs: Vec<Job<F>>, inner: &Inner<F>) {
        let mut batch = Vec::new();
        for job in jobs {
            match job.ask {
                Ask::Send(command) => batch.push((command, job.reply)),
                Ask::Read => {
                    drop(job.reply.send(Turn::Read(self.state())));
                }
                Ask::Checkpoint => {
                    self.commit(leading, std::mem::take(&mut batch), inner)
                        .await;
                    let due = self.due(leading, inner);
                    let checkpointed = match (due, leading.core.as_mut()) {
                        (true, Some(core)) => self.checkpoint(core, inner).await,
                        _ => Ok(()),
                    };
                    drop(job.reply.send(Turn::Checkpointed(checkpointed)));
                }
                Ask::Drain(why) => {
                    self.commit(leading, std::mem::take(&mut batch), inner)
                        .await;
                    let drained = self.drain(leading, why, inner).await;
                    drop(job.reply.send(Turn::Drained(drained)));
                }
            }
        }
        self.commit(leading, batch, inner).await;
    }

    fn due(&self, leading: &Leading<'_, F>, inner: &Inner<F>) -> bool {
        leading.core.as_ref().is_some_and(|core| {
            core.next != core.checkpointed.next
                && (inner.clock)().saturating_sub(core.checkpointed.at_ms)
                    >= inner.policy.checkpoint_ms
        }) && self.life() == Life::Serving
    }

    async fn drain(
        &self,
        leading: &mut Leading<'_, F>,
        why: Why,
        inner: &Inner<F>,
    ) -> Result<u64, ScopeError> {
        let Some(core) = leading.core.as_mut() else {
            panic!("a draining leader of scope {} holds its state", self.scope);
        };
        if self.life() != Life::Draining {
            return Err(ScopeError::Lost);
        }
        let skip = why == Why::Evict && sim::mutated(Mutation::EvictWithoutCheckpoint);
        if !skip && let Err(error) = self.checkpoint(core, inner).await {
            self.queue.update(|queue| {
                if queue.life == Life::Draining {
                    queue.life = Life::Serving;
                }
            });
            return Err(error);
        }
        let seq = core.next;
        self.queue
            .update(|queue| queue.life = Life::Released { seq });
        Ok(seq)
    }

    pub(crate) async fn checkpoint(
        &self,
        core: &mut Core<F>,
        inner: &Inner<F>,
    ) -> Result<(), ScopeError> {
        if core.next == core.checkpointed.next {
            core.since = 0;
            return Ok(());
        }
        let rewrite = sim::mutated(Mutation::CheckpointRewritesUnchangedParts);
        let parts = inner.fold.encode(&core.state);
        let mut names = BTreeSet::new();
        let mut digests = BTreeSet::new();
        let held: BTreeSet<&String> = core.checkpointed.parts.values().collect();
        let mut named = Vec::with_capacity(parts.len());
        let mut fresh = Vec::new();
        let mut kept = 0_u64;
        for part in parts {
            assert!(
                names.insert(part.name.clone()),
                "scope {} encodes part {} once",
                self.scope,
                part.name
            );
            let at = digest(&part.body);
            if digests.insert(at.clone()) {
                if rewrite || !held.contains(&at) {
                    fresh.push((at.clone(), part.body));
                } else {
                    kept += 1;
                }
            }
            named.push((part.name, at));
        }
        drop(held);
        let (listing, chain) = core.checkpointed.listing(&named, core.next);
        let manifest = Manifest {
            scope: self.scope.as_ref().to_owned(),
            epoch: self.epoch,
            seq: core.next,
            parts: named,
            changed_bytes: chain.changed_bytes,
        };
        let fence = Fence {
            part: self.part,
            epoch: self.epoch,
            holder: inner.leases.node().to_owned(),
        };
        let written = inner
            .checkpoints
            .write(&fence, &manifest, &listing, &fresh)
            .await;
        match written {
            Ok(Written::Landed) => {
                let bytes: usize = fresh.iter().map(|(_, body)| body.len()).sum();
                inner.stats.update(|stats| {
                    stats.checkpoints += 1;
                    stats.checkpoint_parts += fresh.len() as u64;
                    stats.checkpoint_parts_kept += kept;
                    stats.checkpoint_part_bytes += bytes as u64;
                    stats.checkpoint_listing_bytes += listing.encoded().len() as u64;
                    if matches!(listing, Listing::Whole(_)) {
                        stats.checkpoint_listings_whole += 1;
                    }
                });
                core.checkpointed = Checkpointed {
                    next: core.next,
                    at_ms: (inner.clock)(),
                    parts: manifest.parts.into_iter().collect(),
                    chain: Some(chain),
                };
                core.since = 0;
                Ok(())
            }
            Ok(Written::Stale) => {
                self.lose();
                Err(ScopeError::Lost)
            }
            Err(error) => {
                inner.stats.update(|stats| stats.checkpoint_failures += 1);
                Err(error.into())
            }
        }
    }

    async fn commit(
        &self,
        leading: &mut Leading<'_, F>,
        mut batch: Vec<(F::Command, oneshot::Sender<Turn<F>>)>,
        inner: &Inner<F>,
    ) {
        while !batch.is_empty() {
            batch = self.commit_once(leading, batch, inner).await;
        }
    }

    fn movable(&self, inner: &Inner<F>) -> bool {
        let life = self.life();
        if !matches!(life, Life::Serving | Life::Draining) {
            return true;
        }
        if inner.leases.held(self.part) != Some(self.epoch) {
            self.lose();
            return true;
        }
        false
    }

    async fn commit_once(
        &self,
        leading: &mut Leading<'_, F>,
        batch: Vec<(F::Command, oneshot::Sender<Turn<F>>)>,
        inner: &Inner<F>,
    ) -> Vec<(F::Command, oneshot::Sender<Turn<F>>)> {
        if self.movable(inner) {
            for (command, reply) in batch {
                drop(reply.send(Turn::Moved(Some(command))));
            }
            return Vec::new();
        }
        let limit = inner.policy.checkpoint_rows;
        let Some(core) = leading.core.as_mut() else {
            panic!(
                "a committing leader of scope {} holds its state",
                self.scope
            );
        };
        if core.since >= limit
            && let Err(error) = self.checkpoint(core, inner).await
        {
            for (_, reply) in batch {
                drop(reply.send(Turn::Committed(Err(error.clone()))));
            }
            return Vec::new();
        }
        let room = limit - core.since;
        let mut working = (*core.state).clone();
        let mut next = core.next;
        let mut rows: Vec<(u64, String)> = Vec::new();
        let mut answers = Vec::new();
        let mut rest = Vec::new();
        for (command, reply) in batch {
            if !rest.is_empty() {
                rest.push((command, reply));
                continue;
            }
            let (bodies, answer) = inner.fold.decide(&working, &command);
            if bodies.len() > limit {
                let bound = ScopeError::Bound {
                    bound: "checkpoint_rows",
                    limit,
                    asked: bodies.len(),
                };
                drop(reply.send(Turn::Committed(Err(bound))));
                continue;
            }
            if rows.len() + bodies.len() > room {
                rest.push((command, reply));
                continue;
            }
            for body in bodies {
                if let Err(error) = inner.fold.apply(&mut working, next, &body) {
                    panic!(
                        "scope {} decided row {next} that its own fold refuses: {error}",
                        self.scope
                    );
                }
                rows.push((next, body));
                next += 1;
            }
            answers.push((reply, next, answer));
        }
        if rows.is_empty() {
            for (reply, seq, answer) in answers {
                drop(reply.send(Turn::Committed(Ok(Committed { seq, reply: answer }))));
            }
            if !rest.is_empty()
                && let Err(error) = self.checkpoint(core, inner).await
            {
                for (_, reply) in rest {
                    drop(reply.send(Turn::Committed(Err(error.clone()))));
                }
                return Vec::new();
            }
            return rest;
        }
        let working = Arc::new(working);
        if sim::mutated(Mutation::PublishBeforeCommit) {
            self.publish(&working, next);
        }
        leading.in_flight = true;
        let appended = inner.log.append(&self.scope, &rows).await;
        leading.in_flight = false;
        let failed = match appended {
            Ok(()) => None,
            Err(StoreError::Busy) => Some(ScopeError::Busy),
            Err(StoreError::Fenced | StoreError::Conflict { .. }) => {
                self.lose();
                Some(ScopeError::Lost)
            }
            Err(_) => {
                self.lose();
                Some(ScopeError::Uncertain)
            }
        };
        if let Some(error) = failed {
            for (reply, _, _) in answers {
                drop(reply.send(Turn::Committed(Err(error.clone()))));
            }
            for (command, reply) in rest {
                drop(reply.send(Turn::Moved(Some(command))));
            }
            return Vec::new();
        }
        let Some(core) = leading.core.as_mut() else {
            panic!(
                "a committing leader of scope {} holds its state",
                self.scope
            );
        };
        core.state = working;
        core.next = next;
        core.since += rows.len();
        self.weight
            .store(inner.fold.weight(&core.state), Ordering::Relaxed);
        self.publish(&core.state, next);
        inner.stats.update(|stats| {
            stats.commits += 1;
            stats.rows += rows.len() as u64;
        });
        for (reply, seq, answer) in answers {
            drop(reply.send(Turn::Committed(Ok(Committed { seq, reply: answer }))));
        }
        if self.due(leading, inner)
            && let Some(core) = leading.core.as_mut()
        {
            let _ = self.checkpoint(core, inner).await;
        }
        rest
    }
}
