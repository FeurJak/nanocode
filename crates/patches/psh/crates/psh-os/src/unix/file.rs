//! Open files of the Unix host: regular files, devices, pipes, FIFOs, sockets and
//! terminals as pollable [`HostFile`]s that never block the engine thread on something
//! another part of the engine has to do first (see [`super::ready`]).

use nanocode_sys::libc;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::task::{Context, Poll};

use psh_engine::HostFile;

use super::ready;

static CLOSED_AT_START: AtomicU8 = AtomicU8::new(0);

#[used]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
static RECORD_CLOSED: extern "C" fn() = record_closed;

extern "C" fn record_closed() {
    for n in 0..3 {
        // SAFETY: fcntl(F_GETFD) only reads the descriptor table.
        if unsafe { libc::fcntl(n, libc::F_GETFD) } < 0 {
            CLOSED_AT_START.fetch_or(1 << n, Ordering::Relaxed);
        }
    }
}

/// What the `read` builtin took from regular files past its lines and gave back
/// (`HostFile::unread`), by descriptor: each file's offset is that far past what the
/// shell has read, until [`sync_read_ahead`] moves it back.
static KEPT: Mutex<Vec<Kept>> = Mutex::new(Vec::new());

struct Kept {
    fd: RawFd,
    buf: Vec<u8>,
    /// What is kept: `buf[at..]`.
    at: usize,
}

/// The block a read that keeps what is past its line takes from a regular file.
const BLOCK: usize = 4096;

fn kept() -> std::sync::MutexGuard<'static, Vec<Kept>> {
    KEPT.lock().unwrap_or_else(|e| e.into_inner())
}

fn seek_back(k: &Kept) {
    let n = k.buf.len() - k.at;
    if n > 0 {
        // SAFETY: plain call on a descriptor the entry's file holds open.
        unsafe { libc::lseek(k.fd, -(n as libc::off_t), libc::SEEK_CUR) };
    }
}

/// Moves the offset of every file that keeps bytes read ahead back over them, so that
/// another process sharing it reads on where the shell stopped: before a fork or a
/// spawn, and before the process ends.
pub fn sync_read_ahead() {
    let Ok(mut kept) = KEPT.try_lock() else {
        // Held across a fork by a thread the copy does not have: nothing is kept for
        // this thread's files then (a read holds it only while it copies).
        return;
    };
    for k in kept.drain(..) {
        seek_back(&k);
    }
}

/// An open descriptor of the host.
pub struct UnixFile {
    file: File,
    /// Bytes read ahead are kept for this descriptor in [`KEPT`].
    kept: AtomicBool,
    /// A pipe, FIFO, socket or character device: I/O on it may wait. Regular files,
    /// directories and block devices never wait for another process.
    stream: bool,
    /// `O_NONBLOCK` is set: a private pipe end of our own, never shared.
    nonblocking: bool,
    terminal: bool,
    write_waits: bool,
    null: bool,
    #[cfg(target_os = "macos")]
    pipe: bool,
}

impl UnixFile {
    /// Wraps an open descriptor, shared or not; its flags are left alone.
    pub fn new(fd: impl Into<OwnedFd>) -> UnixFile {
        use std::os::unix::fs::FileTypeExt;
        let file = File::from(fd.into());
        let meta = file.metadata().ok();
        let kind = meta.as_ref().map(|m| m.file_type());
        let stream = kind.is_none_or(|ft| !(ft.is_file() || ft.is_dir() || ft.is_block_device()));
        let terminal = stream && {
            use std::io::IsTerminal;
            file.is_terminal()
        };
        let device = kind.is_some_and(|ft| ft.is_char_device());
        UnixFile {
            file,
            kept: AtomicBool::new(false),
            stream,
            nonblocking: false,
            terminal,
            write_waits: stream && (terminal || !device),
            null: device && meta.as_ref().is_some_and(is_dev_null),
            #[cfg(target_os = "macos")]
            pipe: meta.as_ref().is_some_and(|m| {
                use std::os::unix::fs::MetadataExt;
                m.file_type().is_fifo() && m.dev() == 0
            }),
        }
    }

