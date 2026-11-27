use std::{ops::Range, process::ExitCode, time::Duration};

use nanocode::claude::{CACHE_LIFETIME, HourCache, LastRequest};
use nanocode_backend_claude_client::sim::Mutation as ClaudeRule;

use crate::rng::{FNV_OFFSET, Rng, fnv};

const STEPS_MIN: u64 = 4;
const STEPS_MAX: u64 = 40;
const RESTART_PPM: u64 = 250_000;
const START_UNIX_SECONDS: u64 = 1_795_000_000;
const POLICIES: [HourCache; 3] = [HourCache::Never, HourCache::AfterIdle, HourCache::Always];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachePolicyMutation {
    None,
    RestoredIgnored,
}

impl CachePolicyMutation {
    const ALL: [Self; 2] = [Self::None, Self::RestoredIgnored];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::RestoredIgnored => "restored_ignored",
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

    fn install(self) {
        nanocode_backend_claude_client::sim::mutate(match self {
            Self::None => ClaudeRule::None,
            Self::RestoredIgnored => ClaudeRule::RestoredIgnored,
        });
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CachePolicyStats {
    pub seeds: u64,
    pub requests: u64,
    pub restarts: u64,
    pub idle_gaps: u64,
    pub resumed_idle: u64,
    pub switched: u64,
}

impl CachePolicyStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.requests += other.requests;
        self.restarts += other.restarts;
        self.idle_gaps += other.idle_gaps;
        self.resumed_idle += other.resumed_idle;
        self.switched += other.switched;
    }
}

struct Session {
    one_hour: bool,
    stored: Option<u64>,
    in_process: Option<u64>,
}

fn run(seed: u64, stats: &mut CachePolicyStats) -> Result<u64, String> {
    let mut rng = Rng::derive(seed, "cache_policy");
    let policy = POLICIES[rng.pick(POLICIES.len())];
    let lifetime = CACHE_LIFETIME.as_secs();
    let mut now = START_UNIX_SECONDS;
    let mut session = Session {
        one_hour: false,
        stored: None,
        in_process: None,
    };
    let mut digest = FNV_OFFSET;
    for _ in 0..rng.range(STEPS_MIN, STEPS_MAX) {
        let restarted = rng.chance(RESTART_PPM);
        if restarted {
            session.in_process = None;
            stats.restarts += 1;
        }
        now += rng.below(2 * lifetime + 2);
        let last = match (session.in_process, session.stored) {
            (Some(at), _) => LastRequest::InProcess {
                elapsed: Duration::from_secs(now - at),
            },
            (None, Some(unix_seconds)) => LastRequest::Stored { unix_seconds },
            (None, None) => LastRequest::None,
        };
        let idle = session.stored.is_some_and(|at| now - at > lifetime);
        stats.idle_gaps += u64::from(idle);
        stats.resumed_idle += u64::from(idle && session.in_process.is_none());
        let expected = policy == HourCache::AfterIdle && !session.one_hour && idle;
        let outlived = !session.one_hour && policy.outlived(last, now);
        if outlived != expected {
            return Err(format!(
                "policy {policy:?}, last request {last:?} at unix {now}: the 1-hour switch is {outlived}, but a session idle {} for over {lifetime}s must switch only under AfterIdle (expected {expected})",
                session.stored.map_or(0, |at| now - at)
            ));
        }
        if outlived {
            session.one_hour = true;
            stats.switched += 1;
        }
        let hour = session.one_hour || policy == HourCache::Always;
        if policy == HourCache::Never && hour {
            return Err("a Never policy session wrote a 1-hour cache entry".to_owned());
        }
        session.stored = Some(now);
        session.in_process = Some(now);
        stats.requests += 1;
        digest = fnv(digest, &[u8::from(hour), u8::from(restarted)]);
    }
    Ok(digest)
}

pub struct CachePolicyOutcome {
    digest: u64,
    wrong: Option<String>,
    stats: CachePolicyStats,
}

pub fn simulate(seed: u64, mutation: CachePolicyMutation) -> CachePolicyOutcome {
    mutation.install();
    let mut stats = CachePolicyStats {
        seeds: 1,
        ..CachePolicyStats::default()
    };
    let verdict = run(seed, &mut stats);
    CachePolicyMutation::None.install();
    let (digest, wrong) = match verdict {
        Ok(digest) => (digest, None),
        Err(message) => (0, Some(message)),
    };
    CachePolicyOutcome {
        digest,
        wrong,
        stats,
    }
}

pub fn judge(
    seed: u64,
    mutation: CachePolicyMutation,
    twice: bool,
    verbose: bool,
    totals: &mut CachePolicyStats,
) -> Option<String> {
    let first = simulate(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!(
            "seed {seed} digest {:016x} stats={:?}",
            first.digest, first.stats
        );
    }
    if first.wrong.is_some() || !twice {
        return first.wrong;
    }
    let second = simulate(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "nondeterminism: digest {:016x} then {:016x}",
            first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: CachePolicyMutation) -> String {
    let mut command = format!("nanocode-sim --scenario cache_policy --seed {seed}");
    if mutation != CachePolicyMutation::None {
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
    mutation: CachePolicyMutation,
    totals: &CachePolicyStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=cache_policy mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        CachePolicyMutation::None if failed == 0 => ExitCode::SUCCESS,
        CachePolicyMutation::None => ExitCode::FAILURE,
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
