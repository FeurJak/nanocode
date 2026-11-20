use std::{
    cell::{Cell, OnceCell},
    future::Future,
    io,
    pin::Pin,
    rc::Rc,
    time::Duration,
};

use nanocode_bytes::Bytes;
use nanocode_shell::{Call, Commands, FileSystem, Reply, Session, join};

use crate::{
    Child, Ended, Entry, Exited, Files, Kind, Limit, Limits, Python, Ran, Remove, Run, Stat, Tree,
    Write,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub steps: u64,
    pub time: Duration,
    pub memory: Option<u64>,
    pub sleep: Duration,
    pub depth: usize,
}

pub struct Command {
    kept: Rc<Kept>,
    rest: Rc<dyn Commands>,
}

struct Kept {
    budget: Budget,
    python: OnceCell<Python>,
    running: Cell<bool>,
    /// A run here needed the compiled-in extensions, so every later one goes where they are.
    #[cfg(not(target_family = "wasm"))]
    native: Cell<bool>,
}

const MIB: u64 = 1 << 20;

const NAMES: [&[u8]; 4] = [
    b"python3",
    b"python",
    b"/usr/bin/python3",
    b"/usr/bin/python",
];

impl Command {
    pub fn new(budget: Budget, rest: Rc<dyn Commands>) -> Self {
        let kept = Kept {
            budget,
            python: OnceCell::new(),
            running: Cell::new(false),
            #[cfg(not(target_family = "wasm"))]
            native: Cell::new(false),
        };
        Self {
            kept: Rc::new(kept),
            rest,
        }
    }
}

impl Commands for Command {
    fn has(&self, name: &[u8]) -> bool {
        NAMES.contains(&name) || self.rest.has(name)
    }

    fn names(&self) -> Vec<Vec<u8>> {
        let mut names: Vec<Vec<u8>> = NAMES.iter().map(|name| name.to_vec()).collect();
        names.extend(self.rest.names());
        names
    }

    fn run(&self, call: Call) -> Pin<Box<dyn Future<Output = i32>>> {
        if !call
            .argv
            .first()
            .is_some_and(|name| NAMES.contains(&&**name))
        {
            return self.rest.run(call);
        }
        let kept = self.kept.clone();
        Box::pin(async move {
            if kept.running.get() {
                call.shell.lacks(b"python3");
                let said =
                    b"python3: Python is running in this shell, which keeps one interpreter\n";
                let _ = call.stderr.write_all(said).await;
                return 126;
            }
            match kept.run(&call).await {
                Ok(status) => status,
                Err(error) => {
                    let _ = call
                        .stderr
                        .write_all(format!("python3: {error}\n").as_bytes())
                        .await;
                    1
                }
            }
        })
    }
}

impl Kept {
    async fn run(&self, call: &Call) -> io::Result<i32> {
        let argv: Result<Vec<String>, _> = call
            .argv
            .iter()
            .skip(1)
            .map(|argument| String::from_utf8(argument.clone()))
            .collect();
        let argv = argv.map_err(|_| io::Error::other("an argument is not UTF-8"))?;
        let mut stdin = Vec::new();
        if call.stdin.read_to_end(&mut stdin).await.is_err() {
            stdin.clear();
        }
        let shell = call.shell.limits();
        let fuel = call.shell.fuel();
        let run = Run {
            argv,
            stdio: crate::Stdio::Held(stdin),
            cwd: call.cwd.clone(),
            env: call.env.clone(),
            limits: Limits {
                output: shell.output,
                file: shell.file,
                sleep: self.budget.sleep,
                steps: self.budget.steps.min(fuel),
                time: self.budget.time,
                memory: self.budget.memory,
                depth: self.budget.depth,
            },
            stop: call.shell.stop(),
        };
        let files = call.shell.files();
        let ran = match (files.answers_now(), files.stat(&call.cwd, true)) {
            (true, Reply::Now(_)) => {
                let direct = Direct {
                    files,
                    shell: call.shell.clone(),
                    copied: None,
                };
                self.interpret(run, Rc::new(direct))?
            }
            (_, asked) => {
                asked.get().await?;
                let roots = roots(&files, &call.cwd, &run.argv).await;
                let tree = Rc::new(copy(&files, roots, shell.file).await?);
                let mixed = Mixed {
                    tree: tree.clone(),
                    real: files.clone(),
                };
                let direct = Direct {
                    files: Rc::new(mixed),
                    shell: call.shell.clone(),
                    copied: Some(tree.clone()),
                };
                let ran = self.interpret(run, Rc::new(direct))?;
                apply(&tree, &files).await?;
                ran
            }
        };
        call.shell.spend(ran.steps);
        self.report(call, ran).await
    }

