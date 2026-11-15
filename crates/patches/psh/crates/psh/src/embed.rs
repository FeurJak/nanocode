//! psh inside an embedder's own process (Unix): `contained` says whether a script is
//! one the shell runs in this process, and `start` runs a script on an engine thread
//! with the embedder's sinks as its output.
//!
//! For a host that runs each command of an agent and wants no shell process between it
//! and the programs the command starts: builtins and psh's utilities run on the engine
//! thread, and every other program is started directly (`posix_spawn`). Such a run
//! shares the embedder's process: its memory, its descriptor table and its fate. The
//! shell's `$$` is not the process's, so `kill` cannot reach the embedder. Every program
//! leads a session of its own: an interrupt reaches each one still running, and stopping
//! the run kills them. What programs leave running when the run ends stays, as with
//! a shell process.

use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::task::{Context, Poll};

use psh_classify::Category;
use psh_engine::host::{FileKind, SIGINT};
use psh_engine::std_host::{drive, engine_thread};
use psh_engine::{ControlHandle, HostFile, Script, Shell, ShellConfig};
use psh_os::unix::{Leftovers, RunKiller, UnixConfig, UnixFile, UnixHost};

/// Where a stream of the script goes. Called on the engine thread with each write; an
/// error is the script's write error (`BrokenPipe` ends the writer as SIGPIPE would).
/// It may block: the script waits, as on a full pipe.
pub type Sink = Box<dyn FnMut(&[u8]) -> io::Result<()> + Send>;

/// One script to run.
pub struct Command {
    pub script: Vec<u8>,
    /// The working directory, absolute.
    pub cwd: Vec<u8>,
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    pub stdout: Sink,
    pub stderr: Sink,
    /// Run as a login shell (`bash -l`): `shopt login_shell` is on.
    pub login: bool,
}

/// A `$$` no process has (above every pid a system gives out): with it psh-os refuses
/// to signal the embedder or its group (`UnixConfig::pid`). No contained script reads it.
const NO_PID: u32 = 99_999_999;

/// Builtins a contained script may not use: they act on the process the shell is.
const PROCESS_BUILTINS: [&str; 3] = ["exec", "kill", "suspend"];

/// Shells psh runs itself (D1410): a script given to one of them runs in this process.
const SHELLS: [&str; 2] = ["bash", "sh"];

/// Whether `script` runs in this process: every command it runs in the shell (also in
/// `$(…)`, `bash -c` strings, `xargs` and `find -exec`) has a literal name and is not one
/// of the builtins that act on the shell's process; nothing is started in the background
/// and nothing long-running is started; nothing reads the shell's process ids. A builtin or
/// shell that runs code the classifier cannot see (`source`, `eval` of a non-literal,
/// `bash -c "$script"`) is not contained; any other program is.
pub fn contained(script: &str, _env: &[(Vec<u8>, Vec<u8>)]) -> bool {
    if script.contains("PPID") || psh_engine::uses_pid(script.as_bytes()) {
        return false;
    }
    let classified = psh_classify::classify(script);
    if classified.error.is_some() || classified.categories.contains(Category::LongRunning) {
        return false;
    }
    classified.commands.iter().all(|command| {
        // A command without words only assigns.
        let Some(word) = command.words.first() else {
            return true;
        };
        let Some(name) = word.value.as_deref() else {
            return false;
        };
        if PROCESS_BUILTINS.contains(&name) {
            return false;
        }
        !command.categories.contains(Category::Unknown)
            || !(psh_engine::is_builtin(name.as_bytes())
                || command
                    .name
                    .as_deref()
                    .is_some_and(|shell| SHELLS.contains(&shell)))
    })
}

/// A script that is running: a handle any thread may use.
#[derive(Clone)]
pub struct Running(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    /// The shell's control and the killer of what it started, once the engine thread
    /// made them.
    shell: Option<(ControlHandle, RunKiller)>,
    /// Asked for before the shell existed.
    stop: bool,
    interrupt: bool,
}

impl Running {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ctrl-C: the script's INT trap runs, or it ends with status 130.
    pub fn interrupt(&self) {
        let mut state = self.state();
        match &state.shell {
            Some((control, killer)) => {
                control.signal(SIGINT);
                killer.interrupt();
            }
            None => state.interrupt = true,
        }
    }

