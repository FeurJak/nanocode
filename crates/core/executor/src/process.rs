use std::{
    ffi::OsStr,
    future::Future,
    io,
    path::Path,
    pin::Pin,
    process::{ExitStatus, Output, Stdio},
    sync::OnceLock,
    task::{Context, Poll},
};

use nanocode_io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

use crate::queue::Shared;

static ORPHANS: OnceLock<Shared<std::process::Child>> = OnceLock::new();

fn orphans() -> &'static Shared<std::process::Child> {
    ORPHANS.get_or_init(Shared::new)
}

fn reap_orphans() {
    let orphans = orphans();
    if orphans.len() == 0 {
        return;
    }
    for mut orphan in orphans.drain() {
        if !matches!(orphan.try_wait(), Ok(Some(_)) | Err(_)) {
            orphans.push(orphan);
        }
    }
}

#[cfg(any(target_os = "android", target_os = "linux"))]
pub fn survivors(group: Option<u32>, mark: (&str, &str)) -> io::Result<Vec<(u32, String)>> {
    let needle = [mark.0.as_bytes(), b"=", mark.1.as_bytes()].concat();
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let Some(id) = entry?
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{id}/stat")) else {
            continue;
        };
        let Some((name, fields)) = stat
            .split_once(" (")
            .and_then(|(_, rest)| rest.rsplit_once(") "))
        else {
            continue;
        };
        let fields: Vec<&str> = fields.split_whitespace().take(4).collect();
        let [state, _, of, session] = fields[..] else {
            continue;
        };
        if state == "Z" {
            continue;
        }
        let grouped = group.is_some_and(|group| of.parse::<u32>().ok() == Some(group));
        let marked = || {
            session.parse::<u32>().ok() == Some(id)
                && std::fs::read(format!("/proc/{id}/environ")).is_ok_and(|environment| {
                    environment
                        .split(|&byte| byte == 0)
                        .any(|variable| variable == needle)
                })
        };
        if !grouped && !marked() {
            continue;
        }
        let line = std::fs::read(format!("/proc/{id}/cmdline")).unwrap_or_default();
        let command = String::from_utf8_lossy(&line)
            .split('\0')
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        found.push((
            id,
            if command.is_empty() {
                name.to_owned()
            } else {
                command
            },
        ));
    }
    found.sort_unstable_by_key(|(id, _)| *id);
    Ok(found)
}

#[cfg(target_os = "macos")]
pub fn survivors(group: Option<u32>, mark: (&str, &str)) -> io::Result<Vec<(u32, String)>> {
    use nanocode_sys::unix as host;

    fn arguments(buffer: &[u8]) -> Option<(Vec<&[u8]>, &[u8])> {
        let (count, rest) = buffer.split_first_chunk::<4>()?;
        let count = usize::try_from(i32::from_ne_bytes(*count)).ok()?;
        let path = rest.iter().position(|&byte| byte == 0)?;
        let mut rest = &rest[path..];
        rest = &rest[rest.iter().position(|&byte| byte != 0)?..];
        let mut words = Vec::with_capacity(count);
        for _ in 0..count {
            let end = rest.iter().position(|&byte| byte == 0)?;
            words.push(&rest[..end]);
            rest = &rest[end + 1..];
        }
        Some((words, rest))
    }

    let needle = [mark.0.as_bytes(), b"=", mark.1.as_bytes()].concat();
    let mut buffer = vec![0_u8; host::argument_bytes_limit()?];
    let mut found = Vec::new();
    for id in host::process_ids()? {
        let Ok(number) = u32::try_from(id) else {
            continue;
        };
        let grouped = group.is_some_and(|group| {
            host::process_group_of(id)
                .ok()
                .and_then(|of| u32::try_from(of).ok())
                == Some(group)
        });
        if !grouped && host::session_of(id).ok() != Some(id) {
            continue;
        }
        let Ok(size) = host::process_arguments(id, &mut buffer) else {
            continue;
        };
        let Some((words, environment)) = arguments(&buffer[..size]) else {
            continue;
        };
        if !grouped
            && !environment
                .split(|&byte| byte == 0)
                .any(|variable| variable == needle)
        {
            continue;
        }
        let command = words
            .iter()
            .map(|word| String::from_utf8_lossy(word))
            .collect::<Vec<_>>()
            .join(" ");
        found.push((number, command));
    }
    found.sort_unstable_by_key(|(id, _)| *id);
    Ok(found)
}

