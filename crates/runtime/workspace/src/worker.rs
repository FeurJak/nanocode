use std::{
    future::{Future, poll_fn},
    io,
    pin::Pin,
    rc::Rc,
    task::Poll,
};

use nanocode_shell::{
    Commands, Config as ShellConfig, Fetch, FileSystem, Identity, Kind, Limits, Machine, Remove,
    Script, Shell, Step, Write,
};
use nanocode_sync::{Stream, oneshot, stream::FuturesUnordered, unbounded};

use crate::{Command, Config, Finished};

type Reply<T> = oneshot::Sender<Result<T, String>>;

pub enum Job {
    Read(Vec<u8>, Reply<Option<Vec<u8>>>),
    Write(Vec<u8>, Vec<u8>, Reply<()>),
    Remove(Vec<u8>, Reply<()>),
    Run(Command, Reply<Finished>),
}

#[cfg(not(target_family = "wasm"))]
pub fn start(config: Config, jobs: unbounded::Receiver<Job>) -> io::Result<()> {
    let handle = nanocode_platform::Handle::try_current()
        .map_err(|_| io::Error::other("a workspace starts inside a runtime"))?;
    std::thread::Builder::new()
        .name("workspace".to_owned())
        .spawn(move || {
            let _entered = handle.enter();
            nanocode_sync::block_on(serve(config, jobs));
        })
        .map(drop)
}

#[cfg(target_family = "wasm")]
pub fn start(config: Config, jobs: unbounded::Receiver<Job>) -> io::Result<()> {
    nanocode_platform::spawn_local(serve(config, jobs));
    Ok(())
}

struct Kept {
    files: Rc<dyn FileSystem>,
    machine: Rc<dyn Machine>,
    commands: Rc<dyn Commands>,
    fetch: Option<Rc<dyn Fetch>>,
    identity: Identity,
    limits: Limits,
    env: Vec<(Vec<u8>, Vec<u8>)>,
    slice: u32,
}

async fn serve(config: Config, mut jobs: unbounded::Receiver<Job>) {
    let kept = Rc::new(Kept {
        files: (config.files)(),
        machine: (config.machine)(),
        commands: (config.commands)(),
        fetch: config.fetch.as_ref().map(|fetch| fetch()),
        identity: config.identity,
        limits: config.limits,
        env: config.env,
        slice: config.slice,
    });
    let mut running: FuturesUnordered<Pin<Box<dyn Future<Output = ()>>>> = FuturesUnordered::new();
    let mut open = true;
    poll_fn(|context| {
        while open {
            match Pin::new(&mut jobs).poll_next(context) {
                Poll::Ready(Some(job)) => running.push(Box::pin(kept.clone().serve(job))),
                Poll::Ready(None) => open = false,
                Poll::Pending => break,
            }
        }
        while let Poll::Ready(Some(())) = Pin::new(&mut running).poll_next(context) {}
        match open || !running.is_empty() {
            true => Poll::Pending,
            false => Poll::Ready(()),
        }
    })
    .await;
}

impl Kept {
    async fn serve(self: Rc<Self>, job: Job) {
        match job {
            Job::Read(path, reply) => {
                let _ = reply.send(self.read(&path).await);
            }
            Job::Write(path, data, reply) => {
                let _ = reply.send(self.write(&path, &data).await);
            }
            Job::Remove(path, reply) => {
                let _ = reply.send(self.remove(&path).await);
            }
            Job::Run(command, mut reply) => {
                let finished = self.run(command, &mut reply).await;
                let _ = reply.send(finished);
            }
        }
    }

    async fn read(&self, path: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let found = match self.files.stat(path, true).get().await {
            Ok(found) => found,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(said(path, &error)),
        };
        if found.kind == Kind::Dir {
            return Err(format!("{}: Is a directory", shown(path)));
        }
        let data = self.files.read(path, self.limits.file).get().await;
        data.map(|data| Some(data.to_vec()))
            .map_err(|error| said(path, &error))
    }

    async fn write(&self, path: &[u8], data: &[u8]) -> Result<(), String> {
        let mut parent = Vec::new();
        let names: Vec<&[u8]> = path.split(|byte| *byte == b'/').collect();
        for name in names.iter().take(names.len().saturating_sub(1)) {
            if name.is_empty() {
                continue;
            }
            parent.push(b'/');
            parent.extend_from_slice(name);
            match self.files.stat(&parent, true).get().await {
                Ok(found) if found.kind == Kind::Dir => {}
                Ok(_) => return Err(format!("{}: Not a directory", shown(&parent))),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let made = self.files.mkdir(&parent, 0o755).get().await;
                    made.map_err(|error| said(&parent, &error))?;
                }
                Err(error) => return Err(said(&parent, &error)),
            }
        }
        let written = self.files.write(path, data, Write::Replace).get().await;
        written.map_err(|error| said(path, &error))
    }

    async fn remove(&self, path: &[u8]) -> Result<(), String> {
        let found = self.files.stat(path, false).get().await;
        let what = match found.map_err(|error| said(path, &error))?.kind {
            Kind::Dir => Remove::Dir,
            Kind::File | Kind::Symlink => Remove::File,
        };
        let removed = self.files.remove(path, what).get().await;
        removed.map_err(|error| said(path, &error))
    }

    async fn run(&self, command: Command, reply: &mut Reply<Finished>) -> Result<Finished, String> {
        let mut env = self.env.clone();
        env.extend(command.env);
        let mut shell = Shell::new(ShellConfig {
            fs: self.files.clone(),
            commands: self.commands.clone(),
            machine: self.machine.clone(),
            identity: self.identity.clone(),
            limits: self.limits,
            env,
            cwd: command.cwd,
            without: Vec::new(),
            strict: false,
        });
        if let Some(fetch) = &self.fetch {
            shell.set_fetch(fetch.clone());
        }
        shell.start(Script {
            source: command.script.into_bytes(),
            name: b"bash".to_vec(),
            args: Vec::new(),
            options: Vec::new(),
            stdin: None,
        });
        let mut timer = command.timeout.map(|timeout| self.machine.sleep(timeout));
        let mut timed_out = false;
        let mut stopped = false;
        poll_fn(|context| {
            if !stopped {
                timed_out = timer
                    .as_mut()
                    .is_some_and(|timer| timer.as_mut().poll(context).is_ready());
                if timed_out || reply.poll_closed(context).is_ready() {
                    shell.cancel();
                    stopped = true;
                }
            }
            shell.set_waker(context.waker().clone());
            match shell.step(self.slice) {
                Step::Done(ran) => Poll::Ready(Ok(Finished {
                    ran: *ran,
                    timed_out,
                })),
                Step::Blocked => Poll::Pending,
                Step::Yield => {
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
                Step::Idle => Poll::Ready(Err("the shell had no script to run".to_owned())),
            }
        })
        .await
    }
}

fn shown(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn said(path: &[u8], error: &io::Error) -> String {
    format!("{}: {error}", shown(path))
}
