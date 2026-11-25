use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io,
    io::Write,
    path::{Path, PathBuf},
};

use crate::{TemporaryFile, fill_random};

const PIN_TAG: &str = "pin";

#[derive(Debug, Clone)]
pub struct Revisioned {
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub number: u64,
    pub bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Committed {
    Landed(u64),
    Conflict,
}

struct Pin {
    base: u64,
    pid: u32,
    path: PathBuf,
}

#[derive(Default)]
struct Listing {
    revisions: Vec<u64>,
    pins: Vec<Pin>,
}

struct Pinned {
    path: PathBuf,
}

impl Drop for Pinned {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl Revisioned {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> io::Result<Revision> {
        loop {
            let Some(number) = self.list()?.revisions.into_iter().max() else {
                return Ok(Revision {
                    number: 0,
                    bytes: read_optional(&self.path)?,
                });
            };
            if let Some(bytes) = read_optional(&self.revision_path(number))? {
                return Ok(Revision {
                    number,
                    bytes: Some(bytes),
                });
            }
        }
    }

    pub fn commit(&self, expected: u64, bytes: &[u8]) -> io::Result<Committed> {
        let next = expected.checked_add(1).ok_or_else(|| {
            io::Error::other(format!(
                "{} has no revision after {expected}",
                self.path.display()
            ))
        })?;
        let pinned = self.pin(expected)?;
        if self
            .list()?
            .revisions
            .iter()
            .any(|number| *number > expected)
        {
            return Ok(Committed::Conflict);
        }
        let mut file = TemporaryFile::new_in(self.directory())?;
        file.write_all(bytes)?;
        file.as_file().sync_all()?;
        match file.persist_noclobber(self.revision_path(next)) {
            Ok(_) => {}
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                return Ok(Committed::Conflict);
            }
            Err(error) => return Err(error.error),
        }
        drop(pinned);
        self.prune(next)?;
        Ok(Committed::Landed(next))
    }

    pub fn update<T, E: From<io::Error>>(
        &self,
        mut change: impl FnMut(Option<&[u8]>) -> Result<(Option<Vec<u8>>, T), E>,
    ) -> Result<T, E> {
        loop {
            let current = self.read()?;
            let (bytes, output) = change(current.bytes.as_deref())?;
            let Some(bytes) = bytes else {
                return Ok(output);
            };
            match self.commit(current.number, &bytes)? {
                Committed::Landed(_) => return Ok(output),
                Committed::Conflict => {}
            }
        }
    }

    fn pin(&self, base: u64) -> io::Result<Pinned> {
        let mut random = [0_u8; 8];
        fill_random(&mut random)?;
        let mut name = self.prefix();
        name.push(format!(
            "{PIN_TAG}.{base}.{}.{:016x}",
            std::process::id(),
            u64::from_le_bytes(random)
        ));
        let path = self.path.with_file_name(name);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Pinned { path })
    }

    fn prune(&self, landed: u64) -> io::Result<()> {
        let listing = self.list()?;
        for pin in &listing.pins {
            if pin.base < landed && pin.pid != std::process::id() && !alive(pin.pid) {
                remove_optional(&pin.path)?;
            }
        }
        let pinned = |number: u64| listing.pins.iter().any(|pin| pin.base + 1 == number);
        for number in listing.revisions {
            if number < landed && !pinned(number) {
                remove_optional(&self.revision_path(number))?;
            }
        }
        Ok(())
    }

    fn directory(&self) -> &Path {
        match self.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        }
    }

    fn prefix(&self) -> OsString {
        let mut prefix = self.path.file_name().unwrap_or_default().to_os_string();
        prefix.push(".");
        prefix
    }

    fn revision_path(&self, number: u64) -> PathBuf {
        let mut name = self.prefix();
        name.push(number.to_string());
        self.path.with_file_name(name)
    }

    fn list(&self) -> io::Result<Listing> {
        let prefix = self.prefix();
        let prefix = prefix.as_encoded_bytes();
        let entries = match fs::read_dir(self.directory()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Listing::default());
            }
            Err(error) => return Err(error),
        };
        let mut listing = Listing::default();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(rest) = name
                .as_encoded_bytes()
                .strip_prefix(prefix)
                .and_then(|rest| std::str::from_utf8(rest).ok())
            else {
                continue;
            };
            if let Some(number) = decimal(rest) {
                listing.revisions.push(number);
                continue;
            }
            let mut fields = rest.split('.');
            if fields.next() != Some(PIN_TAG) {
                continue;
            }
            let (Some(base), Some(pid)) = (
                fields.next().and_then(decimal_or_zero),
                fields.next().and_then(|pid| pid.parse().ok()),
            ) else {
                continue;
            };
            listing.pins.push(Pin {
                base,
                pid,
                path: entry.path(),
            });
        }
        Ok(listing)
    }
}

fn decimal(text: &str) -> Option<u64> {
    if text.starts_with('0') {
        return None;
    }
    decimal_or_zero(text)
}

fn decimal_or_zero(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    match crate::unix::probe_process(pid) {
        Ok(()) => true,
        Err(error) => error.raw_os_error() != Some(nanocode_sys::libc::ESRCH),
    }
}

#[cfg(not(unix))]
fn alive(_pid: u32) -> bool {
    true
}

fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn remove_optional(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
