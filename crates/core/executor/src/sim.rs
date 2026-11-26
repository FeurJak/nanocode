use std::{
    sync::atomic::{AtomicU8, AtomicU64, Ordering},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    UncountedPolls,
    UncheckedDrive,
    StaleWaiters,
    StrandsRefused,
    RefusesServed,
}

static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);
static PAUSE_NS: AtomicU64 = AtomicU64::new(0);
static THREADS_REFUSED: AtomicU64 = AtomicU64::new(0);

pub fn mutate(mutation: Mutation) {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
}

pub fn pause_before_drive(pause: Duration) {
    let nanos = u64::try_from(pause.as_nanos()).expect("a pause fits in u64 nanoseconds");
    PAUSE_NS.store(nanos, Ordering::SeqCst);
}

pub(crate) fn mutated(mutation: Mutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}

pub(crate) fn pause() {
    let nanos = PAUSE_NS.load(Ordering::Relaxed);
    if nanos > 0 {
        std::thread::sleep(Duration::from_nanos(nanos));
    }
}

pub fn refuse_threads(count: u64) {
    THREADS_REFUSED.store(count, Ordering::SeqCst);
}

pub(crate) fn refuses_thread() -> bool {
    THREADS_REFUSED.load(Ordering::Relaxed) > 0
        && THREADS_REFUSED
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
}
