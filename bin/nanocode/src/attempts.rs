use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::{Duration, Instant},
};

use nanocode_executor::process::{Child, Command};
use nanocode_sync::{StreamExt, unbounded, watch};

use crate::{
    cli::{AttemptPickArg, SessionArgs},
    host::Result,
};

type Entries = BTreeMap<PathBuf, Entry>;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Entry {
    Directory(fs::Permissions),
    File(fs::Permissions, u64),
    Link(PathBuf),
}

enum Exit {
    Exited(ExitStatus),
    Stopped(Duration),
    Cancelled(String),
}

struct Finished {
    answer: String,
    elapsed: Duration,
    exit: Exit,
}

#[derive(Clone, Copy)]
struct Counts {
    passed: u64,
    total: u64,
}

struct Checked {
    counts: Option<Counts>,
    elapsed: Duration,
    exit: Exit,
}

struct Changes {
    removed: Vec<PathBuf>,
    written: Vec<PathBuf>,
}

#[derive(Eq, PartialEq)]
enum Output {
    Answer(String),
    Files(Vec<(PathBuf, Option<Entry>, Vec<u8>)>),
}

const SELF_CHECK: &str = "Before you end, find the checks this task gives or implies (its tests, a checker or an example with expected output, each requirement it states), run them in the copy and fix what fails. End the reply with one line of the form `check: <command>`, where the command is one shell line that runs those checks from the copy's directory, exits 0 only when every check passes and prints its counts as a test runner's summary (such as `4 passed, 1 failed`) or as a last line `<passed>/<total>`. That command runs again in the copy after you end, and its own exit and counts decide whether this attempt is kept.";

