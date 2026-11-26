use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    WaitsUnbounded,
    CallBoundSkipsCancel,
}

impl Mutation {
    pub const ALL: [Self; 2] = [Self::WaitsUnbounded, Self::CallBoundSkipsCancel];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::WaitsUnbounded => "waits_unbounded",
            Self::CallBoundSkipsCancel => "call_bound_skips_cancel",
        }
    }

    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        std::iter::once(Self::None)
            .chain(Self::ALL)
            .find(|mutation| mutation.name() == name)
    }
}

static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);

pub fn mutate(mutation: Mutation) {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
}

#[must_use]
pub fn mutated(mutation: Mutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}