    /// A pipe end only we hold: made non-blocking.
    pub(crate) fn private(fd: OwnedFd) -> io::Result<UnixFile> {
        ready::set_nonblocking(fd.as_raw_fd())?;
        Ok(UnixFile {
            file: File::from(fd),
            kept: AtomicBool::new(false),
            stream: true,
            nonblocking: true,
            terminal: false,
            write_waits: true,
            null: false,
            #[cfg(target_os = "macos")]
            pipe: true,
        })
    }

    /// A duplicate of the process's descriptor `n` (0, 1, 2 for the shell's stdio).
    pub fn stdio(n: RawFd) -> io::Result<UnixFile> {
        if (0..3).contains(&n) && CLOSED_AT_START.load(Ordering::Relaxed) & (1 << n) != 0 {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        // SAFETY: fcntl(F_DUPFD_CLOEXEC) only reads the descriptor table; a failure
        // (n not open) is reported, a success returns a new descriptor we own.
        let fd = unsafe { libc::fcntl(n, libc::F_DUPFD_CLOEXEC, 3) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by fcntl and is owned by nobody else.
        let owned = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        Ok(UnixFile::new(owned))
    }

    pub fn raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    #[cfg(target_os = "macos")]
    fn room(&self) -> usize {
        match self.file.metadata() {
            Ok(m) if self.pipe => ready::PIPE_ROOM
                .saturating_sub(m.len() as usize)
                .max(ready::WRITE_CHUNK),
            _ => ready::WRITE_CHUNK,
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn room(&self) -> usize {
        ready::WRITE_CHUNK
    }

    /// Copies what is kept for this descriptor into `buf`, if anything.
    fn take_kept(&self, buf: &mut [u8]) -> Option<usize> {
        if !self.kept.load(Ordering::Relaxed) {
            return None;
        }
        let fd = self.raw_fd();
        let mut kept = kept();
        let Some(i) = kept.iter().position(|k| k.fd == fd) else {
            self.kept.store(false, Ordering::Relaxed);
            return None;
        };
        let k = &mut kept[i];
        let n = buf.len().min(k.buf.len() - k.at);
        buf[..n].copy_from_slice(&k.buf[k.at..k.at + n]);
        k.at += n;
        if k.at == k.buf.len() {
            kept.swap_remove(i);
            self.kept.store(false, Ordering::Relaxed);
        }
        Some(n)
    }

    /// Moves the offset back over what is kept, before the offset is used.
    fn sync(&self) {
        if !self.kept.swap(false, Ordering::Relaxed) {
            return;
        }
        let fd = self.raw_fd();
        let mut kept = kept();
        if let Some(i) = kept.iter().position(|k| k.fd == fd) {
            seek_back(&kept.swap_remove(i));
        }
    }

    fn wait(&self, cx: &mut Context<'_>, events: i16) -> Poll<io::Result<usize>> {
        match ready::watch(self.raw_fd(), events, cx.waker()) {
            Ok(()) => Poll::Pending,
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

fn is_dev_null(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    static NULL: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *NULL.get_or_init(|| std::fs::metadata("/dev/null").ok().map(|n| n.rdev())) == Some(m.rdev())
}

impl Drop for UnixFile {
    fn drop(&mut self) {
        // Another descriptor or process may share the offset.
        self.sync();
        if self.stream {
            ready::forget(self.raw_fd());
        }
    }
}

impl HostFile for UnixFile {
    fn kind(&self) -> Option<psh_engine::host::FileKind> {
        use psh_engine::host::FileKind;
        use std::os::unix::fs::FileTypeExt;
        let t = self.file.metadata().ok()?.file_type();
        Some(if t.is_file() {
            FileKind::File
        } else if t.is_dir() {
            FileKind::Dir
        } else if t.is_fifo() {
            FileKind::Fifo
        } else if t.is_socket() {
            FileKind::Socket
        } else if t.is_char_device() {
            FileKind::CharDevice
        } else if t.is_block_device() {
            FileKind::BlockDevice
        } else {
            FileKind::Other
        })
    }

    fn poll_read(&self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        if let Some(n) = self.take_kept(buf) {
            return Poll::Ready(Ok(n));
        }
        loop {
            if self.stream && !self.nonblocking {
                match ready::now(self.raw_fd(), ready::READ) {
                    Ok(true) => {}
                    Ok(false) => return self.wait(cx, ready::READ),
                    Err(e) => return Poll::Ready(Err(e)),
                }
            }
            match (&self.file).read(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return self.wait(cx, ready::READ);
                }
                r => return Poll::Ready(r),
            }
        }
    }

    fn poll_write(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.sync();
        let mut buf = buf;
        loop {
            if self.write_waits && !self.nonblocking {
                match ready::now(self.raw_fd(), ready::WRITE) {
                    Ok(true) => {}
                    Ok(false) => return self.wait(cx, ready::WRITE),
                    Err(e) => return Poll::Ready(Err(e)),
                }
                // A blocking write larger than what the pipe has room for would wait.
                buf = &buf[..buf.len().min(self.room())];
            }
            match (&self.file).write(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return self.wait(cx, ready::WRITE);
                }
                r => return Poll::Ready(r),
            }
        }
    }

    fn is_terminal(&self) -> bool {
        self.terminal
    }

    fn is_null(&self) -> bool {
        self.null
    }

    fn seek(&self, pos: io::SeekFrom) -> Option<io::Result<u64>> {
        use std::io::Seek;
        self.sync();
        (!self.stream).then(|| (&self.file).seek(pos))
    }

    fn reads_ahead(&self) -> bool {
        !self.stream
    }

    fn poll_read_ahead(&self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        if self.stream || buf.len() >= BLOCK {
            return self.poll_read(cx, buf);
        }
        if let Some(n) = self.take_kept(buf) {
            return Poll::Ready(Ok(n));
        }
        let mut block = vec![0u8; BLOCK];
        let got = loop {
            match (&self.file).read(&mut block) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                r => break r,
            }
        };
        let n = match got {
            Ok(n) => n,
            Err(e) => return Poll::Ready(Err(e)),
        };
        let given = n.min(buf.len());
        buf[..given].copy_from_slice(&block[..given]);
        if n > given {
            block.truncate(n);
            kept().push(Kept {
                fd: self.raw_fd(),
                buf: block,
                at: given,
            });
            self.kept.store(true, Ordering::Relaxed);
        }
        Poll::Ready(Ok(given))
    }

    fn unread(&self, bytes: &[u8]) {
        if bytes.is_empty() || self.stream {
            return;
        }
        let fd = self.raw_fd();
        let mut kept = kept();
        match kept.iter_mut().find(|k| k.fd == fd) {
            // What was just read from the block: its place is still there.
            Some(k) if k.at >= bytes.len() && k.buf[k.at - bytes.len()..k.at] == *bytes => {
                k.at -= bytes.len();
            }
            Some(k) => {
                let mut buf = bytes.to_vec();
                buf.extend_from_slice(&k.buf[k.at..]);
                k.buf = buf;
                k.at = 0;
            }
            None => kept.push(Kept {
                fd,
                buf: bytes.to_vec(),
                at: 0,
            }),
        }
        self.kept.store(true, Ordering::Relaxed);
    }

    fn identity(&self) -> Option<(u64, u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        let m = self.file.metadata().ok()?;
        m.is_file().then(|| (m.dev(), m.ino(), m.size()))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn reopen_path(&self) -> Option<Vec<u8>> {
        use std::os::fd::AsRawFd;
        let m = self.file.metadata().ok()?;
        m.is_file()
            .then(|| format!("/proc/self/fd/{}", self.file.as_raw_fd()).into_bytes())
    }
}