pub async fn run(args: &SessionArgs, count: u64, piped: Option<String>) -> Result<()> {
    if count < 2 {
        return Err("--attempts takes 2 or more attempts".into());
    }
    if args.resume.is_some() || args.continue_last {
        return Err("--attempts starts new sessions and cannot resume one".into());
    }
    let pick = args.attempt_pick.unwrap_or(AttemptPickArg::Verifier);
    let original = match &args.workspace {
        Some(path) => fs::canonicalize(path)?,
        None => std::env::current_dir()?,
    };
    let mut name = original
        .file_name()
        .ok_or("--attempts needs a workspace inside a parent directory")?
        .to_owned();
    name.push(format!(".attempts-{}", std::process::id()));
    let root = original.with_file_name(name);
    let labels: Vec<String> = (1..=count).map(|number| number.to_string()).collect();
    let inputs: Vec<(String, String)> = labels
        .iter()
        .map(|label| {
            let copy = root.join(label);
            let mut note = format!(
                "The working directory {copy} is a private copy of {original} for this attempt. Make every change in the copy: where the task names a path under {original}, use the same path under {copy}.",
                copy = copy.display(),
                original = original.display(),
            );
            if pick == AttemptPickArg::SelfCheck {
                note.push(' ');
                note.push_str(SELF_CHECK);
            }
            (label.clone(), joined(piped.as_deref(), &note))
        })
        .collect();
    let started = Instant::now();
    blocking({
        let (original, root) = (original.clone(), root.clone());
        move || {
            fs::create_dir(&root)?;
            copy_tree(&original, &root.join("base"))?;
            for (label, input) in &inputs {
                copy_tree(&original, &root.join(label))?;
                fs::write(root.join(format!("{label}.in")), input)?;
            }
            Ok(())
        }
    })
    .await?;
    eprintln!(
        "nanocode: copied {} {count} times into {} in {:.1?}",
        original.display(),
        root.display(),
        started.elapsed()
    );
    let deadline = args.attempt_timeout.map(Duration::from_secs);
    let started = Instant::now();
    let (stop, stopping) = watch::channel(None);
    let (sender, mut arrivals) = unbounded::channel();
    for (index, label) in labels.iter().enumerate() {
        let sender = sender.clone();
        let run = attempt(
            root.clone(),
            root.join(label),
            label.clone(),
            deadline,
            stopping.clone(),
        );
        nanocode_platform::spawn_task(async move { drop(sender.send((index, run.await))) });
    }
    drop(sender);
    let mut arrived: Vec<Option<Finished>> = labels.iter().map(|_| None).collect();
    let mut passed = None;
    while let Some((index, done)) = arrivals.next().await {
        let done: Finished = done?;
        let label = &labels[index];
        eprintln!(
            "nanocode: attempt {label} {} after {:.1?}",
            done.exit.describe(),
            done.elapsed
        );
        if pick == AttemptPickArg::SelfCheck && passed.is_none() {
            match check_command(&done.answer) {
                Some(command) => {
                    let checked = check(&root, label, command, deadline).await?;
                    let counts = match checked.counts {
                        Some(counts) => format!(", counting {}/{}", counts.passed, counts.total),
                        None => ", printing no counts".to_owned(),
                    };
                    eprintln!(
                        "nanocode: attempt {label}'s check `{command}` {} after {:.1?}{counts}",
                        checked.exit.describe(),
                        checked.elapsed
                    );
                    if checked.passes() {
                        passed = Some(index);
                        drop(stop.send(Some(label.clone())));
                    }
                }
                None => eprintln!("nanocode: attempt {label} named no check command"),
            }
        }
        arrived[index] = Some(done);
    }
    let mut finished = arrived
        .into_iter()
        .map(|done| done.ok_or("an attempt ended without reporting how it finished"))
        .collect::<std::result::Result<Vec<Finished>, _>>()?;
    eprintln!(
        "nanocode: {count} attempts finished in {:.1?}",
        started.elapsed()
    );
    let kept = format!(
        "nothing was applied; the attempts stay in {}",
        root.display()
    );
    let chosen = match pick {
        AttemptPickArg::Verifier => {
            let answers: Vec<String> = finished.iter().map(|done| done.answer.clone()).collect();
            let found = blocking({
                let root = root.clone();
                move || outputs(&root, &answers)
            })
            .await?;
            match majority(&found) {
                Some(members) => {
                    let chosen = u64::try_from(members[0] + 1)?;
                    let numbers: Vec<String> =
                        members.iter().map(|index| labels[*index].clone()).collect();
                    eprintln!(
                        "nanocode: attempts {} left the same output, so attempt {chosen} was chosen without the verifier",
                        numbers.join(", ")
                    );
                    chosen
                }
                None => {
                    verify(
                        &original,
                        &root,
                        &labels,
                        &finished,
                        piped.as_deref(),
                        deadline,
                        &kept,
                    )
                    .await?
                }
            }
        }
        AttemptPickArg::SelfCheck => {
            let index = passed.ok_or_else(|| {
                format!("no attempt's check command passed every check it counted; {kept}")
            })?;
            let chosen = u64::try_from(index + 1)?;
            eprintln!(
                "nanocode: self-check picked attempt {chosen}, whose check command passed every check it counted"
            );
            chosen
        }
    };
    let changes = blocking({
        let (original, root) = (original.clone(), root.clone());
        move || {
            let base = root.join("base");
            let before = entries(&base)?;
            if !changes(&base, &before, &original, &entries(&original)?)?.is_empty() {
                return Err(io::Error::other(format!(
                    "{} changed while the attempts ran, so an attempt wrote outside its copy",
                    original.display()
                )));
            }
            let winner = root.join(chosen.to_string());
            let after = entries(&winner)?;
            let changes = changes(&base, &before, &winner, &after)?;
            apply(&winner, &after, &original, &before, &changes)?;
            Ok(changes)
        }
    })
    .await
    .map_err(|error| format!("{error}; {kept}"))?;
    eprintln!(
        "nanocode: applied attempt {chosen} to {}: {} written, {} removed",
        original.display(),
        changes.written.len(),
        changes.removed.len()
    );
    let answer = std::mem::take(&mut finished[usize::try_from(chosen - 1)?].answer);
    blocking(move || fs::remove_dir_all(root)).await?;
    print!("{answer}");
    Ok(())
}

