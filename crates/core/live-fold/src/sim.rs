use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    SettleDropsDeletes,
    SettleKeepsStart,
    ResumeSkipsFirst,
    FloorIgnored,
    RebaseSkipped,
    SnapshotSkipsRow,
    SnapshotAtHead,
    OutlineLeaks,
    EncodeEveryFrame,
    RebaseDropsTouched,
}

static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);
static ENCODES: AtomicU64 = AtomicU64::new(0);

pub fn mutate(mutation: Mutation) {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
}

pub fn mutated(mutation: Mutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}

pub fn encodes() -> u64 {
    ENCODES.load(Ordering::Relaxed)
}

pub(crate) fn count_encode() {
    ENCODES.fetch_add(1, Ordering::Relaxed);
}
