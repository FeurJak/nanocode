use std::{
    future::Future,
    pin::Pin,
    sync::OnceLock,
    task::Poll,
    time::{Duration, Instant, SystemTime},
};

pub type HostFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

trait Pinned: Future + Send + Sized + 'static {
    #[inline(never)]
    fn pinned(self) -> Pin<Box<dyn Future<Output = Self::Output> + Send + 'static>> {
        Box::pin(self)
    }
}

impl<F: Future + Send + 'static> Pinned for F {}

pub trait Host: Send + Sync + 'static {
    fn now(&self) -> Instant;
    fn system_time(&self) -> SystemTime;
    fn sleep(&self, duration: Duration) -> HostFuture;
    fn fill_random(&self, bytes: &mut [u8]) -> std::io::Result<()>;
    fn spawn(&self, future: HostFuture);
    fn uuid_stamps(&self) -> Option<&std::sync::atomic::AtomicU64> {
        None
    }
    /// This thread's share of the host, for a thread that works for this one while this one
    /// waits on it (such as the thread that keeps the native Python interpreter). None when the
    /// host keeps nothing per thread.
    fn lend(&self) -> Option<Lent> {
        None
    }
}

/// A thread's share of the host, carried to a thread that works for it while it waits.
pub struct Lent(Box<dyn FnOnce() -> Box<dyn std::any::Any> + Send>);

impl Lent {
    pub fn new(enter: impl FnOnce() -> Box<dyn std::any::Any> + Send + 'static) -> Self {
        Self(Box::new(enter))
    }

    /// Takes on the lending thread's share on this thread until the returned guard drops.
    #[must_use]
    pub fn enter(self) -> Box<dyn std::any::Any> {
        (self.0)()
    }
}

/// This thread's share of the installed host, when the host keeps one per thread.
pub fn lend() -> Option<Lent> {
    host().and_then(Host::lend)
}

static HOST: OnceLock<&'static dyn Host> = OnceLock::new();

pub fn install(host: &'static dyn Host) -> std::io::Result<()> {
    HOST.set(host)
        .map_err(|_| std::io::Error::other("a platform host is already installed"))
}

pub fn host() -> Option<&'static dyn Host> {
    HOST.get().copied()
}

pub fn now() -> Instant {
    host().map_or_else(Instant::now, |host| host.now())
}

pub fn system_time() -> SystemTime {
    host().map_or_else(SystemTime::now, |host| host.system_time())
}

enum Delay {
    Executor(nanocode_executor::Sleep),
    Host(HostFuture),
}

pub fn sleep(duration: Duration) -> impl Future<Output = ()> + Send + 'static {
    let delay = match host() {
        Some(host) => Delay::Host(host.sleep(duration)),
        None => Delay::Executor(nanocode_executor::sleep(duration)),
    };
    async move {
        match delay {
            Delay::Executor(delay) => delay.await,
            Delay::Host(delay) => delay.await,
        }
    }
}

enum Timed<F: Future> {
    Executor(F, nanocode_executor::Sleep),
    Host(F, HostFuture),
}

impl<F: Future> Timed<F> {
    async fn run(self) -> Result<F::Output, crate::Elapsed> {
        match self {
            Self::Executor(future, delay) => {
                let mut future = std::pin::pin!(future);
                let mut delay = delay;
                std::future::poll_fn(|context| {
                    if let Poll::Ready(output) = future.as_mut().poll(context) {
                        return Poll::Ready(Ok(output));
                    }
                    Pin::new(&mut delay)
                        .poll(context)
                        .map(|()| Err(crate::Elapsed))
                })
                .await
            }
            Self::Host(future, mut delay) => {
                let mut future = std::pin::pin!(future);
                std::future::poll_fn(|context| {
                    if let Poll::Ready(output) = future.as_mut().poll(context) {
                        return Poll::Ready(Ok(output));
                    }
                    delay.as_mut().poll(context).map(|()| Err(crate::Elapsed))
                })
                .await
            }
        }
    }
}

pub fn timeout<F: Future>(
    duration: Duration,
    future: F,
) -> impl Future<Output = Result<F::Output, crate::Elapsed>> {
    let timed = match host() {
        Some(host) => Timed::Host(future, host.sleep(duration)),
        None => Timed::Executor(future, nanocode_executor::sleep(duration)),
    };
    timed.run()
}
pub fn sleep_until(deadline: Instant) -> impl Future<Output = ()> + Send + 'static {
    let delay = match host() {
        Some(host) => Delay::Host(host.sleep(deadline.saturating_duration_since(host.now()))),
        None => Delay::Executor(nanocode_executor::sleep_until(deadline)),
    };
    async move {
        match delay {
            Delay::Executor(delay) => delay.await,
            Delay::Host(delay) => delay.await,
        }
    }
}