async fn verify(
    original: &Path,
    root: &Path,
    labels: &[String],
    finished: &[Finished],
    piped: Option<&str>,
    deadline: Option<Duration>,
    kept: &str,
) -> Result<u64> {
    let count = labels.len();
    let mut report = format!(
        "The task above ran as {count} independent attempts, each in its own copy of {original}. In the current directory {root}, base holds the copy taken before the attempts started and the directories 1 to {count} hold each attempt's result; {original} itself is unchanged. Below are each attempt's exit, final answer and diff against base, and <number>.err holds its log. Check the attempts by running commands in their directories, such as the task's tests, without changing what they hold. Then pick the attempt that best completes the task: its changes are applied to {original}. End the reply with one line of the form `chosen: <number>`.\n",
        original = original.display(),
        root = root.display(),
    );
    for (label, done) in labels.iter().zip(finished) {
        let diff = diff(root, label).await?;
        report.push_str(&format!(
            "\n## Attempt {label}\n\n{} after {:.1?}.\n\n### Final answer\n\n{}\n\n### Diff against base\n\n```diff\n{diff}```\n",
            done.exit.describe(),
            done.elapsed,
            done.answer.trim_end(),
        ));
    }
    blocking({
        let (input, report) = (root.join("verifier.in"), joined(piped, &report));
        move || fs::write(input, report)
    })
    .await?;
    let (_stop, stopping) = watch::channel(None);
    let verdict = attempt(
        root.to_owned(),
        root.to_owned(),
        "verifier".to_owned(),
        deadline,
        stopping,
    )
    .await?;
    match verdict.exit {
        Exit::Exited(status) if status.success() => {}
        exit => {
            return Err(format!(
                "the verifier {} ({}/verifier.err); {kept}",
                exit.describe(),
                root.display()
            )
            .into());
        }
    }
    let chosen = verdict
        .answer
        .lines()
        .rev()
        .find_map(|line| line.trim().trim_matches('`').strip_prefix("chosen:"))
        .and_then(|number| number.trim().parse::<usize>().ok())
        .filter(|number| (1..=count).contains(number))
        .ok_or_else(|| {
            format!(
                "the verifier's reply ends with no chosen attempt between 1 and {count}; {kept}"
            )
        })?;
    eprintln!(
        "nanocode: the verifier chose attempt {chosen} after {:.1?}",
        verdict.elapsed
    );
    Ok(u64::try_from(chosen)?)
}

