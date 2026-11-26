use super::{AgentFuture, NanocodeError, Result};
use crate::sim::{Mutation, mutated};
use nanocode_platform::MaybeSend;
use nanocode_sync::future::{self, Either};
use nanocode_sync::{FutureExt as _, mpsc, oneshot};
use nanocode_trace::Instrument as _;
use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

type Job<S> = Box<dyn for<'a> FnOnce(&'a mut S) -> AgentFuture<'a, ()> + Send>;

pub(super) struct Owner<S> {
    jobs: mpsc::UnboundedSender<Job<S>>,
}

async fn serve<S>(mut state: S, mut jobs: mpsc::UnboundedReceiver<Job<S>>) {
    while let Some(job) = jobs.recv().await {
        job(&mut state).await;
    }
}

#[cfg(not(target_family = "wasm"))]
fn start(work: impl Future<Output = ()> + Send + 'static) -> Result<()> {
    nanocode_platform::Handle::try_current()
        .map_err(super::provider_error)?
        .spawn(work);
    Ok(())
}

#[cfg(target_family = "wasm")]
fn start(work: impl Future<Output = ()> + 'static) -> Result<()> {
    nanocode_platform::spawn_local(work);
    Ok(())
}

impl<S: MaybeSend + 'static> Owner<S> {
    pub(super) fn spawn(state: S) -> Result<Self> {
        let (jobs, received) = mpsc::unbounded_channel();
        start(serve(state, received))?;
        Ok(Self { jobs })
    }

    pub(super) fn run<R, F>(
        &self,
        job: F,
    ) -> impl Future<Output = Result<R>> + MaybeSend + use<S, R, F>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut S) -> AgentFuture<'a, R> + Send + 'static,
    {
        let span = nanocode_trace::Span::current();
        let (mut reply, replied) = oneshot::channel();
        let job: Job<S> = Box::new(move |state| {
            Box::pin(
                async move {
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
}

enum Desked<S> {
    Lend(oneshot::Sender<Box<S>>),
    TryLend(oneshot::Sender<Option<Box<S>>>),
    Return(Box<S>),
}

pub(super) struct Desk<S> {
    messages: mpsc::UnboundedSender<Desked<S>>,
    idle: Arc<AtomicBool>,
}

pub(super) struct Lent<S> {
    value: Option<Box<S>>,
    back: mpsc::UnboundedSender<Desked<S>>,
}

struct Lending<S> {
    receiver: oneshot::Receiver<Box<S>>,
    back: mpsc::UnboundedSender<Desked<S>>,
}

struct Kept<S> {
    here: Option<Box<S>>,
    waiting: VecDeque<oneshot::Sender<Box<S>>>,
    idle: Arc<AtomicBool>,
}

impl<S> Kept<S> {
    fn give(&mut self, mut value: Box<S>) {
        assert!(self.here.is_none(), "a lent value comes back once");
        while let Some(waiter) = self.waiting.pop_front() {
            match waiter.send(value) {
                Ok(()) => return,
                Err(back) => value = back,
            }
        }
        self.here = Some(value);
    }

    fn lend(&mut self, waiter: oneshot::Sender<Box<S>>) -> bool {
        let Some(value) = self.here.take() else {
            self.waiting.push_back(waiter);
            return false;
        };
        match waiter.send(value) {
            Ok(()) => true,
            Err(back) => {
                self.here = Some(back);
                false
            }
        }
    }

    fn try_lend(&mut self, waiter: oneshot::Sender<Option<Box<S>>>) {
        let value = self.waiting.is_empty().then(|| self.here.take()).flatten();
        if let Err(Some(back)) = waiter.send(value) {
            self.here = Some(back);
        }
    }

    fn handle(&mut self, message: Desked<S>) -> bool {
        match message {
            Desked::Lend(waiter) => self.lend(waiter),
            Desked::TryLend(waiter) => {
                self.try_lend(waiter);
                false
            }
            Desked::Return(value) => {
                self.give(value);
                false
            }
        }
    }

    fn publish(&self) {
        self.idle.store(self.here.is_some(), Ordering::Release);
    }
}

async fn keep<S>(mut kept: Kept<S>, mut messages: mpsc::UnboundedReceiver<Desked<S>>) {
    let mut held = VecDeque::new();
    while let Some(message) = messages.recv().await {
        let lent = kept.handle(message);
        kept.publish();
        if !lent || !mutated(Mutation::LendBlocksOwner) {
            continue;
        }
        while let Some(message) = messages.recv().await {
            if matches!(message, Desked::Return(_)) {
                kept.handle(message);
                break;
            }
            held.push_back(message);
        }
        while let Some(message) = held.pop_front() {
            kept.handle(message);
        }
        kept.publish();
    }
}

impl<S: MaybeSend + 'static> Desk<S> {
    pub(super) fn spawn(state: S) -> Result<Self> {
        let (messages, received) = mpsc::unbounded_channel();
        let idle = Arc::new(AtomicBool::new(true));
        let kept = Kept {
            here: Some(Box::new(state)),
            waiting: VecDeque::new(),
            idle: Arc::clone(&idle),
        };
        start(keep(kept, received))?;
        Ok(Self { messages, idle })
    }

    pub(super) fn busy(&self) -> bool {
        !self.idle.load(Ordering::Acquire)
    }

    pub(super) async fn lend(&self) -> Result<Lent<S>> {
        let (sender, receiver) = oneshot::channel();
        self.messages
            .send(Desked::Lend(sender))
            .map_err(|_| NanocodeError::AgentStopped)?;
        let mut lending = Lending {
            receiver,
            back: self.messages.clone(),
        };
        let value = (&mut lending.receiver)
            .await
            .map_err(|_| NanocodeError::AgentStopped)?;
        Ok(Lent {
            value: Some(value),
            back: self.messages.clone(),
        })
    }

    pub(super) async fn try_lend(&self) -> Result<Option<Lent<S>>> {
        let (sender, receiver) = oneshot::channel();
        self.messages
            .send(Desked::TryLend(sender))
            .map_err(|_| NanocodeError::AgentStopped)?;
        let value = receiver.await.map_err(|_| NanocodeError::AgentStopped)?;
        Ok(value.map(|value| Lent {
            value: Some(value),
            back: self.messages.clone(),
        }))
    }
}

impl<S> Deref for Lent<S> {
    type Target = S;

    fn deref(&self) -> &S {
        self.value
            .as_deref()
            .expect("a loan holds its value until it is dropped")
    }
}

impl<S> DerefMut for Lent<S> {
    fn deref_mut(&mut self) -> &mut S {
        self.value
            .as_deref_mut()
            .expect("a loan holds its value until it is dropped")
    }
}

impl<S> Drop for Lent<S> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            drop(self.back.send(Desked::Return(value)));
        }
    }
}

impl<S> Drop for Lending<S> {
    fn drop(&mut self) {
        self.receiver.close();
        if let Ok(value) = self.receiver.try_recv() {
            drop(self.back.send(Desked::Return(value)));
        }
    }
}
