use super::{AgentFuture, NanocodeError, Result};
use nanocode_platform::MaybeSend;
use nanocode_sync::future::{self, Either};
use nanocode_sync::{FutureExt as _, mpsc, oneshot};
use nanocode_trace::Instrument as _;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

type Job<S> = Box<dyn for<'a> FnOnce(&'a mut S) -> AgentFuture<'a, ()> + Send>;

pub(super) struct Owner<S> {
    jobs: mpsc::UnboundedSender<Job<S>>,
    queued: Arc<AtomicUsize>,
}

struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        let queued = self.0.fetch_sub(1, Ordering::AcqRel);
        assert!(queued > 0, "an owner job is counted out once");
    }
}

async fn serve<S>(mut state: S, mut jobs: mpsc::UnboundedReceiver<Job<S>>) {
    while let Some(job) = jobs.recv().await {
        job(&mut state).await;
    }
}

#[cfg(not(target_family = "wasm"))]
fn start<S: Send + 'static>(state: S, jobs: mpsc::UnboundedReceiver<Job<S>>) -> Result<()> {
    nanocode_platform::Handle::try_current()
        .map_err(super::provider_error)?
        .spawn(serve(state, jobs));
    Ok(())
}

#[cfg(target_family = "wasm")]
fn start<S: 'static>(state: S, jobs: mpsc::UnboundedReceiver<Job<S>>) -> Result<()> {
    nanocode_platform::spawn_local(serve(state, jobs));
    Ok(())
}

impl<S: MaybeSend + 'static> Owner<S> {
    pub(super) fn spawn(state: S) -> Result<Self> {
        let (jobs, received) = mpsc::unbounded_channel();
        start(state, received)?;
        Ok(Self {
            jobs,
            queued: Arc::default(),
        })
    }

    pub(super) fn busy(&self) -> bool {
        self.queued.load(Ordering::Acquire) > 0
    }

    pub(super) fn run<R, F>(
        &self,
        job: F,
    ) -> impl Future<Output = Result<R>> + MaybeSend + use<S, R, F>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut S) -> AgentFuture<'a, R> + Send + 'static,
    {
        self.queued.fetch_add(1, Ordering::AcqRel);
        self.submit(job)
    }

    pub(super) fn try_run<R, F>(
        &self,
        job: F,
    ) -> Option<impl Future<Output = Result<R>> + MaybeSend + use<S, R, F>>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut S) -> AgentFuture<'a, R> + Send + 'static,
    {
        self.queued
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(self.submit(job))
    }

    pub(super) fn call<R, F>(
        &self,
        job: F,
    ) -> impl Future<Output = Result<R>> + MaybeSend + use<S, R, F>
    where
        R: Send + 'static,
        F: FnOnce(&mut S) -> R + Send + 'static,
    {
        self.run(move |state| Box::pin(async move { job(state) }))
    }

    fn submit<R, F>(&self, job: F) -> impl Future<Output = Result<R>> + MaybeSend + use<S, R, F>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut S) -> AgentFuture<'a, R> + Send + 'static,
    {
        let counted = Counted(Arc::clone(&self.queued));
        let span = nanocode_trace::Span::current();
        let (mut reply, replied) = oneshot::channel();
        let job: Job<S> = Box::new(move |state| {
            Box::pin(
                async move {
                    let _counted = counted;
                    if reply.is_closed() {
                        return;
                    }
                    let work = AssertUnwindSafe(job(state)).catch_unwind();
                    let output = {
                        let abandoned = Box::pin(reply.closed());
                        match future::select(Box::pin(work), abandoned).await {
                            Either::Left((output, _)) => Some(output),
                            Either::Right(_) => None,
                        }
                    };
                    if let Some(output) = output {
                        drop(reply.send(output));
                    }
                }
                .instrument(span),
            )
        });
        let sent = self.jobs.send(job).is_ok();
        async move {
            if !sent {
                return Err(NanocodeError::AgentStopped);
            }
            match replied.await {
                Ok(Ok(output)) => Ok(output),
                Ok(Err(panic)) => std::panic::resume_unwind(panic),
                Err(_) => Err(NanocodeError::AgentStopped),
            }
        }
    }
}
