use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::{self, BufRead, Read, Seek, Write},
    path::PathBuf,
    process::{Command, ExitCode},
    time::{SystemTime, UNIX_EPOCH},
};

use nanocode_build_opt::{
    ab::{Comparison, Side, Verdict},
    graph::Graph,
    plan::{self, Rules, Scope},
    schedule::{self, Job, Shard},
    slots::{self, Machine, Observation, Pin, Placement, Pool, State},
};

const USAGE: &str = "usage:
  nc-build affected [--base REF]      crates the diff since REF (and untracked files) touches, with their dependents; all when a manifest, the lockfile or .cargo changed
  nc-build analyze [--timings DIR|FILE] [--since 30.days] [--manifest-path M] [--top 10]
                                      ranks structural build changes by predicted seconds saved, from cargo metadata, git log and cargo --timings
  nc-build shard K/N [--durations F]  reads job keys on stdin and prints this shard's keys, longest first (F: lines of key and milliseconds)
  nc-build slots take --cores N --label L [--pool large|small] [--machine M] [--minutes T] [--wait S]
                                      takes N cores on the first machine with room, filling machines in order, and prints the lease as JSON;
                                      the large pool (default, nanocode work) fills the largest boxes first, the small pool (pyramid2 work)
                                      the smaller boxes and dev-georgios first; gak-9 only with --machine gak-9
  nc-build slots give LEASE | slots renew LEASE [--minutes T]
  nc-build slots observe MACHINE [--local]
                                      reads the output of 'nproc; cat /proc/loadavg /proc/meminfo; ps -eLo policy=,stat=' from that machine
                                      on stdin (--local runs it here); busy cores are its runnable threads outside SCHED_IDLE, so preemptible
                                      filler never blocks a take; without the ps lines, the 1-minute load average
  nc-build slots list                 each machine's busy cores, load, memory and expiry, also appended to machines.log
  nc-build ab --a CMD --b CMD [--runs N] [--setup CMD] [--label L] [--post TASK [--proposal TEXT] [--board-dir D] [--board-as A]]
                                      runs A and B interleaved N times (default 5) with setup before each run, prints medians, min, max,
                                      the delta and a verdict (faster when the ranges do not overlap, else noise) as text and a JSON line,
                                      and appends it to $NC_BUILD_AB_LOG (~/swarm/ab.jsonl); --post files the winner as a board task
                                      through $NC_BOARD_BIN (nc-board) and posts nothing on noise
  state: $NC_BUILD_SLOTS (~/swarm/slots.json); machines: $NC_BUILD_BOXES (~/swarm/boxes.json), dev-georgios only with a fresh observation, gak-9 by name";

pub fn main() -> ExitCode {
    let started = std::time::Instant::now();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("check") => {
            let code = crate::check::main(&args[1..]);
            if !args
                .iter()
                .any(|arg| matches!(arg.as_str(), "--plan" | "-h" | "--help"))
            {
                metric(&args, started.elapsed().as_millis(), code, compiled());
            }
            return ExitCode::from(code);
        }
        Some("affected") => affected(&args[1..]),
        Some("shard") => shard(&args[1..]),
        Some("slots") => slots_command(&args[1..]),
        Some("ab") => ab_command(&args[1..]),
        Some("analyze") => crate::analyze::analyze(&args[1..]),
        _ => Err(USAGE.to_owned()),
    };
    let code = match result {
        Ok(()) => 0,
        Err(message) => {
            let _ = writeln!(io::stderr(), "{message}");
            2
        }
    };
    metric(&args, started.elapsed().as_millis(), code, None);
    ExitCode::from(code)
}

/// After a check: whether cargo compiled nothing, from the first lines of each output/check log, where
/// cargo prints its Compiling lines; None when no log is there.
fn compiled() -> Option<bool> {
    let logs = std::fs::read_dir("output/check").ok()?;
    let mut any = false;
    for entry in logs.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "log") {
            continue;
        }
        any = true;
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        if io::BufReader::new(file)
            .lines()
            .take(40)
            .map_while(Result::ok)
            .any(|line| {
                line.starts_with("Compiling") || line.trim_start().starts_with("Compiling ")
            })
        {
            return Some(false);
        }
    }
    any.then_some(true)
}

