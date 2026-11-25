use std::{collections::BTreeSet, ops::Range, process::ExitCode};

use crate::rng::{FNV_OFFSET, Rng, fnv};

const WRITERS_MIN: u64 = 2;
const WRITERS_MAX: u64 = 5;
const STEPS: Range<u64> = 200..1_200;
const CLAIM_PPM: u64 = 120_000;
const BEGIN_PPM: u64 = 600_000;
const STATEMENTS_MAX: u64 = 4;
const ABORT_PPM: u64 = 30_000;
const ACK_LOST_PPM: u64 = 50_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlogMutation {
    None,
    AppendSkipsEpoch,
}

impl SlogMutation {
    const ALL: [Self; 2] = [Self::None, Self::AppendSkipsEpoch];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::AppendSkipsEpoch => "append_skips_epoch",
        }
    }

    pub fn names() -> String {
        Self::ALL[1..]
            .iter()
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SlogStats {
    pub seeds: u64,
    pub claims: u64,
    pub commits: u64,
    pub landed: u64,
    pub lost: u64,
    pub waited: u64,
    pub aborts: u64,
    pub acks_lost: u64,
}

impl SlogStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.claims += other.claims;
        self.commits += other.commits;
        self.landed += other.landed;
        self.lost += other.lost;
        self.waited += other.waited;
        self.aborts += other.aborts;
        self.acks_lost += other.acks_lost;
    }
}

enum Head {
    Pending,
    Held,
    Missed,
}

struct Batch {
    epoch: u64,
    head: Head,
    statements: u64,
    staged: Vec<u64>,
}

#[derive(Default)]
struct Writer {
    epoch: Option<u64>,
    batch: Option<Batch>,
}

struct World {
    mutation: SlogMutation,
    epoch: u64,
    issued: BTreeSet<u64>,
    locked: Option<usize>,
    rows: Vec<(u64, usize, u64)>,
    next: u64,
    writers: Vec<Writer>,
    stats: SlogStats,
    wrong: Option<String>,
}

impl World {
    fn claim(&mut self, writer: usize) {
        if self.locked.is_some_and(|holder| holder != writer) {
            self.stats.waited += 1;
            return;
        }
        let before = self.epoch;
        self.epoch += 1;
        assert!(self.epoch > before);
        if !self.issued.insert(self.epoch) {
            self.wrong = Some(format!("epoch {} was issued twice", self.epoch));
        }
        self.writers[writer].epoch = Some(self.epoch);
        self.stats.claims += 1;
    }

    fn head(&mut self, writer: usize, epoch: u64) -> Option<Head> {
        if self.mutation == SlogMutation::AppendSkipsEpoch {
            return Some(Head::Held);
        }
        if self.locked.is_some_and(|holder| holder != writer) {
            self.stats.waited += 1;
            return None;
        }
        if self.epoch != epoch {
            return Some(Head::Missed);
        }
        self.locked = Some(writer);
        Some(Head::Held)
    }

    fn guarded(&self, epoch: u64) -> bool {
        self.mutation == SlogMutation::AppendSkipsEpoch || self.epoch == epoch
    }

    fn step(&mut self, writer: usize, rng: &mut Rng) {
        let Some(mut batch) = self.writers[writer].batch.take() else {
            match self.writers[writer].epoch {
                Some(epoch) if !rng.chance(CLAIM_PPM) => {
                    if rng.chance(BEGIN_PPM) {
                        self.writers[writer].batch = Some(Batch {
                            epoch,
                            head: Head::Pending,
                            statements: rng.range(1, STATEMENTS_MAX + 1),
                            staged: Vec::new(),
                        });
                    }
                }
                _ => self.claim(writer),
            }
            return;
        };
        if matches!(batch.head, Head::Pending) {
            if let Some(head) = self.head(writer, batch.epoch) {
                batch.head = head;
            }
            self.writers[writer].batch = Some(batch);
            return;
        }
        if batch.statements > 0 {
            batch.statements -= 1;
            if self.guarded(batch.epoch) {
                batch.staged.push(self.next);
                self.next += 1;
            }
            self.writers[writer].batch = Some(batch);
            return;
        }
        self.finish(writer, batch, rng);
    }

    fn finish(&mut self, writer: usize, batch: Batch, rng: &mut Rng) {
        if self.locked == Some(writer) {
            self.locked = None;
        }
        if rng.chance(ABORT_PPM) {
            self.stats.aborts += 1;
            return;
        }
        for seq in &batch.staged {
            if self.epoch != batch.epoch && self.wrong.is_none() {
                self.wrong = Some(format!(
                    "writer {writer} appended seq {seq} at epoch {} after the head moved to {}",
                    batch.epoch, self.epoch
                ));
            }
            self.rows.push((*seq, writer, batch.epoch));
            self.stats.landed += 1;
        }
        self.stats.commits += 1;
        match batch.head {
            Head::Missed => {
                if !batch.staged.is_empty() && self.wrong.is_none() {
                    self.wrong = Some(format!(
                        "writer {writer} at epoch {} was refused as Lost but {} rows landed",
                        batch.epoch,
                        batch.staged.len()
                    ));
                }
                self.writers[writer].epoch = None;
                self.stats.lost += 1;
            }
            Head::Held | Head::Pending => {
                if rng.chance(ACK_LOST_PPM) {
                    self.stats.acks_lost += 1;
                }
            }
        }
    }
}

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: SlogStats,
    shape: String,
}

fn run_seed(seed: u64, mutation: SlogMutation) -> Outcome {
    let mut rng = Rng::derive(seed, "slog");
    let writers = rng.range(WRITERS_MIN, WRITERS_MAX + 1);
    let steps = rng.range(STEPS.start, STEPS.end);
    let mut world = World {
        mutation,
        epoch: 0,
        issued: BTreeSet::new(),
        locked: None,
        rows: Vec::new(),
        next: 1,
        writers: (0..writers).map(|_| Writer::default()).collect(),
        stats: SlogStats {
            seeds: 1,
            ..SlogStats::default()
        },
        wrong: None,
    };
    for _ in 0..steps {
        let writer = rng.pick(world.writers.len());
        world.step(writer, &mut rng);
        if world.wrong.is_some() {
            break;
        }
    }
    let mut digest = FNV_OFFSET;
    for (seq, writer, epoch) in &world.rows {
        digest = fnv(digest, &seq.to_le_bytes());
        digest = fnv(digest, &writer.to_le_bytes());
        digest = fnv(digest, &epoch.to_le_bytes());
    }
    digest = fnv(digest, &world.epoch.to_le_bytes());
    Outcome {
        digest,
        wrong: world.wrong,
        stats: world.stats,
        shape: format!("writers={writers} steps={steps}"),
    }
}

pub fn judge(
    seed: u64,
    mutation: SlogMutation,
    twice: bool,
    verbose: bool,
    totals: &mut SlogStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!(
            "seed {seed} digest {:016x} {} stats={:?}",
            first.digest, first.shape, first.stats
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

pub fn replay(seed: u64, mutation: SlogMutation) -> String {
    let mut command = format!("nanocode-sim --scenario slog --seed {seed}");
    if mutation != SlogMutation::None {
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
    mutation: SlogMutation,
    totals: &SlogStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=slog mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        SlogMutation::None if failed == 0 => ExitCode::SUCCESS,
        SlogMutation::None => ExitCode::FAILURE,
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