fn outputs(root: &Path, answers: &[String]) -> io::Result<Vec<Option<Output>>> {
    let base = root.join("base");
    let before = entries(&base)?;
    let mut changed = Vec::new();
    for number in 1..=answers.len() {
        let copy = root.join(number.to_string());
        let after = entries(&copy)?;
        let found = changes(&base, &before, &copy, &after)?;
        changed.push((copy, after, found));
    }
    let touched = |found: &Changes, path: &PathBuf| {
        found.written.contains(path) || found.removed.contains(path)
    };
    let common: Vec<PathBuf> = match changed.first() {
        Some((_, _, first)) => first
            .written
            .iter()
            .chain(&first.removed)
            .filter(|path| changed.iter().all(|(_, _, found)| touched(found, path)))
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    changed
        .iter()
        .zip(answers)
        .map(|((copy, after, _), answer)| {
            if common.is_empty() {
                let answer = answer.trim();
                return Ok((!answer.is_empty()).then(|| Output::Answer(answer.to_owned())));
            }
            let mut files = Vec::new();
            for path in &common {
                let entry = after.get(path).cloned();
                let content = match entry {
                    Some(Entry::File(..)) => fs::read(copy.join(path))?,
                    _ => Vec::new(),
                };
                files.push((path.clone(), entry, content));
            }
            Ok(Some(Output::Files(files)))
        })
        .collect()
}

fn majority(found: &[Option<Output>]) -> Option<Vec<usize>> {
    let mut best: Vec<usize> = Vec::new();
    let mut tied = false;
    for (index, output) in found.iter().enumerate() {
        let Some(output) = output else {
            continue;
        };
        let members: Vec<usize> = found
            .iter()
            .enumerate()
            .filter(|(_, other)| other.as_ref() == Some(output))
            .map(|(member, _)| member)
            .collect();
        if members.first() != Some(&index) {
            continue;
        }
        match members.len().cmp(&best.len()) {
            std::cmp::Ordering::Greater => {
                best = members;
                tied = false;
            }
            std::cmp::Ordering::Equal => tied = true,
            std::cmp::Ordering::Less => {}
        }
    }
    (best.len() >= 2 && !tied).then_some(best)
}

fn check_command(answer: &str) -> Option<&str> {
    answer
        .lines()
        .rev()
        .find_map(|line| line.trim().trim_matches('`').strip_prefix("check:"))
        .map(str::trim)
        .filter(|command| !command.is_empty())
}

fn counts(output: &str) -> Option<Counts> {
    output.lines().rev().find_map(|line| {
        if let Some((passed, total)) = line.trim().split_once('/')
            && let (Ok(passed), Ok(total)) = (passed.trim().parse(), total.trim().parse())
        {
            return Some(Counts { passed, total }).filter(|counts| counts.passed <= counts.total);
        }
        let words: Vec<&str> = line
            .split(|character: char| !character.is_ascii_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect();
        let (mut passed, mut failed, mut found) = (0u64, 0u64, false);
        for pair in words.windows(2) {
            let Ok(number) = pair[0].parse::<u64>() else {
                continue;
            };
            let word = pair[1].to_ascii_lowercase();
            if word == "passed" {
                passed += number;
            } else if ["failed", "error", "errors"].contains(&word.as_str()) {
                failed += number;
            } else {
                continue;
            }
            found = true;
        }
        found.then_some(Counts {
            passed,
            total: passed + failed,
        })
    })
}

async fn check(
    root: &Path,
    label: &str,
    command: &str,
    deadline: Option<Duration>,
) -> Result<Checked> {
    let output = root.join(format!("{label}.check"));
    let (stdout, stderr) = blocking({
        let output = output.clone();
        move || {
            let file = File::create(output)?;
            Ok((file.try_clone()?, file))
        }
    })
    .await?;
    let shell = chosen()?;
    let started = Instant::now();
    let mut child = Command::new(shell.path())
        .args(shell.args(command, false))
        .current_dir(root.join(label))
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()?;
    let (_stop, stopping) = watch::channel(None);
    let ended = nanocode_sync::select! {
        status = child.wait() => Ok(status),
        exit = halted(deadline, stopping) => Err(exit),
    };
    let exit = match ended {
        Ok(status) => Exit::Exited(status?),
        Err(exit) => {
            stop(&mut child).await?;
            exit
        }
    };
    let elapsed = started.elapsed();
    let printed = blocking(move || fs::read(output)).await?;
    Ok(Checked {
        counts: counts(&String::from_utf8_lossy(&printed)),
        elapsed,
        exit,
    })
}

impl Checked {
    fn passes(&self) -> bool {
        self.exit.succeeded()
            && self
                .counts
                .is_some_and(|counts| counts.total > 0 && counts.passed == counts.total)
    }
}

impl Exit {
    fn describe(&self) -> String {
        match self {
            Self::Exited(status) => format!("ended with {status}"),
            Self::Stopped(limit) => format!("stopped at the {limit:?} --attempt-timeout"),
            Self::Cancelled(winner) => {
                format!("stopped once attempt {winner} passed its check")
            }
        }
    }

    fn succeeded(&self) -> bool {
        matches!(self, Self::Exited(status) if status.success())
    }
}

impl Changes {
    const fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.written.is_empty()
    }
}

fn joined(piped: Option<&str>, note: &str) -> String {
    match piped {
        Some(piped) => format!("{piped}\n\n{note}"),
        None => note.to_owned(),
    }
}

fn forwarded() -> Vec<OsString> {
    let mut given = std::env::args_os().skip(1);
    let mut kept = Vec::new();
    while let Some(argument) = given.next() {
        let text = argument.to_string_lossy();
        if [
            "--attempts",
            "--attempt-timeout",
            "--attempt-pick",
            "--cd",
            "-C",
        ]
        .contains(&text.as_ref())
        {
            given.next();
            continue;
        }
        if [
            "--attempts=",
            "--attempt-timeout=",
            "--attempt-pick=",
            "--cd=",
            "-C",
        ]
        .iter()
        .any(|prefix| text.starts_with(prefix))
        {
            continue;
        }
        kept.push(argument);
    }
    kept
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> Result<T> {
    Ok(nanocode_platform::spawn_blocking(work).await??)
}

async fn attempt(
    root: PathBuf,
    directory: PathBuf,
    label: String,
    deadline: Option<Duration>,
    stopping: watch::Receiver<Option<String>>,
) -> Result<Finished> {
    let output = root.join(format!("{label}.out"));
    let (stdin, stdout, stderr) = blocking({
        let (root, label, output) = (root.clone(), label.clone(), output.clone());
        move || {
            Ok((
                File::open(root.join(format!("{label}.in")))?,
                File::create(output)?,
                File::create(root.join(format!("{label}.err")))?,
            ))
        }
    })
    .await?;
    let shell = chosen()?;
    let prefix = shell
        .arguments()
        .strip_suffix(&[nanocode_psh::ARGUMENT.to_owned()])
        .ok_or("the chosen shell is not nanocode's psh")?;
    let started = Instant::now();
    let mut child = Command::new(shell.path())
        .args(prefix)
        .args(forwarded())
        .current_dir(&directory)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()?;
    let ended = nanocode_sync::select! {
        status = child.wait() => Ok(status),
        exit = halted(deadline, stopping) => Err(exit),
    };
    let exit = match ended {
        Ok(status) => Exit::Exited(status?),
        Err(exit) => {
            stop(&mut child).await?;
            exit
        }
    };
    let elapsed = started.elapsed();
    let answer = blocking(move || fs::read_to_string(output)).await?;
    Ok(Finished {
        answer,
        elapsed,
        exit,
    })
}

#[cfg(target_os = "linux")]
async fn stop(child: &mut Child) -> Result<()> {
    if let Some(id) = child.id() {
        let id = i32::try_from(id)?;
        let spawned = blocking(move || stopped_tree(id)).await?;
        for process in spawned {
            kill_spawned(process)?;
        }
    }
    Ok(child.kill().await?)
}

#[cfg(not(target_os = "linux"))]
async fn stop(child: &mut Child) -> Result<()> {
    Ok(child.kill().await?)
}

#[cfg(target_os = "linux")]
fn stopped_tree(id: i32) -> io::Result<Vec<i32>> {
    let gone = |error: &io::Error| error.raw_os_error() == Some(nanocode_sys::libc::ESRCH);
    match nanocode_platform::unix::signal_process(id, nanocode_sys::libc::SIGSTOP) {
        Err(error) if gone(&error) => return Ok(Vec::new()),
        result => result?,
    }
    let mut found = Vec::new();
    loop {
        let spawned: Vec<i32> = parents()?
            .into_iter()
            .filter(|(process, parent)| {
                (*parent == id || found.contains(parent)) && !found.contains(process)
            })
            .map(|(process, _)| process)
            .collect();
        if spawned.is_empty() {
            return Ok(found);
        }
        for process in spawned {
            match nanocode_platform::unix::signal_process(process, nanocode_sys::libc::SIGSTOP) {
                Err(error) if gone(&error) => {}
                result => result?,
            }
            found.push(process);
        }
    }
}

#[cfg(target_os = "linux")]
fn parents() -> io::Result<Vec<(i32, i32)>> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(process) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            stat => stat?,
        };
        let parent = stat
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().nth(1))
            .ok_or_else(|| io::Error::other(format!("/proc/{process}/stat has no parent field")))?
            .parse()
            .map_err(io::Error::other)?;
        found.push((process, parent));
    }
    Ok(found)
}