    /// Ends the run now (status 143): no trap runs, and every program it started is
    /// killed.
    pub fn stop(&self) {
        let mut state = self.state();
        match &state.shell {
            Some((control, killer)) => {
                control.cancel();
                killer.kill_all();
            }
            None => state.stop = true,
        }
    }
}

/// A sink as one of the shell's descriptors: a pipe that is only written.
struct SinkFile(RefCell<Sink>);

impl HostFile for SinkFile {
    fn poll_read(&self, _cx: &mut Context<'_>, _buf: &mut [u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::other("Bad file descriptor")))
    }

    fn poll_write(&self, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready((self.0.borrow_mut())(buf).map(|()| buf.len()))
    }

    fn kind(&self) -> Option<FileKind> {
        Some(FileKind::Fifo)
    }
}

/// Runs `command` as `psh -c SCRIPT` would, with the null device as stdin, on the
/// engine thread the runs share, or on a new one while that thread is busy. `ended` is
/// called on that thread with the status once the shell and what it held are gone, or
/// with the panic that ended the run.
pub fn start(
    command: Command,
    ended: impl FnOnce(std::thread::Result<i32>) + Send + 'static,
) -> io::Result<Running> {
    let stdin = std::fs::File::open("/dev/null")?;
    let running = Running(Arc::default());
    let shared = running.clone();
    let run: Run = Box::new(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let host = UnixHost::new(UnixConfig {
                pid: Some(NO_PID),
                leftovers: Leftovers::Keep,
                inherit_ignored: true,
                ..UnixConfig::default()
            });
            let killer = host.killer();
            let mut cfg = ShellConfig::new(Rc::new(host));
            let arg0 = command
                .env
                .iter()
                .find(|(name, _)| name == crate::NAME_VAR.as_bytes())
                .map_or_else(|| b"psh".to_vec(), |(_, value)| value.clone());
            cfg.env = command.env;
            (cfg.utilities, cfg.nested_shells) = crate::settings(&mut cfg.env);
            cfg.cwd = command.cwd;
            cfg.stdin = Some(Rc::new(UnixFile::new(stdin)));
            cfg.stdout = Some(Rc::new(SinkFile(RefCell::new(command.stdout))));
            cfg.stderr = Some(Rc::new(SinkFile(RefCell::new(command.stderr))));
            let mut shell = Shell::new(cfg);
            let control = shell.control();
            shell.start(Script {
                source: command.script,
                arg0,
                args: Vec::new(),
                dash_c: true,
                options: match command.login {
                    true => vec![b"-O".to_vec(), b"login_shell".to_vec()],
                    false => Vec::new(),
                },
                stdin: false,
            });
            {
                let mut state = shared.state();
                if state.stop {
                    control.cancel();
                } else if state.interrupt {
                    control.signal(SIGINT);
                }
                state.shell = Some((control, killer));
            }
            let status = drive(&mut shell);
            shared.state().shell = None;
            status
        }))
    });
    let ended: Ended = Box::new(ended);
    if ENGINE_BUSY.swap(true, Ordering::Acquire) {
        engine_thread(move || ended(run()))?;
        return Ok(running);
    }
    let sent = match ENGINE.get() {
        Some(engine) => engine.send((run, ended)).map_err(|error| error.0),
        None => {
            let (sender, jobs) = mpsc::channel::<(Run, Ended)>();
            if let Err(error) = engine_thread(move || serve(&jobs)) {
                ENGINE_BUSY.store(false, Ordering::Release);
                return Err(error);
            }
            ENGINE
                .get_or_init(|| sender)
                .send((run, ended))
                .map_err(|error| error.0)
        }
    };
    if let Err((run, ended)) = sent {
        ENGINE_BUSY.store(false, Ordering::Release);
        engine_thread(move || ended(run()))?;
    }
    Ok(running)
}

type Run = Box<dyn FnOnce() -> std::thread::Result<i32> + Send>;
type Ended = Box<dyn FnOnce(std::thread::Result<i32>) + Send>;

static ENGINE: OnceLock<mpsc::Sender<(Run, Ended)>> = OnceLock::new();
static ENGINE_BUSY: AtomicBool = AtomicBool::new(false);

fn serve(jobs: &mpsc::Receiver<(Run, Ended)>) {
    while let Ok((run, ended)) = jobs.recv() {
        let status = run();
        ENGINE_BUSY.store(false, Ordering::Release);
        ended(status);
    }
}