    fn interpret(&self, run: Run, files: Rc<dyn Files>) -> io::Result<Ran> {
        self.running.set(true);
        let ran = self.interpret_where(run, files);
        self.running.set(false);
        ran
    }

    fn interpret_where(&self, run: Run, files: Rc<dyn Files>) -> io::Result<Ran> {
        #[cfg(not(target_family = "wasm"))]
        if self.native.get() || crate::native::wanted(&run, &*files) {
            self.native.set(true);
            return crate::native::run(run, &*files);
        }
        let python = match self.python.get() {
            Some(python) => python,
            None => {
                let python = Python::new()?;
                self.python.get_or_init(|| python)
            }
        };
        Ok(python.run(run, files))
    }

    async fn report(&self, call: &Call, ran: Ran) -> io::Result<i32> {
        call.stdout.write_all(&ran.stdout).await?;
        call.stderr.write_all(&ran.stderr).await?;
        let (status, said) = match ran.ended {
            Ended::Exited(status) => return Ok(status),
            Ended::Cancelled => return Ok(128 + 15),
            Ended::Needs(missing) => {
                call.shell.lacks(b"python3");
                let lines: String = missing
                    .iter()
                    .map(|missing| format!("{missing}\n"))
                    .collect();
                (127, lines)
            }
            Ended::Refused(limit) => {
                let said = match limit {
                    Limit::Output => "it printed more than this shell keeps".to_owned(),
                    Limit::Sleep => "it waited longer than this shell allows".to_owned(),
                    Limit::Steps => format!(
                        "it ran {} steps, this shell's limit for one run; split the work into shorter runs that save their progress to a file and continue from it",
                        ran.steps
                    ),
                    Limit::Time => "it ran longer than this shell allows".to_owned(),
                    Limit::Memory => {
                        let asked = if ran.held.asked == 0 {
                            String::new()
                        } else {
                            format!(" and asked for {} MiB more", mib(ran.held.asked))
                        };
                        format!(
                            "it held {} MiB{asked}, past this shell's {} MiB memory limit for one run; read and process the data in chunks (a file line by line, a batch of rows at a time) and write results out as you go instead of holding them all",
                            mib(ran.held.bytes),
                            mib(self.budget.memory.unwrap_or(u64::MAX))
                        )
                    }
                };
                (126, format!("python3: {said}\n"))
            }
        };
        call.stderr.write_all(said.as_bytes()).await?;
        Ok(status)
    }
}

fn mib(bytes: u64) -> String {
    let tenths = bytes.saturating_mul(10) / MIB;
    format!("{}.{}", tenths / 10, tenths % 10)
}

fn later() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "the host cannot answer this at once",
    )
}

fn now<T>(reply: Reply<T>) -> io::Result<T> {
    match reply {
        Reply::Now(result) => result,
        Reply::Later(_) => Err(later()),
    }
}

const fn stat(stat: nanocode_shell::Stat) -> Stat {
    Stat {
        kind: match stat.kind {
            nanocode_shell::Kind::File => Kind::File,
            nanocode_shell::Kind::Dir => Kind::Dir,
            nanocode_shell::Kind::Symlink => Kind::Symlink,
        },
        size: stat.size,
        mode: stat.mode,
        mtime_ns: stat.mtime_ns,
    }
}

const fn unstat(stat: Stat) -> nanocode_shell::Stat {
    nanocode_shell::Stat {
        kind: match stat.kind {
            Kind::File => nanocode_shell::Kind::File,
            Kind::Dir => nanocode_shell::Kind::Dir,
            Kind::Symlink => nanocode_shell::Kind::Symlink,
        },
        size: stat.size,
        mode: stat.mode,
        mtime_ns: stat.mtime_ns,
    }
}

