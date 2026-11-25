use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

pub mod lease;
pub mod sim;

use sim::{Mutation, mutated};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Partition(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Token {
    pub partition: Partition,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fenced {
    pub partition: Partition,
    pub held: u64,
    pub current: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused<N> {
    Held {
        holder: N,
        epoch: u64,
        expires_at: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row<N> {
    Released {
        epoch: u64,
        cursor: u64,
    },
    Held {
        holder: N,
        epoch: u64,
        expires_at: u64,
        cursor: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member<N> {
    pub node: N,
    pub seen_at: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub claim: Vec<Partition>,
    pub release: Vec<Token>,
}

fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

const fn finish(mut hash: u64) -> u64 {
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^ (hash >> 33)
}

#[must_use]
pub fn partition_of(key: &[u8], count: NonZeroU32) -> Partition {
    let hash = finish(fnv(FNV_OFFSET, key));
    Partition(
        u32::try_from(hash % u64::from(count.get()))
            .expect("a remainder of a u32 count fits a u32"),
    )
}

#[must_use]
pub fn weight(node: &[u8], partition: Partition) -> u64 {
    finish(fnv(fnv(FNV_OFFSET, &partition.0.to_le_bytes()), node))
}

#[must_use]
pub fn assignee<N: AsRef<[u8]> + Ord>(partition: Partition, live: &BTreeSet<N>) -> Option<&N> {
    if mutated(Mutation::AssignByIndex) {
        let len = u32::try_from(live.len()).ok().filter(|len| *len > 0)?;
        return live.iter().nth(usize::try_from(partition.0 % len).ok()?);
    }
    live.iter().max_by(|a, b| {
        weight(a.as_ref(), partition)
            .cmp(&weight(b.as_ref(), partition))
            .then_with(|| b.cmp(a))
    })
}

#[must_use]
pub fn assignment<N: AsRef<[u8]> + Ord + Clone>(
    count: NonZeroU32,
    live: &BTreeSet<N>,
) -> BTreeMap<Partition, N> {
    (0..count.get())
        .map(Partition)
        .filter_map(|partition| assignee(partition, live).map(|node| (partition, node.clone())))
        .collect()
}

#[must_use]
pub fn live<N: Ord + Clone>(members: &[Member<N>], now: u64, ttl: u64) -> BTreeSet<N> {
    members
        .iter()
        .filter(|member| member.seen_at.saturating_add(ttl) > now)
        .map(|member| member.node.clone())
        .collect()
}

impl<N: Clone + Eq> Row<N> {
    #[must_use]
    pub const fn unclaimed() -> Self {
        Self::Released {
            epoch: 0,
            cursor: 0,
        }
    }

    #[must_use]
    pub const fn epoch(&self) -> u64 {
        match self {
            Self::Released { epoch, .. } | Self::Held { epoch, .. } => *epoch,
        }
    }

    #[must_use]
    pub const fn cursor(&self) -> u64 {
        match self {
            Self::Released { cursor, .. } | Self::Held { cursor, .. } => *cursor,
        }
    }

    #[must_use]
    pub const fn holder(&self) -> Option<&N> {
        match self {
            Self::Released { .. } => None,
            Self::Held { holder, .. } => Some(holder),
        }
    }

    #[must_use]
    pub fn claimable(&self, node: &N, now: u64, grace: u64) -> bool {
        if mutated(Mutation::ClaimStealsLive) {
            return true;
        }
        match self {
            Self::Released { .. } => true,
            Self::Held {
                holder, expires_at, ..
            } => {
                let lapsed = if mutated(Mutation::ClaimSkipsGrace) {
                    *expires_at < now
                } else {
                    expires_at.saturating_add(grace) < now
                };
                holder == node || lapsed
            }
        }
    }

    pub fn claim(
        &self,
        node: &N,
        now: u64,
        ttl: u64,
        grace: u64,
    ) -> Result<(Self, u64), Refused<N>> {
        if let Self::Held {
            holder,
            epoch,
            expires_at,
            ..
        } = self
            && !self.claimable(node, now, grace)
        {
            return Err(Refused::Held {
                holder: holder.clone(),
                epoch: *epoch,
                expires_at: *expires_at,
            });
        }
        let raised = if mutated(Mutation::ClaimKeepsEpoch) && self.holder().is_some() {
            self.epoch()
        } else {
            self.epoch() + 1
        };
        Ok((
            Self::Held {
                holder: node.clone(),
                epoch: raised,
                expires_at: now.saturating_add(ttl),
                cursor: self.cursor(),
            },
            raised,
        ))
    }

    pub fn renew(
        &self,
        partition: Partition,
        node: &N,
        epoch: u64,
        now: u64,
        ttl: u64,
    ) -> Result<Self, Fenced> {
        match self {
            Self::Held {
                holder,
                epoch: current,
                cursor,
                ..
            } if holder == node && (*current == epoch || mutated(Mutation::RenewIgnoresEpoch)) => {
                Ok(Self::Held {
                    holder: node.clone(),
                    epoch: *current,
                    expires_at: now.saturating_add(ttl),
                    cursor: *cursor,
                })
            }
            _ => Err(Fenced {
                partition,
                held: epoch,
                current: self.epoch(),
            }),
        }
    }

    pub fn release(
        &self,
        partition: Partition,
        node: &N,
        epoch: u64,
        cursor: u64,
    ) -> Result<Self, Fenced> {
        match self {
            Self::Held {
                holder,
                epoch: current,
                cursor: before,
                ..
            } if holder == node && *current == epoch => Ok(Self::Released {
                epoch,
                cursor: if mutated(Mutation::ReleaseDropsCursor) {
                    *before
                } else {
                    cursor
                },
            }),
            _ => Err(Fenced {
                partition,
                held: epoch,
                current: self.epoch(),
            }),
        }
    }

    pub fn fence(&self, token: Token) -> Result<(), Fenced> {
        let admitted = match self {
            Self::Held { epoch, .. } => *epoch == token.epoch,
            Self::Released { .. } => false,
        };
        if admitted || mutated(Mutation::Unfenced) {
            return Ok(());
        }
        Err(Fenced {
            partition: token.partition,
            held: token.epoch,
            current: self.epoch(),
        })
    }
}

#[must_use]
pub fn plan<N: AsRef<[u8]> + Ord + Clone>(
    me: &N,
    count: NonZeroU32,
    live: &BTreeSet<N>,
    held: &BTreeMap<Partition, u64>,
    rows: &BTreeMap<Partition, Row<N>>,
    now: u64,
    grace: u64,
) -> Plan {
    let mut plan = Plan::default();
    for partition in (0..count.get()).map(Partition) {
        let mine = assignee(partition, live) == Some(me);
        match (mine, held.get(&partition)) {
            (true, None) => {
                let claimable = rows.get(&partition).is_none_or(|row| {
                    if mutated(Mutation::WaitsForFree) {
                        return row.holder().is_none();
                    }
                    row.claimable(me, now, grace)
                });
                if claimable {
                    plan.claim.push(partition);
                }
            }
            (false, Some(epoch)) if !mutated(Mutation::KeepsMoved) => {
                plan.release.push(Token {
                    partition,
                    epoch: *epoch,
                });
            }
            _ => {}
        }
    }
    plan
}
