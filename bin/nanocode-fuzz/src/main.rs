macro_rules! ensure {
    ($holds:expr, $($reason:tt)+) => {
        if !$holds {
            return Err(format!($($reason)+));
        }
    };
}

mod board_fold;
mod board_sync;
mod live_fold;
mod partitions;
mod rng;

use std::cell::RefCell;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rng::Rng;

pub type Checked = Result<(), String>;

const ITERATIONS_DEFAULT: u64 = 100_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    BoardFold,
    BoardFoldSpread,
    BoardSyncIngest,
    LiveFoldDecode,
    PartitionsRow,
}

impl Target {
    const ALL: [Self; 5] = [
        Self::BoardFold,
        Self::BoardFoldSpread,
        Self::BoardSyncIngest,
        Self::LiveFoldDecode,
        Self::PartitionsRow,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::BoardFold => "board_fold",
            Self::BoardFoldSpread => "board_fold_spread",
            Self::BoardSyncIngest => "board_sync_ingest",
            Self::LiveFoldDecode => "live_fold_decode",
            Self::PartitionsRow => "partitions_row",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|target| target.name() == name)
    }

    fn check(self, rng: &mut Rng) -> Checked {
        match self {
            Self::BoardFold => board_fold::check(rng),
            Self::BoardFoldSpread => board_fold::spread(rng),
            Self::BoardSyncIngest => board_sync::check(rng),
            Self::LiveFoldDecode => live_fold::check(rng),
            Self::PartitionsRow => partitions::check(rng),
        }
    }
}

enum Count {
    Iterations(u64),
    One(u64),
}

struct Run {
    seed: u64,
    count: Count,
    jobs: usize,
    targets: Vec<Target>,
}

fn usage() -> ExitCode {
    let names: Vec<&str> = Target::ALL.iter().map(|target| target.name()).collect();
    eprintln!(
        "usage: nanocode-fuzz fuzz [--seed S] [--iterations N | --iteration I] [--jobs J] [--target {}]",
        names.join("|")
    );
    ExitCode::from(2)
}

fn parse(args: &[String]) -> Option<Run> {
    let (command, rest) = args.split_first()?;
    if command != "fuzz" {
        return None;
    }
    let mut seed = None;
    let mut count = None;
    let mut jobs = None;
    let mut target = None;
    let mut rest = rest.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next()?;
        match flag.as_str() {
            "--seed" if seed.is_none() => seed = Some(value.parse().ok()?),
            "--iterations" if count.is_none() => {
                count = Some(Count::Iterations(value.parse().ok()?));
            }
            "--iteration" if count.is_none() => count = Some(Count::One(value.parse().ok()?)),
            "--jobs" if jobs.is_none() => jobs = Some(value.parse().ok().filter(|jobs| *jobs > 0)?),
            "--target" if target.is_none() => target = Some(Target::parse(value)?),
            _ => return None,
        }
    }
    let seed = seed.unwrap_or_else(|| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is past the epoch");
        u64::try_from(now.as_nanos() % u128::from(u64::MAX)).expect("a remainder fits a u64")
    });
    let jobs = jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from));
    Some(Run {
        seed,
        count: count.unwrap_or(Count::Iterations(ITERATIONS_DEFAULT)),
        jobs,
        targets: target.map_or_else(|| Target::ALL.to_vec(), |target| vec![target]),
    })
}

thread_local! {
    static PANICKED: RefCell<String> = const { RefCell::new(String::new()) };
}

fn record_panics() {
    std::panic::set_hook(Box::new(|info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()))
            .unwrap_or_default();
        PANICKED
            .with(|panicked| *panicked.borrow_mut() = format!("panic: {message} at {location}"));
    }));
}

fn one(seed: u64, target: Target, iteration: u64) -> Checked {
    let checked = std::panic::catch_unwind(|| {
        let mut rng = Rng::new(seed, iteration, target.name());
        target.check(&mut rng)
    });
    checked.unwrap_or_else(|_| Err(PANICKED.with(|panicked| panicked.take())))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(run) = parse(&args) else {
        return usage();
    };
    record_panics();
    let (first, total) = match run.count {
        Count::Iterations(n) => (0, n),
        Count::One(i) => (i, 1),
    };
    let width = run.targets.len() as u64;
    let work = total * width;
    let cursor = AtomicU64::new(0);
    let started = Instant::now();
    let failures: Vec<String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..run.jobs.min(usize::try_from(work).unwrap_or(usize::MAX)).max(1))
            .map(|_| {
                scope.spawn(|| {
                    let mut failed = Vec::new();
                    loop {
                        let at = cursor.fetch_add(1, Ordering::Relaxed);
                        if at >= work {
                            return failed;
                        }
                        let target = run.targets[usize::try_from(at % width).expect("a target index fits")];
                        let iteration = first + at / width;
                        if let Err(reason) = one(run.seed, target, iteration) {
                            let reason = reason.replace('\n', " ");
                            let line = format!(
                                    "FAIL fuzz target={} seed={} iteration={iteration} {reason} :: replay: cargo run -q -p nanocode-fuzz --release -- fuzz --seed {} --target {} --iteration {iteration}",
                                    target.name(),
                                    run.seed,
                                    run.seed,
                                    target.name()
                            );
                            // Print as found, so a shard killed at its deadline keeps its failures.
                            println!("{line}");
                            failed.push(line);
                        }
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("a fuzz worker catches its panics"))
            .collect()
    });
    let seconds = started.elapsed().as_secs_f64();
    let names: Vec<&str> = run.targets.iter().map(|target| target.name()).collect();
    println!(
        "FUZZ iterations={work} failed={} seed={} seconds={seconds:.3} iterations_per_s={:.0} targets=[{}]",
        failures.len(),
        run.seed,
        work as f64 / seconds.max(f64::MIN_POSITIVE),
        names.join(",")
    );
    if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