const fn unhow(how: nanocode_shell::Write) -> Write {
    match how {
        nanocode_shell::Write::Replace => Write::Replace,
        nanocode_shell::Write::Append => Write::Append,
        nanocode_shell::Write::New => Write::New,
    }
}

const fn unwhat(what: nanocode_shell::Remove) -> Remove {
    match what {
        nanocode_shell::Remove::File => Remove::File,
        nanocode_shell::Remove::Dir => Remove::Dir,
    }
}

const fn how(how: Write) -> nanocode_shell::Write {
    match how {
        Write::Replace => nanocode_shell::Write::Replace,
        Write::Append => nanocode_shell::Write::Append,
        Write::New => nanocode_shell::Write::New,
    }
}

const fn what(what: Remove) -> nanocode_shell::Remove {
    match what {
        Remove::File => nanocode_shell::Remove::File,
        Remove::Dir => nanocode_shell::Remove::Dir,
    }
}

/// The files Python copied in, answered at once, over the file system they
/// came from for every path outside them. Python and the commands it starts
/// both work on it, so a command sees what Python wrote before starting it.
struct Mixed {
    tree: Rc<Tree>,
    real: Rc<dyn FileSystem>,
}

impl FileSystem for Mixed {
    fn stat(&self, path: &[u8], follow: bool) -> Reply<nanocode_shell::Stat> {
        if self.tree.holds(path) {
            let found = if follow {
                self.tree.stat(path)
            } else {
                self.tree.lstat(path)
            };
            Reply::Now(found.map(unstat))
        } else {
            self.real.stat(path, follow)
        }
    }

    fn read(&self, path: &[u8], limit: u64) -> Reply<Bytes> {
        if self.tree.holds(path) {
            Reply::Now(self.tree.read(path, limit).map(Bytes::from))
        } else {
            self.real.read(path, limit)
        }
    }

    fn write(&self, path: &[u8], data: &[u8], how: nanocode_shell::Write) -> Reply<()> {
        if self.tree.holds(path) {
            Reply::Now(self.tree.write(path, data, unhow(how)))
        } else {
            self.real.write(path, data, how)
        }
    }

    fn list(&self, path: &[u8]) -> Reply<Vec<Vec<u8>>> {
        if self.tree.holds(path) {
            Reply::Now(self.tree.list(path))
        } else {
            self.real.list(path)
        }
    }

    fn mkdir(&self, path: &[u8], mode: u32) -> Reply<()> {
        if self.tree.holds(path) {
            Reply::Now(self.tree.mkdir(path, mode))
        } else {
            self.real.mkdir(path, mode)
        }
    }

    fn remove(&self, path: &[u8], what: nanocode_shell::Remove) -> Reply<()> {
        if self.tree.holds(path) {
            Reply::Now(self.tree.remove(path, unwhat(what)))
        } else {
            self.real.remove(path, what)
        }
    }

    fn rename(&self, from: &[u8], to: &[u8]) -> Reply<()> {
        match (self.tree.holds(from), self.tree.holds(to)) {
            (true, true) => Reply::Now(self.tree.rename(from, to)),
            (false, false) => self.real.rename(from, to),
            _ => Reply::Now(Err(io::ErrorKind::CrossesDevices.into())),
        }
    }

    fn set_mode(&self, path: &[u8], mode: u32) -> Reply<()> {
        if self.tree.holds(path) {
            Reply::Now(self.tree.set_mode(path, mode))
        } else {
            self.real.set_mode(path, mode)
        }
    }

    fn set_mtime(&self, path: &[u8], mtime_ns: i64) -> Reply<()> {
        if self.tree.holds(path) {
            Reply::Now(self.tree.set_mtime(path, mtime_ns))
        } else {
            self.real.set_mtime(path, mtime_ns)
        }
    }

    fn symlink(&self, target: &[u8], link: &[u8]) -> Reply<()> {
        if self.tree.holds(link) {
            Reply::Now(self.tree.symlink(target, link))
        } else {
            self.real.symlink(target, link)
        }
    }