/// Appends one point for this invocation to the swarm dashboard's feed, in its ingest format:
/// `$NC_METRICS_LOG` (`off`: none), else ~/swarm/dash/feeds/BUILD.jsonl where that directory
/// exists, else ~/swarm/metrics/BUILD.jsonl. Best effort: it reads HEAD from .git without
/// running git, and ignores any error, so it never changes the result.
fn metric(args: &[String], wall_ms: u128, exit: u8, cached: Option<bool>) {
    let started = std::time::Instant::now();
    let path = match std::env::var_os("NC_METRICS_LOG") {
        Some(path) if path == "off" => return,
        Some(path) => PathBuf::from(path),
        None if home().join("swarm/dash/feeds").is_dir() => {
            home().join("swarm/dash/feeds/BUILD.jsonl")
        }
        None => home().join("swarm/metrics/BUILD.jsonl"),
    };
    let cmd = match args {
        [] => return,
        [group, sub, ..] if group == "slots" => format!("nc-build slots {sub}"),
        // the series scripts/check wrote before it ran this engine
        [sub, rest @ ..] if sub == "check" => {
            if rest.iter().any(|arg| arg == "--full") {
                "check --full".to_owned()
            } else {
                "check".to_owned()
            }
        }
        [sub, ..] => format!("nc-build {sub}"),
    };
    let (commit, repo) = git_head().unwrap_or_default();
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .unwrap_or_default()
        .trim()
        .to_owned();
    let point = serde_json::json!({
        "name": format!("build {repo} {cmd}"),
        "value": wall_ms,
        "unit": "ms",
        "better": "down",
        "source": "measured",
        "at": now_ms(),
        "commit": commit,
        "note": format!("{host} exit={exit}"),
        "repo": repo,
        "cmd": cmd,
        "box": host,
        "exit": exit,
        "noop": false,
        "cached": cached,
        "metric_us": started.elapsed().as_micros(),
    });
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(format!("{point}\n").as_bytes());
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

/// The current directory's checkout: HEAD's commit (12 hex digits) and the repository's name,
/// read from .git (a worktree's gitdir, its commondir, loose refs and packed-refs) without running
/// git, which costs more than the rest of the point.
fn git_head() -> Option<(String, String)> {
    let cwd = std::env::current_dir().ok()?;
    let dot = cwd
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|dot| dot.exists())?;
    let gitdir = if dot.is_file() {
        let text = std::fs::read_to_string(&dot).ok()?;
        dot.parent()?.join(text.trim().strip_prefix("gitdir: ")?)
    } else {
        dot
    };
    let common = std::fs::read_to_string(gitdir.join("commondir"))
        .map_or_else(|_| gitdir.clone(), |text| gitdir.join(text.trim()));
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head = head.trim();
    let commit = match head.strip_prefix("ref: ") {
        None => head.to_owned(),
        Some(name) => std::fs::read_to_string(gitdir.join(name))
            .or_else(|_| std::fs::read_to_string(common.join(name)))
            .ok()
            .or_else(|| {
                let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
                packed.lines().find_map(|line| {
                    let (sha, reference) = line.split_once(' ')?;
                    (reference == name).then(|| sha.to_owned())
                })
            })?,
    };
    let commit: String = commit.trim().chars().take(12).collect();
    let common = std::fs::canonicalize(&common).unwrap_or(common);
    let base = if common.ends_with(".git") {
        common.parent()?
    } else {
        common.as_path()
    };
    let name = base.file_name()?.to_str()?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    Some((commit, name.to_owned()))
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
            let input = if args.iter().any(|arg| arg == "--local") {
                run("sh", &["-c", PROBE])?
            } else {
                let mut input = String::new();
                io::stdin()
                    .read_to_string(&mut input)
                    .map_err(|error| error.to_string())?;
                input
            };
            let observation = parse_observation(&input, now_ms())
                .ok_or("expected nproc, /proc/loadavg, /proc/meminfo and optionally ps -eLo policy=,stat= on stdin")?;
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
    let mut runnable = None;
    let mut total = None;
    let mut available = None;
    for line in input.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            [count] if cores.is_none() => cores = count.parse().ok(),
            [one, _, _, _, _] if load.is_none() => load = one.parse().ok(),
            ["MemTotal:", kb, ..] => total = kb.parse().ok(),
            ["MemAvailable:", kb, ..] => available = kb.parse().ok(),
            [policy, stat] if POLICIES.contains(policy) => {
                let count = runnable.get_or_insert(0_u32);
                if *policy != "IDL" && stat.starts_with('R') {
                    *count += 1;
                }
            }
            _ => {}
        }
    }
    Some(Observation {
        load: runnable.map_or(load?, f64::from),
        cores: cores?,
        mem_total_kb: total?,
        mem_available_kb: available?,
        at_ms,
    })
}

const POLICIES: [&str; 7] = ["TS", "FF", "RR", "B", "ISO", "IDL", "DLN"];

const PROBE: &str = "nproc; cat /proc/loadavg /proc/meminfo; ps -eLo policy=,stat=";

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