#[cfg(all(
    unix,
    not(any(target_os = "android", target_os = "linux", target_os = "macos"))
))]
pub fn survivors(_group: Option<u32>, _mark: (&str, &str)) -> io::Result<Vec<(u32, String)>> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(not(unix))]
pub fn survivors(_group: Option<u32>, _mark: (&str, &str)) -> io::Result<Vec<(u32, String)>> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

pub struct Command {
    std: std::process::Command,
    kill_on_drop: bool,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.std.fmt(formatter)
    }
}

impl From<std::process::Command> for Command {
    fn from(std: std::process::Command) -> Self {
        Self {
            std,
            kill_on_drop: false,
        }
    }
}

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self::from(std::process::Command::new(program))
    }

    #[must_use]
    pub const fn as_std(&self) -> &std::process::Command {
        &self.std
    }

    pub const fn as_std_mut(&mut self) -> &mut std::process::Command {
        &mut self.std
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.std.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.std.args(args);
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.std.env(key, value);
        self
    }

    pub fn envs<I, K, V>(&mut self, variables: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.std.envs(variables);
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.std.env_remove(key);
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.std.env_clear();
        self
    }

    pub fn current_dir(&mut self, directory: impl AsRef<Path>) -> &mut Self {
        self.std.current_dir(directory);
        self
    }

    pub fn stdin(&mut self, configuration: impl Into<Stdio>) -> &mut Self {
        self.std.stdin(configuration);
        self
    }

    pub fn stdout(&mut self, configuration: impl Into<Stdio>) -> &mut Self {
        self.std.stdout(configuration);
        self
    }

    pub fn stderr(&mut self, configuration: impl Into<Stdio>) -> &mut Self {
        self.std.stderr(configuration);
        self
    }

    pub const fn kill_on_drop(&mut self, kill_on_drop: bool) -> &mut Self {
        self.kill_on_drop = kill_on_drop;
        self
    }

    #[must_use]
    pub const fn get_kill_on_drop(&self) -> bool {
        self.kill_on_drop
    }

    #[cfg(unix)]
    pub fn process_group(&mut self, group: i32) -> &mut Self {
        std::os::unix::process::CommandExt::process_group(&mut self.std, group);
        self
    }

    #[cfg(unix)]
    pub fn uid(&mut self, id: u32) -> &mut Self {
        std::os::unix::process::CommandExt::uid(&mut self.std, id);
        self
    }

    #[cfg(unix)]
    pub fn gid(&mut self, id: u32) -> &mut Self {
        std::os::unix::process::CommandExt::gid(&mut self.std, id);
        self
    }

    pub fn spawn(&mut self) -> io::Result<Child> {
        reap_orphans();
        let mut child = self.std.spawn()?;
        let stdin = child.stdin.take().map(ChildStdin::from_std).transpose();
        let stdout = child.stdout.take().map(ChildStdout::from_std).transpose();
        let stderr = child.stderr.take().map(ChildStderr::from_std).transpose();
        let exit = Exit::new(&child);
        let (stdin, stdout, stderr, exit) = match (stdin, stdout, stderr, exit) {
            (Ok(stdin), Ok(stdout), Ok(stderr), Ok(exit)) => (stdin, stdout, stderr, exit),
            (Err(error), ..) | (_, Err(error), ..) | (_, _, Err(error), _) | (.., Err(error)) => {
                let _ = child.kill();
                let _ = child.try_wait();
                orphans().push(child);
                return Err(error);
            }
        };
        Ok(Child {
            child: Some(child),
            status: None,
            exit,
            kill_on_drop: self.kill_on_drop,
            stdin,
            stdout,
            stderr,
        })
    }

    pub fn output(&mut self) -> impl Future<Output = io::Result<Output>> + use<> {
        self.std.stdout(Stdio::piped());
        self.std.stderr(Stdio::piped());
        let child = self.spawn();
        async { child?.wait_with_output().await }
    }

    pub fn status(&mut self) -> impl Future<Output = io::Result<ExitStatus>> + use<> {
        let child = self.spawn();
        async {
            let mut child = child?;
            child.stdin.take();
            child.stdout.take();
            child.stderr.take();
            child.wait().await
        }
    }
}