    fn read_link(&self, path: &[u8]) -> Reply<Vec<u8>> {
        if self.tree.holds(path) {
            Reply::Now(self.tree.read_link(path))
        } else {
            self.real.read_link(path)
        }
    }

    fn hard_link(&self, existing: &[u8], link: &[u8]) -> Reply<()> {
        match (self.tree.holds(existing), self.tree.holds(link)) {
            (true, true) => Reply::Now(self.tree.hard_link(existing, link)),
            (false, false) => self.real.hard_link(existing, link),
            _ => Reply::Now(Err(io::ErrorKind::CrossesDevices.into())),
        }
    }

    fn links(&self, path: &[u8], follow: bool) -> Option<nanocode_shell::Links> {
        if self.tree.holds(path) {
            self.tree
                .links(path, follow)
                .map(|(group, names)| nanocode_shell::Links { group, names })
        } else {
            self.real.links(path, follow)
        }
    }

    fn answers_now(&self) -> bool {
        false
    }
}

struct Direct {
    files: Rc<dyn FileSystem>,
    shell: Session,
    copied: Option<Rc<Tree>>,
}

impl Files for Direct {
    fn stat(&self, path: &[u8]) -> io::Result<Stat> {
        now(self.files.stat(path, true)).map(stat)
    }

    fn read(&self, path: &[u8], limit: u64) -> io::Result<Vec<u8>> {
        now(self.files.read(path, limit)).map(|data| data.to_vec())
    }

    fn write(&self, path: &[u8], data: &[u8], write: Write) -> io::Result<()> {
        now(self.files.write(path, data, how(write)))
    }

    fn list(&self, path: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        now(self.files.list(path))
    }

    fn mkdir(&self, path: &[u8], mode: u32) -> io::Result<()> {
        now(self.files.mkdir(path, mode))
    }

    fn remove(&self, path: &[u8], remove: Remove) -> io::Result<()> {
        now(self.files.remove(path, what(remove)))
    }

    fn rename(&self, from: &[u8], to: &[u8]) -> io::Result<()> {
        now(self.files.rename(from, to))
    }

    fn set_mode(&self, path: &[u8], mode: u32) -> io::Result<()> {
        now(self.files.set_mode(path, mode))
    }

    fn set_mtime(&self, path: &[u8], mtime_ns: i64) -> io::Result<()> {
        now(self.files.set_mtime(path, mtime_ns))
    }

    fn lstat(&self, path: &[u8]) -> io::Result<Stat> {
        now(self.files.stat(path, false)).map(stat)
    }

    fn symlink(&self, target: &[u8], link: &[u8]) -> io::Result<()> {
        now(self.files.symlink(target, link))
    }

    fn read_link(&self, path: &[u8]) -> io::Result<Vec<u8>> {
        now(self.files.read_link(path))
    }

    fn hard_link(&self, existing: &[u8], link: &[u8]) -> io::Result<()> {
        now(self.files.hard_link(existing, link))
    }

    fn links(&self, path: &[u8], follow: bool) -> Option<(u64, u64)> {
        self.files
            .links(path, follow)
            .map(|links| (links.group, links.names))
    }

    fn spawns(&self) -> bool {
        true
    }

    fn spawn(&self, child: Child) -> io::Result<Exited> {
        let child = nanocode_shell::Child {
            argv: child.argv,
            env: child.env,
            cwd: child.cwd,
            stdin: child.stdin.into(),
            merged: child.merged,
        };
        let exited = match &self.copied {
            Some(tree) => self.shell.run_over(child, |real| {
                Rc::new(Mixed {
                    tree: tree.clone(),
                    real,
                })
            }),
            None => self.shell.run(child),
        }?;
        Ok(Exited {
            status: exited.status,
            stdout: exited.stdout,
            stderr: exited.stderr,
        })
    }
}