pub fn timeout_at<F: Future>(
    deadline: Instant,
    future: F,
) -> impl Future<Output = Result<F::Output, crate::Elapsed>> {
    let timed = match host() {
        Some(host) => Timed::Host(
            future,
            host.sleep(deadline.saturating_duration_since(host.now())),
        ),
        None => Timed::Executor(future, nanocode_executor::sleep_until(deadline)),
    };
    timed.run()
}

enum Wake {
    Executor(nanocode_executor::Sleep),
    Host(HostFuture),
}

pub struct Sleep {
    deadline: Instant,
    wake: Wake,
}

impl Sleep {
    pub fn after(duration: Duration) -> Self {
        Self::until(now() + duration)
    }

    pub fn until(deadline: Instant) -> Self {
        let wake = match host() {
            Some(host) => Wake::Host(host.sleep(deadline.saturating_duration_since(host.now()))),
            None => Wake::Executor(nanocode_executor::sleep_until(deadline)),
        };
        Self { deadline, wake }
    }

    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn reset(&mut self, deadline: Instant) {
        self.deadline = deadline;
        match (&mut self.wake, host()) {
            (Wake::Executor(delay), None) => delay.reset(deadline),
            (wake, _) => *wake = Self::until(deadline).wake,
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut std::task::Context<'_>) -> Poll<()> {
        match &mut self.get_mut().wake {
            Wake::Executor(delay) => Pin::new(delay).poll(context),
            Wake::Host(delay) => delay.as_mut().poll(context),
        }
    }
}

#[derive(Debug)]
pub struct TryCurrentError;

impl std::fmt::Display for TryCurrentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "there is no reactor running, must be called from the context of a nanocode runtime",
        )
    }
}

impl std::error::Error for TryCurrentError {}

#[derive(Clone)]
pub enum Handle {
    Host(&'static dyn Host),
    Executor(nanocode_executor::Handle),
}

impl Handle {
    pub fn try_current() -> Result<Self, TryCurrentError> {
        if let Some(host) = host() {
            return Ok(Self::Host(host));
        }
        nanocode_executor::Handle::try_current()
            .map(Self::Executor)
            .map_err(|_| TryCurrentError)
    }

    pub fn enter(&self) -> Option<nanocode_executor::EnterGuard<'_>> {
        match self {
            Self::Host(_) => None,
            Self::Executor(runtime) => Some(runtime.enter()),
        }
    }

    #[inline]
    pub fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let future: HostFuture = future.pinned();
        match self {
            Self::Host(host) => host.spawn(future),
            Self::Executor(runtime) => drop(runtime.spawn(future)),
        }
    }
}

#[inline]
pub fn spawn(future: impl Future<Output = ()> + Send + 'static) {
    let future: HostFuture = future.pinned();
    match host() {
        Some(host) => host.spawn(future),
        None => drop(nanocode_executor::spawn(future)),
    }
}

#[inline]
#[track_caller]
pub fn spawn_task<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> nanocode_executor::JoinHandle<T> {
    let future = future.pinned();
    match host() {
        Some(host) => nanocode_executor::spawn_with(future, |driven| host.spawn(driven)),
        None => nanocode_executor::spawn(future),
    }
}

#[inline]
#[track_caller]
pub fn spawn_on<T: Send + 'static>(
    runtime: &nanocode_executor::Handle,
    future: impl Future<Output = T> + Send + 'static,
) -> nanocode_executor::JoinHandle<T> {
    let future = future.pinned();
    runtime.spawn(future)
}

#[inline]
#[track_caller]
pub fn spawn_joined<T: Send + 'static>(
    set: &mut nanocode_executor::JoinSet<T>,
    future: impl Future<Output = T> + Send + 'static,
) -> nanocode_executor::AbortHandle {
    let future = future.pinned();
    match host() {
        Some(host) => set.push(nanocode_executor::spawn_with(future, |driven| {
            host.spawn(driven);
        })),
        None => set.spawn(future),
    }
}

#[track_caller]
pub fn spawn_blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> nanocode_executor::JoinHandle<T> {
    let work: Box<dyn FnOnce() -> T + Send + 'static> = Box::new(work);
    nanocode_executor::spawn_blocking(work)
}