fn shell(command: &str) -> Result<std::time::Duration, String> {
    let started = std::time::Instant::now();
    let output = Command::new("sh")
        .args(["-c", command])
        .output()
        .map_err(|error| format!("sh -c {command}: {error}"))?;
    let elapsed = started.elapsed();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
        return Err(format!(
            "{command}: {}\n{}",
            output.status,
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        ));
    }
    Ok(elapsed)
}

fn ab_command(args: &[String]) -> Result<(), String> {
    let a = option(args, "--a").ok_or("--a CMD")?;
    let b = option(args, "--b").ok_or("--b CMD")?;
    let runs: usize = option(args, "--runs")
        .map_or(Ok(5), str::parse)
        .map_err(|error| format!("--runs: {error}"))?;
    if runs == 0 {
        return Err("--runs must be at least 1".to_owned());
    }
    let setup = option(args, "--setup");
    let label = option(args, "--label").unwrap_or("ab");
    let mut samples = [Vec::with_capacity(runs), Vec::with_capacity(runs)];
    for run in 1..=runs {
        for (index, (side, command)) in [(Side::A, a), (Side::B, b)].into_iter().enumerate() {
            if let Some(setup) = setup {
                shell(setup)
                    .map_err(|error| format!("setup before {} run {run}: {error}", side.name()))?;
            }
            let elapsed =
                shell(command).map_err(|error| format!("{} run {run}: {error}", side.name()))?;
            let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
            let _ = writeln!(
                io::stderr(),
                "run {run}/{runs} {} {:.2}s",
                side.name(),
                elapsed.as_secs_f64()
            );
            samples[index].push(ms);
        }
    }
    let comparison = Comparison::of(&samples[0], &samples[1]).ok_or("no samples")?;
    let mut record = comparison.to_json();
    record["label"] = label.into();
    record["a_cmd"] = a.into();
    record["b_cmd"] = b.into();
    record["setup"] = setup.into();
    record["at"] = (now_ms() / 1000).into();
    record["cwd"] = std::env::current_dir()
        .map(|dir| dir.display().to_string())
        .unwrap_or_default()
        .into();
    let path = std::env::var_os("NC_BUILD_AB_LOG")
        .map_or_else(|| home().join("swarm/ab.jsonl"), PathBuf::from);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| writeln!(file, "{record}"))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    writeln!(io::stdout().lock(), "{label}\n{}", comparison.text())
        .map_err(|error| error.to_string())?;
    if let Some(task) = option(args, "--post") {
        record["posted"] = post(task, args, &comparison, a, b, label)?.into();
    }
    print(&record)
}

fn post(
    task: &str,
    args: &[String],
    comparison: &Comparison,
    a: &str,
    b: &str,
    label: &str,
) -> Result<bool, String> {
    let (winner, loser) = match comparison.verdict {
        Verdict::Faster(Side::B) => (b, a),
        Verdict::Faster(Side::A) => (a, b),
        Verdict::Noise => {
            let _ = writeln!(io::stderr(), "noise: no proposal posted");
            return Ok(false);
        }
    };
    let proposal = option(args, "--proposal").unwrap_or(label);
    let (fast, slow) = match comparison.verdict {
        Verdict::Faster(Side::A) => (&comparison.a, &comparison.b),
        _ => (&comparison.b, &comparison.a),
    };
    let spec = format!(
        "{proposal}\n\nnc-build ab measured {} interleaved runs: `{winner}` median {:.2}s (min {:.2}s, max {:.2}s) vs `{loser}` median {:.2}s (min {:.2}s, max {:.2}s), {:.1}% faster, ranges disjoint, sign test p={:.3}.",
        fast.samples_ms.len(),
        fast.median_ms / 1000.0,
        fast.min_ms as f64 / 1000.0,
        fast.max_ms as f64 / 1000.0,
        slow.median_ms / 1000.0,
        slow.min_ms as f64 / 1000.0,
        slow.max_ms as f64 / 1000.0,
        (slow.median_ms - fast.median_ms) * 100.0 / slow.median_ms.max(1.0),
        comparison.sign_p()
    );
    let board = std::env::var("NC_BOARD_BIN").unwrap_or_else(|_| "nc-board".to_owned());
    let mut command = vec!["post".to_owned(), task.to_owned()];
    for pair in ["kind=build-opt".to_owned(), format!("ab={label}")] {
        command.extend(["--label".to_owned(), pair]);
    }
    for flag in ["--board-dir", "--board-as"] {
        if let Some(value) = option(args, flag) {
            command.extend([flag.replace("board-", ""), value.to_owned()]);
        }
    }
    command.push(spec);
    let words: Vec<&str> = command.iter().map(String::as_str).collect();
    let posted = run(&board, &words)?;
    let _ = writeln!(io::stderr(), "posted {task}: {}", posted.trim());
    Ok(true)
}