#[cfg(unix)]
enum Exit {
    #[cfg(target_os = "linux")]
    Descriptor(crate::net::Source<std::os::fd::OwnedFd>),
    Signal(crate::signal::unix::Signal),
}

#[cfg(unix)]
impl Exit {
    fn new(child: &std::process::Child) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        if let Ok(descriptor) = nanocode_sys::unix::signal::pidfd_open(child.id()) {
            return Ok(Self::Descriptor(crate::net::Source::new(descriptor)?));
        }
        let _ = child;
        crate::signal::unix::signal(crate::signal::unix::SignalKind::child()).map(Self::Signal)
    }

    async fn changed(&mut self) {
        match self {
            #[cfg(target_os = "linux")]
            Self::Descriptor(descriptor) => {
                descriptor.ready(crate::reactor::Direction::Read).await;
            }
            Self::Signal(signal) => {
                signal.recv().await;
            }
        }
    }
}

#[cfg(windows)]
const EXIT_POLL: std::time::Duration = std::time::Duration::from_millis(1);

#[cfg(windows)]
struct Exit;

#[cfg(windows)]
impl Exit {
    #[allow(clippy::unnecessary_wraps)]
    const fn new(_child: &std::process::Child) -> io::Result<Self> {
        Ok(Self)
    }

    async fn changed(&mut self) {
        crate::time::sleep(EXIT_POLL).await;
    }
}

pub struct Child {
    child: Option<std::process::Child>,
    status: Option<ExitStatus>,
    exit: Exit,
    kill_on_drop: bool,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl std::fmt::Debug for Child {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Child")
            .field("id", &self.id())
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Child {
    #[must_use]
    pub fn id(&self) -> Option<u32> {
        match (&self.child, self.status) {
            (Some(child), None) => Some(child.id()),
            _ => None,
        }
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let status = child.try_wait()?;
        if status.is_some() {
            self.status = status;
        }
        Ok(status)
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        drop(self.stdin.take());
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            self.exit.changed().await;
        }
    }

    pub fn start_kill(&mut self) -> io::Result<()> {
        match (self.status, self.child.as_mut()) {
            (None, Some(child)) => child.kill(),
            _ => Ok(()),
        }
    }

    pub async fn kill(&mut self) -> io::Result<()> {
        self.start_kill()?;
        self.wait().await?;
        Ok(())
    }

    pub async fn wait_with_output(mut self) -> io::Result<Output> {
        async fn read_to_end<A: AsyncRead + Unpin>(pipe: &mut Option<A>) -> io::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            if let Some(pipe) = pipe.as_mut() {
                pipe.read_to_end(&mut bytes).await?;
            }
            Ok(bytes)
        }
        let mut stdout_pipe = self.stdout.take();
        let mut stderr_pipe = self.stderr.take();
        let (status, out, err) = {
            let mut waiting = std::pin::pin!(self.wait());
            let mut stdout = std::pin::pin!(read_to_end(&mut stdout_pipe));
            let mut stderr = std::pin::pin!(read_to_end(&mut stderr_pipe));
            let mut status = None;
            let mut out = None;
            let mut err = None;
            std::future::poll_fn(|context| {
                if status.is_none()
                    && let Poll::Ready(result) = waiting.as_mut().poll(context)
                {
                    status = Some(result?);
                }
                if out.is_none()
                    && let Poll::Ready(result) = stdout.as_mut().poll(context)
                {
                    out = Some(result?);
                }
                if err.is_none()
                    && let Poll::Ready(result) = stderr.as_mut().poll(context)
                {
                    err = Some(result?);
                }
                if status.is_some() && out.is_some() && err.is_some() {
                    Poll::Ready(Ok::<(), io::Error>(()))
                } else {
                    Poll::Pending
                }
            })
            .await?;
            (status, out, err)
        };
        drop(stdout_pipe);
        drop(stderr_pipe);
        Ok(Output {
            status: status.unwrap_or_else(|| unreachable!("the wait finished")),
            stdout: out.unwrap_or_default(),
            stderr: err.unwrap_or_default(),
        })
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.status.is_some() {
            return;
        }
        let Some(mut child) = self.child.take() else {
            return;
        };
        if self.kill_on_drop {
            let _ = child.kill();
        }
        if !matches!(child.try_wait(), Ok(Some(_))) {
            orphans().push(child);
        }
    }
}

