use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    WaitRepeatsIds,
    RestoreRemembersWaits,
    RouteSkipsClaim,
    ScopeJobsOverlap,
    RestoreSharesClaim,
}

impl Mutation {
    pub const ALL: [Self; 6] = [
        Self::None,
        Self::WaitRepeatsIds,
        Self::RestoreRemembersWaits,
        Self::RouteSkipsClaim,
        Self::ScopeJobsOverlap,
        Self::RestoreSharesClaim,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::WaitRepeatsIds => "subagents_wait_repeats_ids",
            Self::RestoreRemembersWaits => "subagents_restore_remembers_waits",
            Self::RouteSkipsClaim => "subagents_route_skips_claim",
            Self::ScopeJobsOverlap => "subagents_scope_jobs_overlap",
            Self::RestoreSharesClaim => "subagents_restore_shares_claim",
        }
    }
}

static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);

pub fn mutate(mutation: Mutation) {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
}

pub fn mutated(mutation: Mutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}
