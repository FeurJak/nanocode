use std::{collections::BTreeMap, ops::Range, process::ExitCode};

use nanocode_partitions::{
    Partition, Row, Token,
    lease::{Answer, Claim, ClaimAnswer, Drain, Lease, Policy, Unowned},
    sim::{self as partitions_sim, Mutation as Rule},
};

use crate::rng::{FNV_OFFSET, Rng, fnv};

const NODES_MIN: u64 = 2;
const NODES_MAX: u64 = 4;
const STEPS_MIN: u64 = 40;
const STEPS_MAX: u64 = 240;
const TTL_MS: u64 = 1_000;
const SKEW_MS: u64 = 100;
const MARGIN_MS: u64 = 2 * SKEW_MS;
const STATEMENT_MS: u64 = 300;
const GRACE_MS: u64 = STATEMENT_MS;
const TICK_MS_MAX: u64 = 2 * STATEMENT_MS;
const START_MS: u64 = 1_000_000;
const POLICY: Policy = Policy {
    ttl_ms: TTL_MS,
    grace_ms: GRACE_MS,
    margin_ms: MARGIN_MS,
};
const SCOPE: Partition = Partition(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseMutation {
    None,
    ClaimKeepsEpoch,
    ReleaseDropsCursor,
    RenewIgnoresEpoch,
    ClaimSkipsGrace,
    ClaimStealsLive,
    WriteSkipsHeadCas,
    CommitLocksLease,
}

impl LeaseMutation {
    const ALL: [Self; 8] = [
        Self::None,
        Self::ClaimKeepsEpoch,
        Self::ReleaseDropsCursor,
        Self::RenewIgnoresEpoch,
        Self::ClaimSkipsGrace,
        Self::ClaimStealsLive,
        Self::WriteSkipsHeadCas,
        Self::CommitLocksLease,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ClaimKeepsEpoch => "claim_keeps_epoch",
            Self::ReleaseDropsCursor => "release_drops_cursor",
            Self::RenewIgnoresEpoch => "renew_ignores_epoch",
            Self::ClaimSkipsGrace => "claim_skips_grace",
            Self::ClaimStealsLive => "claim_steals_live",
            Self::WriteSkipsHeadCas => "write_skips_head_cas",
            Self::CommitLocksLease => "commit_locks_lease",
        }
    }

    pub fn names() -> String {
        Self::ALL
            .iter()
            .skip(1)
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }

    const fn rule(self) -> Rule {
        match self {
            Self::ClaimKeepsEpoch => Rule::ClaimKeepsEpoch,
            Self::ReleaseDropsCursor => Rule::ReleaseDropsCursor,
            Self::RenewIgnoresEpoch => Rule::RenewIgnoresEpoch,
            Self::ClaimSkipsGrace => Rule::ClaimSkipsGrace,
            Self::ClaimStealsLive => Rule::ClaimStealsLive,
            Self::WriteSkipsHeadCas => Rule::Unfenced,
            Self::None | Self::CommitLocksLease => Rule::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LeaseStats {
    pub seeds: u64,
    pub steps: u64,
    pub claims_sent: u64,
    pub claimed: u64,
    pub claims_refused: u64,
    pub claims_unknown: u64,
    pub read_back_owned: u64,
    pub read_back_unowned: u64,
    pub renewed: u64,
    pub renews_moved: u64,
    pub renews_unknown: u64,
    pub writes_landed: u64,
    pub writes_moved: u64,
    pub writes_unknown: u64,
    pub writes_aborted: u64,
    pub self_fenced: u64,
    pub drains: u64,
    pub released: u64,
    pub releases_moved: u64,
    pub releases_unknown: u64,
    pub takeovers_expired: u64,
    pub takeovers_released: u64,
    pub claims_behind_commit: u64,
    pub crashes: u64,
    pub timeouts: u64,
    pub drops: u64,
    pub lost_replies: u64,
    pub skews: u64,
    pub stale_replayed: u64,
    pub stale_refused: u64,
}

impl LeaseStats {
    const fn counters(&self) -> [(&'static str, u64); 29] {
        [
            ("steps", self.steps),
            ("claims_sent", self.claims_sent),
            ("claimed", self.claimed),
            ("claims_refused", self.claims_refused),
            ("claims_unknown", self.claims_unknown),
            ("read_back_owned", self.read_back_owned),
            ("read_back_unowned", self.read_back_unowned),
            ("renewed", self.renewed),
            ("renews_moved", self.renews_moved),
            ("renews_unknown", self.renews_unknown),
            ("writes_landed", self.writes_landed),
            ("writes_moved", self.writes_moved),
            ("writes_unknown", self.writes_unknown),
            ("writes_aborted", self.writes_aborted),
            ("self_fenced", self.self_fenced),
            ("drains", self.drains),
            ("released", self.released),
            ("releases_moved", self.releases_moved),
            ("releases_unknown", self.releases_unknown),
            ("takeovers_expired", self.takeovers_expired),
            ("takeovers_released", self.takeovers_released),
            ("claims_behind_commit", self.claims_behind_commit),
            ("crashes", self.crashes),
            ("timeouts", self.timeouts),
            ("drops", self.drops),
            ("lost_replies", self.lost_replies),
            ("skews", self.skews),
            ("stale_replayed", self.stale_replayed),
            ("stale_refused", self.stale_refused),
        ]
    }

    pub fn add(&mut self, other: &Self) {
        let mut sum = [0_u64; 30];
        sum[0] = self.seeds + other.seeds;
        for (index, ((_, mine), (_, theirs))) in
            self.counters().iter().zip(other.counters()).enumerate()
        {
            sum[index + 1] = mine + theirs;
        }
        *self = Self {
            seeds: sum[0],
            steps: sum[1],
            claims_sent: sum[2],
            claimed: sum[3],
            claims_refused: sum[4],
            claims_unknown: sum[5],
            read_back_owned: sum[6],
            read_back_unowned: sum[7],
            renewed: sum[8],
            renews_moved: sum[9],
            renews_unknown: sum[10],
            writes_landed: sum[11],
            writes_moved: sum[12],
            writes_unknown: sum[13],
            writes_aborted: sum[14],
            self_fenced: sum[15],
            drains: sum[16],
            released: sum[17],
            releases_moved: sum[18],
            releases_unknown: sum[19],
            takeovers_expired: sum[20],
            takeovers_released: sum[21],
            claims_behind_commit: sum[22],
            crashes: sum[23],
            timeouts: sum[24],
            drops: sum[25],
            lost_replies: sum[26],
            skews: sum[27],
            stale_replayed: sum[28],
            stale_refused: sum[29],
        };
    }

    fn summary(&self) -> String {
        let mut line = format!("seeds={}", self.seeds);
        for (name, value) in self.counters() {
            line.push_str(&format!(" {name}={value}"));
        }
        line
    }

    fn uncovered(&self) -> Vec<&'static str> {
        self.counters()
            .into_iter()
            .filter(|(_, value)| *value == 0)
            .map(|(name, _)| name)
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Claim,
    Renew { epoch: u64 },
    Write { epoch: u64, id: u64 },
    Release { epoch: u64 },
    ReadBack,
}

#[derive(Clone, Copy, Debug)]
struct Request {
    node: usize,
    incarnation: u32,
    kind: Kind,
    sent_at: u64,
    answered: bool,
}

#[derive(Clone, Debug)]
enum Reply {
    Claim(ClaimAnswer),
    Renew {
        epoch: u64,
        answer: Answer,
    },
    Write {
        epoch: u64,
        id: u64,
        answer: Answer,
        pos: u64,
    },
    Release {
        epoch: u64,
        answer: Answer,
        cursor: u64,
    },
    Row(Row<usize>),
    Unknown(Kind),
}

#[derive(Clone, Debug)]
struct Delivery {
    node: usize,
    incarnation: u32,
    sent_at: u64,
    reply: Reply,
}

#[derive(Clone, Copy, Debug)]
struct Open {
    request: Request,
    began_at: u64,
}

#[derive(Clone, Copy, Debug)]
struct Landed {
    id: u64,
    epoch: u64,
    node: usize,
    pos: u64,
}

#[derive(Clone, Copy, Debug)]
struct Node {
    lease: Lease,
    incarnation: u32,
    offset: i64,
    writes: u64,
}

struct World {
    rng: Rng,
    mutation: LeaseMutation,
    nodes: Vec<Node>,
    row: Row<usize>,
    now: u64,
    pos: u64,
    log: Vec<Landed>,
    issued: BTreeMap<u64, usize>,
    acked: BTreeMap<u64, u64>,
    network: Vec<Request>,
    replies: Vec<Delivery>,
    open: Option<Open>,
    sent: Vec<Request>,
    next_id: u64,
    stats: LeaseStats,
}

impl World {
    fn local(&self, node: usize) -> u64 {
        self.now.saturating_add_signed(self.nodes[node].offset)
    }

    fn send(&mut self, node: usize, kind: Kind) {
        let request = Request {
            node,
            incarnation: self.nodes[node].incarnation,
            kind,
            sent_at: self.local(node),
            answered: false,
        };
        if matches!(kind, Kind::Write { .. }) {
            self.nodes[node].writes += 1;
            self.sent.push(request);
        }
        self.network.push(request);
    }

    fn reply(&mut self, request: &Request, reply: Reply) {
        if request.answered {
            return;
        }
        self.replies.push(Delivery {
            node: request.node,
            incarnation: request.incarnation,
            sent_at: request.sent_at,
            reply,
        });
    }

    fn act(&mut self, node: usize) {
        let now = self.local(node);
        match self.nodes[node].lease {
            Lease::Unowned(unowned) => {
                self.stats.claims_sent += 1;
                self.nodes[node].lease = Lease::Claiming(unowned.claim(now));
                self.send(node, Kind::Claim);
            }
            Lease::Owned(owned) => match self.rng.below(10) {
                0..=2 => self.send(node, Kind::Renew { epoch: owned.epoch }),
                3 => {
                    self.stats.drains += 1;
                    self.nodes[node].lease = Lease::Draining(owned.drain());
                }
                _ => match owned.writable(now, POLICY) {
                    Ok(epoch) => {
                        self.next_id += 1;
                        let id = self.next_id;
                        self.send(node, Kind::Write { epoch, id });
                    }
                    Err(_) => {
                        self.stats.self_fenced += 1;
                        self.send(node, Kind::Renew { epoch: owned.epoch });
                    }
                },
            },
            Lease::Draining(draining) => {
                if self.nodes[node].writes == 0 {
                    self.send(
                        node,
                        Kind::Release {
                            epoch: draining.epoch,
                        },
                    );
                } else {
                    self.send(
                        node,
                        Kind::Renew {
                            epoch: draining.epoch,
                        },
                    );
                }
            }
            Lease::Claiming(_) => {}
            Lease::Released(_) | Lease::Lost(_) => {
                self.nodes[node].lease = Lease::Unowned(Unowned);
            }
        }
    }

    fn land(&mut self, index: usize) -> Result<(), String> {
        let request = self.network[index];
        let lease_locked = self.mutation == LeaseMutation::CommitLocksLease && self.open.is_some();
        match request.kind {
            Kind::Claim | Kind::Release { .. } if self.open.is_some() => {
                if matches!(request.kind, Kind::Claim) {
                    self.stats.claims_behind_commit += 1;
                }
                return Ok(());
            }
            Kind::Renew { .. } if lease_locked => {
                return Err(format!(
                    "I3 (no lock across a commit): node {} renew waited on the lease row a commit holds",
                    request.node
                ));
            }
            Kind::Write { .. } if self.open.is_some() => return Ok(()),
            _ => {}
        }
        self.network.swap_remove(index);
        match request.kind {
            Kind::Claim => self.land_claim(&request),
            Kind::Renew { epoch } => self.land_renew(&request, epoch),
            Kind::Write { epoch, .. } => {
                self.begin(request, epoch);
                Ok(())
            }
            Kind::Release { epoch } => self.land_release(&request, epoch),
            Kind::ReadBack => {
                let row = self.row.clone();
                self.reply(&request, Reply::Row(row));
                Ok(())
            }
        }
    }

    fn land_claim(&mut self, request: &Request) -> Result<(), String> {
        let before = self.row.clone();
        let Ok((next, epoch)) = before.claim(&request.node, self.now, TTL_MS, GRACE_MS) else {
            self.reply(request, Reply::Claim(ClaimAnswer::Refused));
            return Ok(());
        };
        match &before {
            Row::Released { .. } => {}
            Row::Held {
                holder, expires_at, ..
            } => {
                if *holder != request.node && self.now <= expires_at + GRACE_MS {
                    return Err(format!(
                        "I4 (no stolen lease): node {} claimed epoch {epoch} at db {} from node {holder} whose lease runs to {expires_at} plus grace {GRACE_MS}",
                        request.node, self.now
                    ));
                }
                if *holder != request.node {
                    self.stats.takeovers_expired += 1;
                }
            }
        }
        if matches!(before, Row::Released { epoch, .. } if epoch > 0) {
            self.stats.takeovers_released += 1;
        }
        if let Some(owner) = self.issued.get(&epoch) {
            return Err(format!(
                "I1 (one writer per epoch): node {} claimed epoch {epoch}, already issued to node {owner}",
                request.node
            ));
        }
        self.issued.insert(epoch, request.node);
        let cursor = next.cursor();
        self.row = next;
        self.reply(
            request,
            Reply::Claim(ClaimAnswer::Claimed { epoch, cursor }),
        );
        Ok(())
    }

    fn land_renew(&mut self, request: &Request, epoch: u64) -> Result<(), String> {
        let before = self.row.clone();
        match before.renew(SCOPE, &request.node, epoch, self.now, TTL_MS) {
            Ok(next) => {
                if before.epoch() != epoch || before.holder() != Some(&request.node) {
                    return Err(format!(
                        "I5 (no resurrection): node {}'s renewal at epoch {epoch} landed on {before:?}",
                        request.node
                    ));
                }
                self.row = next;
                self.reply(
                    request,
                    Reply::Renew {
                        epoch,
                        answer: Answer::Landed,
                    },
                );
            }
            Err(fenced) => self.reply(
                request,
                Reply::Renew {
                    epoch,
                    answer: Answer::Moved {
                        current: fenced.current.max(epoch + 1),
                    },
                },
            ),
        }
        Ok(())
    }

    fn begin(&mut self, request: Request, epoch: u64) {
        let token = Token {
            partition: SCOPE,
            epoch,
        };
        if let Err(fenced) = self.row.fence(token) {
            if request.answered {
                self.stats.stale_refused += 1;
            }
            let Kind::Write { id, .. } = request.kind else {
                unreachable!("only writes begin a commit");
            };
            self.reply(
                &request,
                Reply::Write {
                    epoch,
                    id,
                    answer: Answer::Moved {
                        current: fenced.current.max(epoch + 1),
                    },
                    pos: 0,
                },
            );
            return;
        }
        self.open = Some(Open {
            request,
            began_at: self.now,
        });
    }

    fn commit(&mut self) -> Result<(), String> {
        let Some(open) = self.open.take() else {
            return Ok(());
        };
        let request = open.request;
        let Kind::Write { epoch, id } = request.kind else {
            unreachable!("only writes hold the head");
        };
        if self.now > open.began_at + STATEMENT_MS {
            self.stats.writes_aborted += 1;
            self.reply(&request, Reply::Unknown(request.kind));
            return Ok(());
        }
        if self.row.epoch() != epoch {
            return Err(format!(
                "I1 (only the current epoch writes): write {id} from node {} at epoch {epoch} landed while the scope is at {:?}",
                request.node, self.row
            ));
        }
        if self.issued.get(&epoch) != Some(&request.node) {
            return Err(format!(
                "I1 (one writer per epoch): write {id} from node {} landed at epoch {epoch}, issued to {:?}",
                request.node,
                self.issued.get(&epoch)
            ));
        }
        self.pos += 1;
        self.log.push(Landed {
            id,
            epoch,
            node: request.node,
            pos: self.pos,
        });
        let pos = self.pos;
        self.reply(
            &request,
            Reply::Write {
                epoch,
                id,
                answer: Answer::Landed,
                pos,
            },
        );
        Ok(())
    }

    fn land_release(&mut self, request: &Request, epoch: u64) -> Result<(), String> {
        match self.row.release(SCOPE, &request.node, epoch, self.pos) {
            Ok(next) => {
                if next.cursor() != self.pos {
                    return Err(format!(
                        "I2 (release carries the cursor): node {} released epoch {epoch} at cursor {} with writes through {}",
                        request.node,
                        next.cursor(),
                        self.pos
                    ));
                }
                self.row = next;
                let cursor = self.pos;
                self.reply(
                    request,
                    Reply::Release {
                        epoch,
                        answer: Answer::Landed,
                        cursor,
                    },
                );
            }
            Err(fenced) => self.reply(
                request,
                Reply::Release {
                    epoch,
                    answer: Answer::Moved {
                        current: fenced.current.max(epoch + 1),
                    },
                    cursor: 0,
                },
            ),
        }
        Ok(())
    }

    fn deliver(&mut self, delivery: Delivery) {
        let node = delivery.node;
        if self.nodes[node].incarnation != delivery.incarnation {
            return;
        }
        let reply = delivery.reply;
        if matches!(
            reply,
            Reply::Write { .. } | Reply::Unknown(Kind::Write { .. })
        ) {
            self.nodes[node].writes -= 1;
        }
        let lease = self.nodes[node].lease;
        let next = match (lease, reply) {
            (Lease::Claiming(claiming), Reply::Claim(answer))
                if claiming.sent_at == delivery.sent_at =>
            {
                match claiming.answered(answer, POLICY) {
                    Claim::Owned(owned) => {
                        self.stats.claimed += 1;
                        Lease::Owned(owned)
                    }
                    Claim::Unowned(unowned) => {
                        self.stats.claims_refused += 1;
                        Lease::Unowned(unowned)
                    }
                    Claim::Unknown(claiming) => {
                        self.stats.claims_unknown += 1;
                        self.send(node, Kind::ReadBack);
                        Lease::Claiming(claiming)
                    }
                }
            }
            (Lease::Claiming(claiming), Reply::Unknown(Kind::Claim))
                if claiming.sent_at == delivery.sent_at =>
            {
                self.stats.claims_unknown += 1;
                self.send(node, Kind::ReadBack);
                Lease::Claiming(claiming)
            }
            (Lease::Claiming(claiming), Reply::Row(row)) => match claiming.read_back(&row, &node) {
                Claim::Owned(owned) => {
                    self.stats.read_back_owned += 1;
                    Lease::Owned(owned)
                }
                Claim::Unowned(_) | Claim::Unknown(_) => {
                    self.stats.read_back_unowned += 1;
                    Lease::Unowned(Unowned)
                }
            },
            (Lease::Claiming(claiming), Reply::Unknown(Kind::ReadBack)) => {
                self.send(node, Kind::ReadBack);
                Lease::Claiming(claiming)
            }
            (Lease::Draining(draining), Reply::Unknown(Kind::ReadBack)) => {
                self.send(node, Kind::ReadBack);
                Lease::Draining(draining)
            }
            (Lease::Draining(draining), Reply::Row(row)) => {
                self.drained(node, draining.read_back(&row, &node))
            }
            (Lease::Owned(owned), Reply::Renew { epoch, answer }) if owned.epoch == epoch => {
                self.count_renew(answer);
                owned
                    .renewed(answer, delivery.sent_at, POLICY)
                    .map_or_else(Lease::Lost, Lease::Owned)
            }
            (Lease::Draining(draining), Reply::Renew { epoch, answer })
                if draining.epoch == epoch =>
            {
                self.count_renew(answer);
                draining
                    .renewed(answer, delivery.sent_at, POLICY)
                    .map_or_else(Lease::Lost, Lease::Draining)
            }
            (Lease::Owned(_) | Lease::Draining(_), Reply::Unknown(Kind::Renew { .. })) => {
                self.stats.renews_unknown += 1;
                lease
            }
            (
                _,
                Reply::Write {
                    epoch,
                    id,
                    answer,
                    pos,
                },
            ) => self.written(lease, epoch, id, answer, pos),
            (_, Reply::Unknown(Kind::Write { .. })) => {
                self.stats.writes_unknown += 1;
                lease
            }
            (
                Lease::Draining(draining),
                Reply::Release {
                    epoch,
                    answer,
                    cursor,
                },
            ) if draining.epoch == epoch => self.drained(node, draining.released(answer, cursor)),
            (Lease::Draining(draining), Reply::Unknown(Kind::Release { epoch }))
                if draining.epoch == epoch =>
            {
                self.drained(node, draining.released(Answer::Unknown, 0))
            }
            _ => lease,
        };
        self.nodes[node].lease = next;
    }

    const fn count_renew(&mut self, answer: Answer) {
        match answer {
            Answer::Landed => self.stats.renewed += 1,
            Answer::Moved { .. } => self.stats.renews_moved += 1,
            Answer::Unknown => self.stats.renews_unknown += 1,
        }
    }

    fn written(&mut self, lease: Lease, epoch: u64, id: u64, answer: Answer, pos: u64) -> Lease {
        match answer {
            Answer::Landed => {
                self.stats.writes_landed += 1;
                self.acked.insert(id, pos);
                match lease {
                    Lease::Owned(owned) if owned.epoch == epoch => {
                        Lease::Owned(owned.committed(pos))
                    }
                    Lease::Draining(draining) if draining.epoch == epoch => {
                        Lease::Draining(draining.committed(pos))
                    }
                    _ => lease,
                }
            }
            Answer::Moved { current } => {
                self.stats.writes_moved += 1;
                match lease {
                    Lease::Owned(owned) if owned.epoch == epoch => {
                        Lease::Lost(owned.fenced(current))
                    }
                    Lease::Draining(draining) if draining.epoch == epoch => {
                        Lease::Lost(draining.fenced(current))
                    }
                    _ => lease,
                }
            }
            Answer::Unknown => {
                self.stats.writes_unknown += 1;
                lease
            }
        }
    }

    fn drained(&mut self, node: usize, drain: Drain) -> Lease {
        match drain {
            Drain::Released(released) => {
                self.stats.released += 1;
                Lease::Released(released)
            }
            Drain::Lost(lost) => {
                self.stats.releases_moved += 1;
                Lease::Lost(lost)
            }
            Drain::Unknown(draining) => {
                self.stats.releases_unknown += 1;
                self.send(node, Kind::ReadBack);
                Lease::Draining(draining)
            }
        }
    }

    fn crash(&mut self, node: usize) {
        self.stats.crashes += 1;
        let state = &mut self.nodes[node];
        state.incarnation += 1;
        state.lease = Lease::Unowned(Unowned);
        state.writes = 0;
    }

    fn believed(&self) -> Result<(), String> {
        for (index, node) in self.nodes.iter().enumerate() {
            let Lease::Owned(owned) = node.lease else {
                continue;
            };
            if owned.writable(self.local(index), POLICY).is_err() {
                continue;
            }
            let live = match &self.row {
                Row::Held {
                    holder,
                    epoch,
                    expires_at,
                    ..
                } => *holder == index && *epoch >= owned.epoch && self.now <= *expires_at,
                Row::Released { .. } => false,
            };
            if !live {
                return Err(format!(
                    "I1 (one believed writer): node {index} believes it may write at epoch {} (local deadline {}, local now {}) while the scope is {:?} at db {}",
                    owned.epoch,
                    owned.expires_at,
                    self.local(index),
                    self.row,
                    self.now
                ));
            }
        }
        for (id, pos) in &self.acked {
            if !self
                .log
                .iter()
                .any(|landed| landed.id == *id && landed.pos == *pos)
            {
                return Err(format!(
                    "I2 (no lost acked write): write {id} was acked at {pos} and is gone"
                ));
            }
        }
        Ok(())
    }

    fn step(&mut self, forced_crash: bool) -> Result<(), String> {
        self.stats.steps += 1;
        let nodes = self.nodes.len();
        if forced_crash {
            let node = self.rng.pick(nodes);
            self.crash(node);
            return Ok(());
        }
        match self.rng.below(100) {
            0..=24 => {
                let node = self.rng.pick(nodes);
                self.act(node);
            }
            25..=44 if !self.network.is_empty() => {
                let index = self.rng.pick(self.network.len());
                self.land(index)?;
            }
            45..=54 if self.open.is_some() => self.commit()?,
            55..=72 if !self.replies.is_empty() => {
                let index = self.rng.pick(self.replies.len());
                let mut delivery = self.replies.swap_remove(index);
                if self.rng.chance(100_000) {
                    self.stats.lost_replies += 1;
                    if let Some(kind) = kind_of(&delivery.reply) {
                        delivery.reply = Reply::Unknown(kind);
                    }
                }
                self.deliver(delivery);
            }
            73..=76 if !self.network.is_empty() => {
                let index = self.rng.pick(self.network.len());
                let request = self.network[index];
                if !request.answered {
                    self.stats.timeouts += 1;
                    self.network[index].answered = true;
                    self.reply(&request, Reply::Unknown(request.kind));
                }
            }
            77..=78 if !self.network.is_empty() => {
                let index = self.rng.pick(self.network.len());
                let request = self.network.swap_remove(index);
                self.stats.drops += 1;
                self.reply(&request, Reply::Unknown(request.kind));
            }
            79..=80 => {
                let node = self.rng.pick(nodes);
                self.crash(node);
            }
            81..=88 => self.now += self.rng.range(1, TICK_MS_MAX + 1),
            89..=92 => {
                self.stats.skews += 1;
                let node = self.rng.pick(nodes);
                let span = self.rng.below(2 * SKEW_MS + 1);
                self.nodes[node].offset = i64::try_from(span).unwrap_or(0) - SKEW_MS as i64;
            }
            93..=95 if !self.sent.is_empty() => {
                self.stats.stale_replayed += 1;
                let mut copy = self.sent[self.rng.pick(self.sent.len())];
                copy.answered = true;
                self.network.push(copy);
            }
            _ => {}
        }
        self.believed()
    }

    fn digest(&self) -> u64 {
        let mut digest = FNV_OFFSET;
        for landed in &self.log {
            digest = fnv(
                digest,
                format!(
                    "{}@{}:{}:{};",
                    landed.id, landed.epoch, landed.node, landed.pos
                )
                .as_bytes(),
            );
        }
        fnv(digest, format!("{:?}|{}", self.row, self.now).as_bytes())
    }
}

const fn kind_of(reply: &Reply) -> Option<Kind> {
    match reply {
        Reply::Claim(_) => Some(Kind::Claim),
        Reply::Renew { epoch, .. } => Some(Kind::Renew { epoch: *epoch }),
        Reply::Write { epoch, id, .. } => Some(Kind::Write {
            epoch: *epoch,
            id: *id,
        }),
        Reply::Unknown(_) => None,
        Reply::Release { epoch, .. } => Some(Kind::Release { epoch: *epoch }),
        Reply::Row(_) => Some(Kind::ReadBack),
    }
}

struct Shape {
    nodes: usize,
    steps: u64,
    crash_at: u64,
}

impl Shape {
    fn of(seed: u64) -> Self {
        let mut rng = Rng::derive(seed, "lease shape");
        let steps = rng.range(STEPS_MIN, STEPS_MAX + 1);
        Self {
            nodes: rng.range(NODES_MIN, NODES_MAX + 1) as usize,
            steps,
            crash_at: seed % steps,
        }
    }

    fn describe(&self) -> String {
        format!(
            "nodes={} steps={} crash_at={}",
            self.nodes, self.steps, self.crash_at
        )
    }
}

fn run(seed: u64, mutation: LeaseMutation, stats: &mut LeaseStats) -> Result<u64, String> {
    let shape = Shape::of(seed);
    let mut world = World {
        rng: Rng::derive(seed, "lease"),
        mutation,
        nodes: vec![
            Node {
                lease: Lease::unowned(),
                incarnation: 0,
                offset: 0,
                writes: 0,
            };
            shape.nodes
        ],
        row: Row::unclaimed(),
        now: START_MS,
        pos: 0,
        log: Vec::new(),
        issued: BTreeMap::new(),
        acked: BTreeMap::new(),
        network: Vec::new(),
        replies: Vec::new(),
        open: None,
        sent: Vec::new(),
        next_id: 0,
        stats: LeaseStats::default(),
    };
    partitions_sim::mutate(mutation.rule());
    let mut verdict = Ok(());
    for step in 0..shape.steps {
        verdict = world.step(step == shape.crash_at);
        if verdict.is_err() {
            break;
        }
    }
    partitions_sim::mutate(Rule::None);
    world.stats.seeds = 1;
    *stats = world.stats;
    verdict.map(|()| world.digest())
}

pub struct LeaseOutcome {
    digest: u64,
    wrong: Option<String>,
    stats: LeaseStats,
    shape: String,
}

pub fn run_seed(seed: u64, mutation: LeaseMutation) -> LeaseOutcome {
    let shape = Shape::of(seed).describe();
    std::panic::catch_unwind(|| {
        let mut stats = LeaseStats::default();
        let (digest, wrong) = match run(seed, mutation, &mut stats) {
            Ok(digest) => (digest, None),
            Err(message) => (0, Some(message)),
        };
        LeaseOutcome {
            digest,
            wrong,
            stats,
            shape: shape.clone(),
        }
    })
    .unwrap_or_else(|panic| {
        partitions_sim::mutate(Rule::None);
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default();
        LeaseOutcome {
            digest: 0,
            wrong: Some(format!("panic: {message}")),
            stats: LeaseStats::default(),
            shape: String::new(),
        }
    })
}

pub fn judge(
    seed: u64,
    mutation: LeaseMutation,
    twice: bool,
    verbose: bool,
    totals: &mut LeaseStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!(
            "seed {seed} digest {:016x} {} stats={}",
            first.digest,
            first.shape,
            first.stats.summary()
        );
    }
    if first.wrong.is_some() || !twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let second = run_seed(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: LeaseMutation) -> String {
    let mut command = format!("nanocode-sim --scenario lease --seed {seed}");
    if mutation != LeaseMutation::None {
        command.push_str(" --mutation ");
        command.push_str(mutation.name());
    }
    command
}

pub fn report(
    seeds: &Range<u64>,
    passed: u64,
    failed: u64,
    twice: bool,
    mutation: LeaseMutation,
    totals: &LeaseStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=lease mutation={} passed={passed} failed={failed} twice={twice} stats={}",
        seeds.start,
        seeds.end,
        mutation.name(),
        totals.summary()
    );
    let uncovered = totals.uncovered();
    match mutation {
        LeaseMutation::None if failed == 0 && (totals.seeds < 64 || uncovered.is_empty()) => {
            ExitCode::SUCCESS
        }
        LeaseMutation::None => {
            if !uncovered.is_empty() {
                println!(
                    "COVERAGE lease transitions never taken: {}",
                    uncovered.join(",")
                );
            }
            ExitCode::FAILURE
        }
        _ if failed > 0 => {
            println!("MUTATION {} caught by {failed} seeds", mutation.name());
            ExitCode::SUCCESS
        }
        _ => {
            println!("MUTATION {} NOT caught", mutation.name());
            ExitCode::FAILURE
        }
    }
}