#[cfg(unix)]
macro_rules! pipe_type {
    ($name:ident, $std:ty) => {
        pub struct $name {
            pipe: crate::net::unix::Pipe,
        }

        impl $name {
            pub fn from_std(pipe: $std) -> io::Result<Self> {
                Ok(Self {
                    pipe: crate::net::unix::Pipe::from_owned_fd(std::os::fd::OwnedFd::from(pipe))?,
                })
            }

            pub fn into_owned_fd(self) -> io::Result<std::os::fd::OwnedFd> {
                self.pipe.into_owned_fd()
            }
        }

        impl std::os::fd::AsFd for $name {
            fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
                std::os::fd::AsFd::as_fd(&self.pipe)
            }
        }

        impl std::os::fd::AsRawFd for $name {
            fn as_raw_fd(&self) -> std::os::fd::RawFd {
                std::os::fd::AsRawFd::as_raw_fd(&std::os::fd::AsFd::as_fd(&self.pipe))
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.pipe.fmt(formatter)
            }
        }
    };
}

#[cfg(windows)]
macro_rules! pipe_type {
    ($name:ident, $std:ty) => {
        pub struct $name {
            pipe: crate::fs::Blocking<$std>,
        }

        impl $name {
            #[allow(clippy::unnecessary_wraps)]
            pub const fn from_std(pipe: $std) -> io::Result<Self> {
                Ok(Self {
                    pipe: crate::fs::Blocking::new(pipe),
                })
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(stringify!($name))
            }
        }
    };
}

pipe_type!(ChildStdin, std::process::ChildStdin);
pipe_type!(ChildStdout, std::process::ChildStdout);
pipe_type!(ChildStderr, std::process::ChildStderr);

#[cfg(unix)]
macro_rules! read_pipe {
    ($name:ident) => {
        impl AsyncRead for $name {
            fn poll_read(
                mut self: Pin<&mut Self>,
                context: &mut Context<'_>,
                buffer: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                Pin::new(&mut self.pipe).poll_read(context, buffer)
            }
        }
    };
}

#[cfg(windows)]
macro_rules! read_pipe {
    ($name:ident) => {
        impl AsyncRead for $name {
            fn poll_read(
                mut self: Pin<&mut Self>,
                context: &mut Context<'_>,
                buffer: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                Pin::new(&mut self.pipe).poll_read(context, buffer)
            }
        }
    };
}

read_pipe!(ChildStdout);
read_pipe!(ChildStderr);

impl AsyncWrite for ChildStdin {
    #[cfg(unix)]
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.pipe).poll_write(context, buffer)
    }

    #[cfg(windows)]
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.pipe).poll_write(context, buffer)
    }

    #[cfg(unix)]
    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    #[cfg(windows)]
    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.pipe).poll_flush(context)
    }

    #[cfg(unix)]
    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    #[cfg(windows)]
    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.pipe).poll_shutdown(context)
    }

    #[cfg(unix)]
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.pipe).poll_write_vectored(context, buffers)
    }

    #[cfg(unix)]
    fn is_write_vectored(&self) -> bool {
        true
    }
}

impl TryFrom<std::process::ChildStdin> for ChildStdin {
    type Error = io::Error;

    fn try_from(pipe: std::process::ChildStdin) -> io::Result<Self> {
        Self::from_std(pipe)
    }
}

impl TryFrom<std::process::ChildStdout> for ChildStdout {
    type Error = io::Error;

    fn try_from(pipe: std::process::ChildStdout) -> io::Result<Self> {
        Self::from_std(pipe)
    }
}

impl TryFrom<std::process::ChildStderr> for ChildStderr {
    type Error = io::Error;

    fn try_from(pipe: std::process::ChildStderr) -> io::Result<Self> {
        Self::from_std(pipe)
    }
}
