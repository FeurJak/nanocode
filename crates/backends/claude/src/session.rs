use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use nanocode_agent_core::{NanocodeError, Result};
use nanocode_platform::MaybeSend;
use nanocode_sync::future::{self, Either};
use nanocode_sync::{FutureExt as _, mpsc, oneshot};
use nanocode_trace::Instrument as _;

use crate::sim::{Mutation, mutated};

#[cfg(not(target_family = "wasm"))]
#[doc(hidden)]
pub type Work<R> = Pin<Box<dyn Future<Output = R> + Send>>;
#[cfg(target_family = "wasm")]
#[doc(hidden)]
pub type Work<R> = Pin<Box<dyn Future<Output = R>>>;

type Admitted = Box<dyn FnOnce() -> Work<()> + Send>;
type Persisted = Box<dyn FnOnce(u64) -> Work<bool> + Send>;
type Reply<R> = oneshot::Sender<std::thread::Result<R>>;

#[doc(hidden)]
pub struct Session {
    admissions: mpsc::UnboundedSender<Admitted>,
    persists: mpsc::UnboundedSender<Persisted>,
    revision: Arc<AtomicU64>,
}

struct Lanes {
    admissions: mpsc::UnboundedReceiver<Admitted>,
    persists: mpsc::UnboundedReceiver<Persisted>,
    admissions_closed: bool,
    persists_closed: bool,
    admitting: Option<Work<()>>,
    persisting: Option<Work<bool>>,
    revision: u64,
    published: Arc<AtomicU64>,
}

impl Lanes {
    fn poll_admission(&mut self, context: &mut Context<'_>) -> bool {
        if self.admitting.is_none() && !self.admissions_closed {
            match self.admissions.poll_recv(context) {
                Poll::Ready(Some(job)) => self.admitting = Some(job()),
                Poll::Ready(None) => self.admissions_closed = true,
                Poll::Pending => {}
            }
        }
        let Some(work) = self.admitting.as_mut() else {
            return false;
        };
        if work.as_mut().poll(context).is_pending() {
            return false;
        }
        self.admitting = None;
        true
    }

    fn poll_persistence(&mut self, context: &mut Context<'_>) -> bool {
        if self.persisting.is_none() && !self.persists_closed {
            match self.persists.poll_recv(context) {
                Poll::Ready(Some(job)) => self.persisting = Some(job(self.revision)),
                Poll::Ready(None) => self.persists_closed = true,
                Poll::Pending => {}
            }
        }
        let Some(work) = self.persisting.as_mut() else {
            return false;
        };
        let Poll::Ready(ran) = work.as_mut().poll(context) else {
            return false;
        };
        self.persisting = None;
        if ran {
            self.revision = self
                .revision
                .checked_add(1)
                .expect("a session boundary revision never wraps");
            self.published.store(self.revision, Ordering::Release);
        }
        true
    }

    fn poll_lanes(&mut self, context: &mut Context<'_>) -> Poll<()> {
        loop {
            let admitted = self.poll_admission(context);
            let persisted = self.poll_persistence(context);
            if admitted || persisted {
                continue;
            }
            let closed = self.admissions_closed && self.persists_closed;
            if closed && self.admitting.is_none() && self.persisting.is_none() {
                return Poll::Ready(());
            }
            return Poll::Pending;
        }
    }
}

async fn serve(mut lanes: Lanes) {
    std::future::poll_fn(|context| lanes.poll_lanes(context)).await;
}

#[cfg(not(target_family = "wasm"))]
fn start(lanes: Lanes) -> Result<()> {
    nanocode_platform::Handle::try_current()
        .map_err(|error| NanocodeError::InvalidRequest(error.to_string()))?
        .spawn(serve(lanes));
    Ok(())
}

#[cfg(target_family = "wasm")]
fn start(lanes: Lanes) -> Result<()> {
    nanocode_platform::spawn_local(serve(lanes));
    Ok(())
}

async fn answered<R>(sent: bool, replied: oneshot::Receiver<std::thread::Result<R>>) -> Result<R> {
    if !sent {
        return Err(NanocodeError::AgentStopped);
    }
    match replied.await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => Err(NanocodeError::AgentStopped),
    }
}

