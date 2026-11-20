use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::{self, BufRead, Read, Seek, Write},
    path::PathBuf,
    process::{Command, ExitCode},
    time::{SystemTime, UNIX_EPOCH},
};

use nanocode_build_opt::{
    graph::Graph,
    plan::{self, Rules, Scope},
    schedule::{self, Job, Shard},
    slots::{self, Machine, Observation, Pin, Placement, Pool, State},
};

const USAGE: &str = "usage:
  nc-build affected [--base REF]      crates the diff since REF (and untracked files) touches, with their dependents; all when a manifest, the lockfile or .cargo changed
  nc-build shard K/N [--durations F]  reads job keys on stdin and prints this shard's keys, longest first (F: lines of key and milliseconds)
  nc-build slots take --cores N --label L [--pool large|small] [--machine M] [--minutes T] [--wait S]
                                      takes N cores on the first machine with room, filling machines in order, and prints the lease as JSON;
                                      the large pool (default, nanocode work) fills the largest boxes first, the small pool (pyramid2 work)
                                      the smaller boxes and dev-georgios first; gak-9 only with --machine gak-9
  nc-build slots give LEASE | slots renew LEASE [--minutes T]
  nc-build slots observe MACHINE      reads nproc, /proc/loadavg and /proc/meminfo from that machine on stdin
  nc-build slots list                 each machine's busy cores, load, memory and expiry, also appended to machines.log
  state: $NC_BUILD_SLOTS (~/swarm/slots.json); machines: $NC_BUILD_BOXES (~/swarm/boxes.json), dev-georgios only with a fresh observation, gak-9 by name";

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("affected") => affected(&args[1..]),
        Some("shard") => shard(&args[1..]),
        Some("slots") => slots_command(&args[1..]),
        _ => Err(USAGE.to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            let _ = writeln!(io::stderr(), "{message}");
            ExitCode::from(2)
        }
    }
}

fn option<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|at| args.get(at + 1))
        .map(String::as_str)
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("{program}: {error}"))
}

fn affected(args: &[String]) -> Result<(), String> {
    let base = option(args, "--base").unwrap_or("HEAD");
    let metadata = run(
        "cargo",
        &["metadata", "--format-version", "1", "--locked", "--no-deps"],
    )?;
    let graph = Graph::from_metadata(&metadata).map_err(|error| format!("{error:?}"))?;
    let mut changed: BTreeSet<String> = BTreeSet::new();
    for listing in [
        run("git", &["diff", "--name-only", base, "--"])?,
        run("git", &["ls-files", "--others", "--exclude-standard"])?,
    ] {
        changed.extend(
            listing
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned),
        );
    }
    let changed: Vec<String> = changed.into_iter().collect();
    let mut out = io::stdout().lock();
    match plan::affected(&graph, &Rules::cargo(), &changed) {
        Scope::Everything => writeln!(out, "all"),
        Scope::Crates(crates) => crates.iter().try_for_each(|name| writeln!(out, "{name}")),
    }
    .map_err(|error| error.to_string())
}

