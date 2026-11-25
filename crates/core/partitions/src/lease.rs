use crate::Row;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub ttl_ms: u64,
    pub grace_ms: u64,
    pub margin_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unowned;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claiming {
    pub attempt: u32,
    pub sent_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owned {
    pub epoch: u64,
    pub expires_at: u64,
    pub cursor: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Draining {
    pub epoch: u64,
    pub expires_at: u64,
    pub cursor: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Released {
    pub epoch: u64,
    pub cursor: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lost {
    pub epoch: u64,
    pub by: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lease {
    Unowned(Unowned),
    Claiming(Claiming),
    Owned(Owned),
    Draining(Draining),
    Released(Released),
    Lost(Lost),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimAnswer {
    Claimed { epoch: u64, cursor: u64 },
    Refused,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    Owned(Owned),
    Unowned(Unowned),
    Unknown(Claiming),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Landed,
    Moved { current: u64 },
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drain {
    Released(Released),
    Lost(Lost),
    Unknown(Draining),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelfFenced {
    pub epoch: u64,
    pub expires_at: u64,
    pub now: u64,
}

const fn writable(
    epoch: u64,
    expires_at: u64,
    now: u64,
    policy: Policy,
) -> Result<u64, SelfFenced> {
    if now.saturating_add(policy.margin_ms) < expires_at {
        Ok(epoch)
    } else {
        Err(SelfFenced {
            epoch,
            expires_at,
            now,
        })
    }
}

const fn lost(epoch: u64, current: u64) -> Lost {
    assert!(current != epoch, "a refusal names an epoch that moved");
    Lost { epoch, by: current }
}

impl Unowned {
    #[must_use]
    pub const fn claim(self, now: u64) -> Claiming {
        Claiming {
            attempt: 1,
            sent_at: now,
        }
    }
}

impl Claiming {
    #[must_use]
    pub const fn answered(self, answer: ClaimAnswer, policy: Policy) -> Claim {
        match answer {
            ClaimAnswer::Claimed { epoch, cursor } => Claim::Owned(Owned {
                epoch,
                expires_at: self.sent_at.saturating_add(policy.ttl_ms),
                cursor,
            }),
            ClaimAnswer::Refused => Claim::Unowned(Unowned),
            ClaimAnswer::Unknown => Claim::Unknown(self),
        }
    }

    #[must_use]
    pub fn read_back<N: Clone + Eq>(self, row: &Row<N>, me: &N) -> Claim {
        match row {
            Row::Held {
                holder,
                epoch,
                cursor,
                ..
            } if holder == me => Claim::Owned(Owned {
                epoch: *epoch,
                expires_at: self.sent_at,
                cursor: *cursor,
            }),
            Row::Held { .. } | Row::Released { .. } => Claim::Unowned(Unowned),
        }
    }

    #[must_use]
    pub const fn retry(self, now: u64) -> Self {
        Self {
            attempt: self.attempt + 1,
            sent_at: now,
        }
    }
}

impl Owned {
    pub const fn writable(&self, now: u64, policy: Policy) -> Result<u64, SelfFenced> {
        writable(self.epoch, self.expires_at, now, policy)
    }

    pub const fn renewed(self, answer: Answer, sent_at: u64, policy: Policy) -> Result<Self, Lost> {
        match answer {
            Answer::Landed => Ok(Self {
                expires_at: sent_at.saturating_add(policy.ttl_ms),
                ..self
            }),
            Answer::Moved { current } => Err(self.fenced(current)),
            Answer::Unknown => Ok(self),
        }
    }

    #[must_use]
    pub const fn committed(self, cursor: u64) -> Self {
        Self {
            cursor: if cursor > self.cursor {
                cursor
            } else {
                self.cursor
            },
            ..self
        }
    }

    #[must_use]
    pub const fn fenced(self, current: u64) -> Lost {
        lost(self.epoch, current)
    }

    #[must_use]
    pub const fn drain(self) -> Draining {
        Draining {
            epoch: self.epoch,
            expires_at: self.expires_at,
            cursor: self.cursor,
        }
    }
}

impl Draining {
    pub const fn writable(&self, now: u64, policy: Policy) -> Result<u64, SelfFenced> {
        writable(self.epoch, self.expires_at, now, policy)
    }

    pub const fn renewed(self, answer: Answer, sent_at: u64, policy: Policy) -> Result<Self, Lost> {
        match answer {
            Answer::Landed => Ok(Self {
                expires_at: sent_at.saturating_add(policy.ttl_ms),
                ..self
            }),
            Answer::Moved { current } => Err(self.fenced(current)),
            Answer::Unknown => Ok(self),
        }
    }

    #[must_use]
    pub const fn committed(self, cursor: u64) -> Self {
        Self {
            cursor: if cursor > self.cursor {
                cursor
            } else {
                self.cursor
            },
            ..self
        }
    }

    #[must_use]
    pub const fn fenced(self, current: u64) -> Lost {
        lost(self.epoch, current)
    }

    #[must_use]
    pub const fn released(self, answer: Answer, cursor: u64) -> Drain {
        match answer {
            Answer::Landed => Drain::Released(Released {
                epoch: self.epoch,
                cursor,
            }),
            Answer::Moved { current } => Drain::Lost(self.fenced(current)),
            Answer::Unknown => Drain::Unknown(self),
        }
    }

    #[must_use]
    pub fn read_back<N: Clone + Eq>(self, row: &Row<N>, me: &N) -> Drain {
        match row {
            Row::Released { epoch, cursor } if *epoch == self.epoch => Drain::Released(Released {
                epoch: *epoch,
                cursor: *cursor,
            }),
            Row::Held { holder, epoch, .. } if holder == me && *epoch == self.epoch => {
                Drain::Unknown(self)
            }
            Row::Held { .. } | Row::Released { .. } => {
                Drain::Lost(self.fenced(row.epoch().max(self.epoch + 1)))
            }
        }
    }
}

impl Lease {
    #[must_use]
    pub const fn unowned() -> Self {
        Self::Unowned(Unowned)
    }

    #[must_use]
    pub const fn epoch(&self) -> Option<u64> {
        match self {
            Self::Owned(Owned { epoch, .. }) | Self::Draining(Draining { epoch, .. }) => {
                Some(*epoch)
            }
            Self::Unowned(_) | Self::Claiming(_) | Self::Released(_) | Self::Lost(_) => None,
        }
    }

    pub const fn writable(&self, now: u64, policy: Policy) -> Option<Result<u64, SelfFenced>> {
        match self {
            Self::Owned(owned) => Some(owned.writable(now, policy)),
            Self::Draining(draining) => Some(draining.writable(now, policy)),
            Self::Unowned(_) | Self::Claiming(_) | Self::Released(_) | Self::Lost(_) => None,
        }
    }

    #[must_use]
    pub const fn admits(&self) -> bool {
        matches!(self, Self::Owned(_))
    }
}