async fn served<R>(mut reply: Reply<R>, work: Work<R>, answering: impl FnOnce()) -> bool {
    let work = AssertUnwindSafe(work).catch_unwind();
    let output = {
        let abandoned = Box::pin(reply.closed());
        match future::select(Box::pin(work), abandoned).await {
            Either::Left((output, _)) => Some(output),
            Either::Right(_) => None,
        }
    };
    let Some(output) = output else {
        return false;
    };
    answering();
    drop(reply.send(output));
    true
}

impl Session {
    #[doc(hidden)]
    pub fn spawn() -> Result<Self> {
        let (admissions, admitted) = mpsc::unbounded_channel();
        let (persists, persisted) = mpsc::unbounded_channel();
        let revision = Arc::new(AtomicU64::new(0));
        start(Lanes {
            admissions: admitted,
            persists: persisted,
            admissions_closed: false,
            persists_closed: false,
            admitting: None,
            persisting: None,
            revision: 0,
            published: Arc::clone(&revision),
        })?;
        Ok(Self {
            admissions,
            persists,
            revision,
        })
    }

    #[doc(hidden)]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    #[doc(hidden)]
    pub fn admit<R, F>(&self, work: F) -> impl Future<Output = Result<R>> + MaybeSend + use<R, F>
    where
        R: Send + 'static,
        F: FnOnce() -> Work<R> + Send + 'static,
    {
        let span = nanocode_trace::Span::current();
        let (reply, replied) = oneshot::channel();
        let job: Admitted = Box::new(move || {
            Box::pin(
                async move {
                    if !reply.is_closed() {
                        served(reply, work(), || {}).await;
                    }
                }
                .instrument(span),
            )
        });
        answered(self.admissions.send(job).is_ok(), replied)
    }

    #[doc(hidden)]
    pub fn persist<R, F>(&self, work: F) -> impl Future<Output = Result<R>> + MaybeSend + use<R, F>
    where
        R: Send + 'static,
        F: FnOnce() -> Work<R> + Send + 'static,
    {
        let persisted = self.persisted(None, work);
        async move {
            Ok(persisted
                .await?
                .expect("an unconditional persistence command always runs"))
        }
    }

    #[doc(hidden)]
    pub fn persist_at<R, F>(
        &self,
        seen: u64,
        work: F,
    ) -> impl Future<Output = Result<Option<R>>> + MaybeSend + use<R, F>
    where
        R: Send + 'static,
        F: FnOnce() -> Work<R> + Send + 'static,
    {
        self.persisted(Some(seen), work)
    }

    fn persisted<R, F>(
        &self,
        seen: Option<u64>,
        work: F,
    ) -> impl Future<Output = Result<Option<R>>> + MaybeSend + use<R, F>
    where
        R: Send + 'static,
        F: FnOnce() -> Work<R> + Send + 'static,
    {
        let span = nanocode_trace::Span::current();
        let published = Arc::clone(&self.revision);
        let (reply, replied) = oneshot::channel();
        let job: Persisted = Box::new(move |revision| {
            Box::pin(
                async move {
                    if reply.is_closed() {
                        return false;
                    }
                    if seen.is_some_and(|seen| seen != revision)
                        && !mutated(Mutation::PersistIgnoresRevision)
                    {
                        drop(reply.send(Ok(None)));
                        return false;
                    }
                    let next = revision
                        .checked_add(1)
                        .expect("a session boundary revision never wraps");
                    let work = Box::pin(async move { Some(work().await) });
                    let ran =
                        served(reply, work, || published.store(next, Ordering::Release)).await;
                    ran || !mutated(Mutation::AbandonedPersistKeepsRevision)
                }
                .instrument(span),
            )
        });
        let sent = if mutated(Mutation::PersistenceOnAdmissionLane) {
            let current = Arc::clone(&self.revision);
            let queued: Admitted = Box::new(move || {
                Box::pin(async move {
                    job(current.load(Ordering::Acquire)).await;
                })
            });
            self.admissions.send(queued).is_ok()
        } else {
            self.persists.send(job).is_ok()
        };
        answered(sent, replied)
    }
}