fn shard(args: &[String]) -> Result<(), String> {
    let shard = args
        .first()
        .map(|text| Shard::parse(text))
        .transpose()
        .map_err(|error| format!("{error:?}"))?
        .unwrap_or(Shard::WHOLE);
    let durations = match option(args, "--durations") {
        Some(path) => std::fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?,
        None => String::new(),
    };
    let expected = |key: &str| {
        durations.lines().find_map(|line| {
            let (name, ms) = line.rsplit_once(char::is_whitespace)?;
            (name.trim() == key)
                .then(|| ms.trim().parse().ok())
                .flatten()
        })
    };
    let jobs: Vec<Job> = io::stdin()
        .lock()
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .map(|key| Job {
            expected_ms: expected(&key),
            key,
        })
        .collect();
    let mut out = io::stdout().lock();
    schedule::plan(jobs, shard)
        .iter()
        .try_for_each(|job| writeln!(out, "{}", job.key))
        .map_err(|error| error.to_string())
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

fn machines(pool: Pool) -> Vec<Machine> {
    let path = std::env::var_os("NC_BUILD_BOXES")
        .map_or_else(|| home().join("swarm/boxes.json"), PathBuf::from);
    let boxes = std::fs::read_to_string(path).unwrap_or_default();
    let gak = Machine {
        name: "gak-9".to_owned(),
        cores: 6,
        expires_ms: None,
        pin: Pin::Nice,
        placement: Placement::Named,
    };
    let dev = Machine {
        name: "dev-georgios".to_owned(),
        cores: 32,
        expires_ms: None,
        pin: Pin::Taskset,
        placement: Placement::LastResort,
    };
    slots::machines_from_boxes(&boxes, pool, &[gak], &[dev])
}

fn with_state<T>(change: impl FnOnce(&mut State) -> T) -> Result<T, String> {
    let path = std::env::var_os("NC_BUILD_SLOTS")
        .map_or_else(|| home().join("swarm/slots.json"), PathBuf::from);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    file.lock().map_err(|error| error.to_string())?;
    let mut json = String::new();
    file.read_to_string(&mut json)
        .map_err(|error| error.to_string())?;
    let mut state =
        State::from_json(&json).ok_or_else(|| format!("{}: not a slots state", path.display()))?;
    let result = change(&mut state);
    let json = state.to_json();
    file.set_len(0).map_err(|error| error.to_string())?;
    file.rewind().map_err(|error| error.to_string())?;
    file.write_all(json.as_bytes())
        .map_err(|error| error.to_string())?;
    Ok(result)
}

fn print(value: &serde_json::Value) -> Result<(), String> {
    writeln!(io::stdout().lock(), "{value}").map_err(|error| error.to_string())
}

fn minutes(args: &[String], default: u64) -> Result<u64, String> {
    option(args, "--minutes")
        .map_or(Ok(default), str::parse)
        .map(|minutes: u64| minutes * 60_000)
        .map_err(|error| format!("--minutes: {error}"))
}

fn slots_command(args: &[String]) -> Result<(), String> {
    let pool = match option(args, "--pool") {
        None | Some("large" | "nanocode") => Pool::Large,
        Some("small" | "pyramid2") => Pool::Small,
        Some(other) => return Err(format!("--pool {other}: large or small")),
    };
    let machines = machines(pool);
    match args.first().map(String::as_str) {
        Some("take") => {
            let cores: u32 = option(args, "--cores")
                .ok_or("--cores N")?
                .parse()
                .map_err(|error| format!("--cores: {error}"))?;
            let label = option(args, "--label").ok_or("--label L")?;
            let ttl = minutes(args, 60)?;
            let wait_ms = option(args, "--wait")
                .map_or(Ok(0), str::parse::<u64>)
                .map_err(|error| format!("--wait: {error}"))?
                * 1000;
            let only = option(args, "--machine");
            let started = now_ms();
            loop {
                let now = now_ms();
                let lease =
                    with_state(|state| state.take(&machines, cores, label, only, now, ttl))?;
                if let Some(lease) = lease {
                    let machine = machines
                        .iter()
                        .find(|machine| machine.name == lease.machine);
                    log_line(&format!(
                        "take {} {} {} cores {} cpus {}",
                        lease.id,
                        lease.machine,
                        lease.cores,
                        lease.label,
                        lease.cpus()
                    ));
                    return print(&lease.to_json(machine));
                }
                if now.saturating_sub(started) >= wait_ms {
                    return Err(format!("no machine has {cores} free cores"));
                }
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        }
        Some("give") => {
            let id = args.get(1).ok_or("give LEASE")?;
            let lease = with_state(|state| state.give(id))?.ok_or("no such lease")?;
            log_line(&format!(
                "give {} {} {} cores {} after {}s",
                lease.id,
                lease.machine,
                lease.cores,
                lease.label,
                now_ms().saturating_sub(lease.taken_ms) / 1000
            ));
            Ok(())
        }
        Some("renew") => {
            let id = args.get(1).ok_or("renew LEASE")?;
            let until = now_ms() + minutes(args, 60)?;
            let lease =
                with_state(|state| state.renew(id, until).cloned())?.ok_or("no such lease")?;
            let machine = machines
                .iter()
                .find(|machine| machine.name == lease.machine);
            print(&lease.to_json(machine))
        }
        Some("observe") => {
            let name = args.get(1).ok_or("observe MACHINE")?;
            let mut input = String::new();
            io::stdin()
                .read_to_string(&mut input)
                .map_err(|error| error.to_string())?;
            let observation = parse_observation(&input, now_ms())
                .ok_or("expected nproc, /proc/loadavg and /proc/meminfo on stdin")?;
            with_state(|state| {
                state.observations.insert(name.clone(), observation);
            })?;
            list(&machines)
        }
        Some("list") => list(&machines),
        _ => Err(USAGE.to_owned()),
    }
}

fn parse_observation(input: &str, at_ms: u64) -> Option<Observation> {
    let mut cores = None;
    let mut load = None;
    let mut total = None;
    let mut available = None;
    for line in input.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            [count] if cores.is_none() => cores = count.parse().ok(),
            [one, _, _, _, _] if load.is_none() => load = one.parse().ok(),
            ["MemTotal:", kb, ..] => total = kb.parse().ok(),
            ["MemAvailable:", kb, ..] => available = kb.parse().ok(),
            _ => {}
        }
    }
    Some(Observation {
        load: load?,
        cores: cores?,
        mem_total_kb: total?,
        mem_available_kb: available?,
        at_ms,
    })
}

