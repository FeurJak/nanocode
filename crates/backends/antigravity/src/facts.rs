use std::{future::Future, pin::Pin, sync::Arc};

use nanocode_agent_core::{NanocodeError, Result};
use nanocode_durability::facts::{FactLog, Folded, OwnedFact, commit_fact, fold_facts};
use nanocode_sync::{future, mpsc, oneshot};

use crate::fold::{AntigravityDecision, AntigravityFold};

#[cfg(not(target_family = "wasm"))]
type Work<'a, R> = Pin<Box<dyn Future<Output = R> + Send + 'a>>;
#[cfg(target_family = "wasm")]
type Work<'a, R> = Pin<Box<dyn Future<Output = R> + 'a>>;

type Job = Box<dyn for<'a> FnOnce(&'a mut Owner) -> Work<'a, ()> + Send>;

pub(crate) struct AntigravityFacts {
    jobs: mpsc::UnboundedSender<Job>,
}

struct Owner {
    log: Arc<dyn FactLog>,
    folded: Option<Folded<AntigravityFold>>,
}

fn store_error(error: nanocode_durability::StoreError) -> NanocodeError {
    nanocode_durability::agent_store_error(error)
}

fn stopped() -> NanocodeError {
    NanocodeError::InvalidExecutionPolicy("the antigravity fact log's owning task stopped".into())
}

async fn serve(mut owner: Owner, mut jobs: mpsc::UnboundedReceiver<Job>) {
    while let Some(job) = jobs.recv().await {
        job(&mut owner).await;
    }
    if let Some(folded) = owner.folded.take()
        && let Ok(snapshot) = folded.snapshot()
    {
        owner.log.remember(snapshot);
    }
}

#[cfg(not(target_family = "wasm"))]
fn spawn(owner: Owner) -> Result<mpsc::UnboundedSender<Job>> {
    let runtime = nanocode_platform::Handle::try_current()
        .map_err(|error| NanocodeError::InvalidExecutionPolicy(error.to_string()))?;
    let (jobs, received) = mpsc::unbounded_channel();
    runtime.spawn(serve(owner, received));
    Ok(jobs)
}

#[cfg(target_family = "wasm")]
fn spawn(owner: Owner) -> Result<mpsc::UnboundedSender<Job>> {
    let (jobs, received) = mpsc::unbounded_channel();
    nanocode_platform::spawn_local(serve(owner, received));
    Ok(jobs)
}

impl Owner {
    async fn taken(&mut self) -> Result<Folded<AntigravityFold>> {
        if let Some(folded) = self.folded.take() {
            return Ok(folded);
        }
        fold_facts::<AntigravityFold>(&*self.log)
            .await
            .map_err(store_error)
    }

    async fn read<R>(&mut self, read: impl FnOnce(&AntigravityFold) -> R) -> Result<R> {
        let folded = self.taken().await?;
        let value = read(folded.state());
        self.folded = Some(folded);
        Ok(value)
    }

    async fn commit(&mut self, fact: OwnedFact<AntigravityDecision>) -> Result<u64> {
        let folded = self.taken().await?;
        let (folded, seq) = commit_fact(&*self.log, folded, fact)
            .await
            .map_err(store_error)?;
        self.folded = Some(folded);
        Ok(seq)
    }

    async fn refresh(&mut self) -> Result<()> {
        let folded = fold_facts::<AntigravityFold>(&*self.log)
            .await
            .map_err(store_error)?;
        self.folded = Some(folded);
        Ok(())
    }
}

impl AntigravityFacts {
    pub(crate) async fn open(log: Arc<dyn FactLog>) -> Result<Self> {
        let folded = fold_facts::<AntigravityFold>(&*log)
            .await
            .map_err(store_error)?;
        let jobs = spawn(Owner {
            log,
            folded: Some(folded),
        })?;
        Ok(Self { jobs })
    }

    async fn run<R: Send + 'static>(
        &self,
        work: impl for<'a> FnOnce(&'a mut Owner) -> Work<'a, R> + Send + 'static,
    ) -> Result<R> {
        let (mut reply, replied) = oneshot::channel();
        let job: Job = Box::new(move |owner| {
            Box::pin(async move {
                if reply.is_closed() {
                    return;
                }
                let work = work(owner);
                let output = {
                    let abandoned = Box::pin(reply.closed());
                    match future::select(work, abandoned).await {
                        future::Either::Left((output, _)) => Some(output),
                        future::Either::Right(_) => None,
                    }
                };
                if let Some(output) = output {
                    drop(reply.send(output));
                }
            })
        });
        self.jobs.send(job).map_err(|_| stopped())?;
        replied.await.map_err(|_| stopped())
    }

    pub(crate) async fn read<R: Send + 'static>(
        &self,
        read: impl FnOnce(&AntigravityFold) -> R + Send + 'static,
    ) -> Result<R> {
        self.run(move |owner| Box::pin(owner.read(read))).await?
    }

    pub(crate) async fn seq(&self) -> Result<u64> {
        self.run(|owner| {
            Box::pin(async move {
                let folded = owner.taken().await?;
                let seq = folded.seq();
                owner.folded = Some(folded);
                Ok(seq)
            })
        })
        .await?
    }

    pub(crate) async fn commit(&self, fact: OwnedFact<AntigravityDecision>) -> Result<u64> {
        self.run(move |owner| Box::pin(owner.commit(fact))).await?
    }

    pub(crate) async fn refresh(&self) -> Result<()> {
        self.run(|owner| Box::pin(owner.refresh())).await?
    }
}
