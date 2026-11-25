use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use nanocode_platform::{Committed, Revisioned};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Claimed {
    Won(u64),
    Busy,
}

#[derive(Clone, Debug)]
pub struct Claim {
    path: PathBuf,
    lifetime: Duration,
}

impl Claim {
    #[must_use]
    pub fn new(path: PathBuf, lifetime: Duration) -> Self {
        Self { path, lifetime }
    }

    pub async fn take(&self) -> io::Result<Claimed> {
        let (path, lifetime) = (self.path.clone(), self.lifetime);
        blocking(move || take(&path, lifetime)).await
    }

    pub async fn release(&self, revision: u64) -> io::Result<()> {
        let path = self.path.clone();
        blocking(move || release(&path, revision)).await
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    nanocode_platform::spawn_blocking(work)
        .await
        .map_err(io::Error::other)?
}

fn take(path: &Path, lifetime: Duration) -> io::Result<Claimed> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let document = Revisioned::new(path);
    let current = document.read()?;
    let now = nanocode_platform::unix_millis();
    if expires_ms(current.bytes.as_deref())? > now {
        return Ok(Claimed::Busy);
    }
    let lifetime_ms = u64::try_from(lifetime.as_millis())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "claim lifetime overflows"))?;
    let expires = now
        .checked_add(lifetime_ms)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "claim expiry overflows"))?;
    match document.commit(current.number, expires.to_string().as_bytes())? {
        Committed::Landed(revision) => Ok(Claimed::Won(revision)),
        Committed::Conflict => Ok(Claimed::Busy),
    }
}

fn release(path: &Path, revision: u64) -> io::Result<()> {
    match Revisioned::new(path).commit(revision, b"")? {
        Committed::Landed(_) | Committed::Conflict => Ok(()),
    }
}

fn expires_ms(bytes: Option<&[u8]>) -> io::Result<u64> {
    match bytes {
        None | Some([]) => Ok(0),
        Some(bytes) => std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "claim holds no expiry")),
    }
}