fn log_line(line: &str) {
    let path = std::env::var_os("NC_BUILD_MACHINES_LOG")
        .map_or_else(|| home().join("swarm/machines.log"), PathBuf::from);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {line}", now_ms() / 1000);
    }
}

fn list(machines: &[Machine]) -> Result<(), String> {
    let now = now_ms();
    let state = with_state(|state| {
        state.expire(now);
        state.clone()
    })?;
    let mut out = io::stdout().lock();
    let (mut busy_total, mut cores_total) = (0, 0);
    for machine in machines {
        let usage = state.usage(machine, now);
        let observation = state.observations.get(&machine.name);
        let memory = observation.map_or("mem ?".to_owned(), |observation| {
            format!(
                "mem {}% free",
                observation.mem_available_kb * 100 / observation.mem_total_kb.max(1)
            )
        });
        let load = observation.map_or("load ?".to_owned(), |observation| {
            format!(
                "load {:.1}{}",
                observation.load,
                if usage.observed { "" } else { " (stale)" }
            )
        });
        let expires = machine
            .expires_ms
            .map_or("no expiry".to_owned(), |expires| {
                format!("expires in {}m", expires.saturating_sub(now) / 60_000)
            });
        let line = format!(
            "{} {}/{} cores busy, {} leased, {load}, {memory}, {expires}",
            machine.name, usage.busy, usage.cores, usage.leased
        );
        writeln!(out, "{line}").map_err(|error| error.to_string())?;
        log_line(&format!("machine {line}"));
        busy_total += usage.busy;
        cores_total += usage.cores;
    }
    let total = format!("total {busy_total}/{cores_total} cores busy");
    log_line(&total);
    for lease in &state.leases {
        writeln!(
            out,
            "lease {} {} cpus {} {} until {}s",
            lease.id,
            lease.machine,
            lease.cpus(),
            lease.label,
            lease.until_ms.saturating_sub(now) / 1000
        )
        .map_err(|error| error.to_string())?;
    }
    writeln!(out, "{total}").map_err(|error| error.to_string())
}
