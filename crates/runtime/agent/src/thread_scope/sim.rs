use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    PartDropped,
    AdoptionSkipsRetired,
    AdoptionSkipsBlobs,
    JobForwarded,
    JobRowsDropped,
}

impl Mutation {
    pub const ALL: [Self; 5] = [
        Self::PartDropped,
        Self::AdoptionSkipsRetired,
        Self::AdoptionSkipsBlobs,
        Self::JobForwarded,
        Self::JobRowsDropped,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PartDropped => "part_dropped",
            Self::AdoptionSkipsRetired => "adoption_skips_retired",
            Self::AdoptionSkipsBlobs => "adoption_skips_blobs",
            Self::JobForwarded => "job_forwarded",
            Self::JobRowsDropped => "job_rows_dropped",
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