/// The directories copied in for a run over a file system that cannot answer
/// at once: the working directory, the temporary directory programs write
/// scratch files and scripts to, and the directory of the script file named
/// on the command line, without one nested in another.
async fn roots(files: &Rc<dyn FileSystem>, cwd: &[u8], argv: &[String]) -> Vec<Vec<u8>> {
    let mut wanted = vec![cwd.to_vec()];
    let mut extra = vec![b"/tmp".to_vec()];
    if let Some(script) = argv.first().filter(|arg| !arg.starts_with('-')) {
        let script = script.as_bytes();
        if script.starts_with(b"/") {
            if let Some(slash) = script.iter().rposition(|&byte| byte == b'/') {
                if slash > 0 {
                    extra.push(script[..slash].to_vec());
                }
            }
        }
    }
    for dir in extra {
        let is_dir = matches!(
            files.stat(&dir, true).get().await,
            Ok(found) if matches!(found.kind, nanocode_shell::Kind::Dir)
        );
        if is_dir {
            wanted.push(dir);
        }
    }
    let inside = |path: &[u8], dir: &[u8]| {
        dir == b"/"
            || path == dir
            || (path.starts_with(dir) && path.get(dir.len()) == Some(&b'/'))
    };
    let mut kept: Vec<Vec<u8>> = Vec::new();
    for (index, dir) in wanted.iter().enumerate() {
        let covered = wanted.iter().enumerate().any(|(other, root)| {
            other != index && inside(dir, root) && (dir != root || other < index)
        });
        if !covered {
            kept.push(dir.clone());
        }
    }
    kept
}

/// Copies the roots in; the first must copy whole, while a file or directory
/// under a later one that cannot be read is left out rather than failing the run.
async fn copy(files: &Rc<dyn FileSystem>, roots: Vec<Vec<u8>>, limit: u64) -> io::Result<Tree> {
    let tree = Tree::new(roots.clone());
    let first = roots.first().cloned().unwrap_or_default();
    let mut dirs = roots;
    while let Some(dir) = dirs.pop() {
        let strict = dir.starts_with(&first);
        match copy_dir(files, &tree, &dir, limit, &mut dirs).await {
            Err(_) if !strict => {}
            done => done?,
        }
    }
    Ok(tree)
}

async fn copy_dir(
    files: &Rc<dyn FileSystem>,
    tree: &Tree,
    dir: &[u8],
    limit: u64,
    dirs: &mut Vec<Vec<u8>>,
) -> io::Result<()> {
    let mode = files.stat(dir, true).get().await?.mode;
    for name in files.list(dir).get().await? {
        let path = join(dir, &name);
        let found = files.stat(&path, true).get().await?;
        match found.kind {
            nanocode_shell::Kind::Dir => dirs.push(path),
            nanocode_shell::Kind::File => {
                if let Some(links) = files.links(&path, true) {
                    tree.join(path.clone(), links.group);
                }
                let entry = Entry::File {
                    data: files.read(&path, limit).get().await?.to_vec(),
                    mode: found.mode,
                    mtime_ns: found.mtime_ns,
                };
                tree.put(path, entry);
            }
            nanocode_shell::Kind::Symlink => {}
        }
    }
    tree.put(dir.to_vec(), Entry::Dir { mode });
    Ok(())
}

async fn apply(tree: &Tree, files: &Rc<dyn FileSystem>) -> io::Result<()> {
    let delta = tree.delta();
    for path in &delta.removed {
        let what = match files.stat(path, false).get().await?.kind {
            nanocode_shell::Kind::Dir => nanocode_shell::Remove::Dir,
            _ => nanocode_shell::Remove::File,
        };
        files.remove(path, what).get().await?;
    }
    for (path, mode) in &delta.made {
        files.mkdir(path, *mode).get().await?;
    }
    for path in &delta.written {
        let data = tree.take(path).unwrap_or_default();
        let write = nanocode_shell::Write::Replace;
        files.write(path, &data, write).get().await?;
    }
    for (existing, link) in &delta.hard_linked {
        files.hard_link(existing, link).get().await?;
    }
    for (path, mode) in &delta.modes {
        files.set_mode(path, *mode).get().await?;
    }
    for (link, target) in &delta.linked {
        files.symlink(target, link).get().await?;
    }
    for (path, mtime_ns) in &delta.times {
        match files.set_mtime(path, *mtime_ns).get().await {
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {}
            done => done?,
        }
    }
    Ok(())
}