#[cfg(target_os = "linux")]
fn kill_spawned(id: i32) -> io::Result<()> {
    let gone = |error: &io::Error| error.raw_os_error() == Some(nanocode_sys::libc::ESRCH);
    match nanocode_platform::unix::signal_group(id, nanocode_sys::libc::SIGKILL) {
        Err(error) if gone(&error) => {
            match nanocode_platform::unix::signal_process(id, nanocode_sys::libc::SIGKILL) {
                Err(error) if gone(&error) => Ok(()),
                result => result,
            }
        }
        result => result,
    }
}

async fn halted(deadline: Option<Duration>, mut stopping: watch::Receiver<Option<String>>) -> Exit {
    let limit = async {
        match deadline {
            Some(limit) => {
                nanocode_platform::sleep(limit).await;
                limit
            }
            None => std::future::pending().await,
        }
    };
    let winner = async {
        if let Ok(Some(winner)) = stopping
            .wait_for(Option::is_some)
            .await
            .map(|winner| winner.clone())
        {
            return winner;
        }
        std::future::pending().await
    };
    nanocode_sync::select! {
        limit = limit => Exit::Stopped(limit),
        winner = winner => Exit::Cancelled(winner),
    }
}

async fn diff(root: &Path, label: &str) -> Result<String> {
    let shell = chosen()?;
    let output = Command::new(shell.path())
        .args(shell.args(&format!("diff -ruN base {label}"), false))
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .await?;
    match output.status.code() {
        Some(0 | 1) => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        _ => Err(format!(
            "diff of attempt {label} ended with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        )
        .into()),
    }
}

fn chosen() -> Result<nanocode_home::shell::Shell> {
    Ok(nanocode_home::shell::chosen().ok_or("no shell was chosen")?)
}

fn entries(root: &Path) -> io::Result<Entries> {
    let mut found = Entries::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        for entry in fs::read_dir(root.join(&relative))? {
            let entry = entry?;
            let path = relative.join(entry.file_name());
            let metadata = fs::symlink_metadata(entry.path())?;
            let kind = metadata.file_type();
            let described = if kind.is_dir() {
                pending.push(path.clone());
                Entry::Directory(metadata.permissions())
            } else if kind.is_file() {
                Entry::File(metadata.permissions(), metadata.len())
            } else if kind.is_symlink() {
                Entry::Link(fs::read_link(entry.path())?)
            } else {
                return Err(io::Error::other(format!(
                    "{} is not a file, a directory or a symbolic link",
                    entry.path().display()
                )));
            };
            found.insert(path, described);
        }
    }
    Ok(found)
}

fn changes(from_root: &Path, from: &Entries, to_root: &Path, to: &Entries) -> io::Result<Changes> {
    let removed = from
        .keys()
        .filter(|path| !to.contains_key(*path))
        .cloned()
        .collect();
    let mut written = Vec::new();
    for (path, entry) in to {
        let same = match (from.get(path), entry) {
            (Some(old), Entry::File(..)) if old == entry => {
                fs::read(from_root.join(path))? == fs::read(to_root.join(path))?
            }
            (Some(old), _) => old == entry,
            (None, _) => false,
        };
        if !same {
            written.push(path.clone());
        }
    }
    Ok(Changes { removed, written })
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir(to)?;
    let found = entries(from)?;
    for (path, entry) in &found {
        write(&from.join(path), &to.join(path), entry)?;
    }
    for (path, entry) in found.iter().rev() {
        if let Entry::Directory(permissions) = entry {
            fs::set_permissions(to.join(path), permissions.clone())?;
        }
    }
    fs::set_permissions(to, fs::metadata(from)?.permissions())
}

fn apply(
    winner_root: &Path,
    winner: &Entries,
    original_root: &Path,
    before: &Entries,
    changes: &Changes,
) -> io::Result<()> {
    let mut gone: Vec<&PathBuf> = Vec::new();
    for path in &changes.removed {
        if gone.iter().any(|removed| path.starts_with(removed)) {
            continue;
        }
        remove(&original_root.join(path), &before[path])?;
        gone.push(path);
    }
    let mut directories = Vec::new();
    for path in &changes.written {
        let entry = &winner[path];
        let target = original_root.join(path);
        match (before.get(path), entry) {
            (Some(Entry::Directory(_)), Entry::Directory(_)) => {}
            (Some(old), _) => {
                remove(&target, old)?;
                write(&winner_root.join(path), &target, entry)?;
            }
            (None, _) => write(&winner_root.join(path), &target, entry)?,
        }
        if let Entry::Directory(permissions) = entry {
            directories.push((target, permissions.clone()));
        }
    }
    for (target, permissions) in directories.into_iter().rev() {
        fs::set_permissions(target, permissions)?;
    }
    Ok(())
}

fn remove(target: &Path, entry: &Entry) -> io::Result<()> {
    match entry {
        Entry::Directory(_) => fs::remove_dir_all(target),
        Entry::File(..) | Entry::Link(_) => fs::remove_file(target),
    }
}

fn write(from: &Path, to: &Path, entry: &Entry) -> io::Result<()> {
    match entry {
        Entry::Directory(_) => fs::create_dir(to),
        Entry::File(..) => fs::copy(from, to).map(drop),
        Entry::Link(target) => link(target, to),
    }
}

#[cfg(unix)]
fn link(target: &Path, to: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, to)
}

#[cfg(not(unix))]
fn link(_: &Path, to: &Path) -> io::Result<()> {
    Err(io::Error::other(format!(
        "--attempts copies symbolic links only on Unix: {}",
        to.display()
    )))
}
