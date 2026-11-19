use crate::{
    model::{Caps, Digest, Entry, MODE_BITS, Manifest, Op, Refusal, parent},
    store::{Batch, Blob, Failure, Store},
};
use nanocode_bytes::{Bytes, BytesMut};
use nanocode_digest::Sha256;
use nanocode_shell::{FileSystem, Kind, Machine, Remove, Reply, Stat, Write, normalize};
use nanocode_sync::Semaphore;
use std::{cell::RefCell, collections::BTreeMap, io, num::NonZeroUsize, rc::Rc, sync::Arc};

pub const SYMLINK_HOPS: u32 = 40;
const FILE_MODE: u32 = 0o644;
const ROOT_MODE: u32 = 0o755;
const LINK_MODE: u32 = 0o777;

struct Inner {
    store: Arc<dyn Store>,
    workspace: String,
    caps: Caps,
    machine: Rc<dyn Machine>,
    manifest: RefCell<Manifest>,
    blobs: RefCell<BTreeMap<Digest, Blob>>,
    gate: Semaphore,
}

#[derive(Clone)]
pub struct WorkspaceFs {
    inner: Rc<Inner>,
}

pub struct Upload {
    fs: WorkspaceFs,
    path: Vec<u8>,
    expected: Digest,
    mode: u32,
    hasher: Sha256,
    data: BytesMut,
    size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Download {
    pub digest: Digest,
    pub mode: u32,
    pub mtime_ns: i64,
    pub bytes: Bytes,
}

impl Download {
    pub fn chunks(&self, chunk: NonZeroUsize) -> impl Iterator<Item = Bytes> + '_ {
        let chunk = chunk.get();
        (0..self.bytes.len())
            .step_by(chunk)
            .map(move |at| self.bytes.slice(at..self.bytes.len().min(at + chunk)))
    }
}

fn invalid(why: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why)
}

fn missing<T>() -> io::Result<T> {
    Err(io::ErrorKind::NotFound.into())
}

fn key(path: &[u8]) -> io::Result<String> {
    let path = normalize(path);
    let text = std::str::from_utf8(&path).map_err(|_| invalid("workspace paths are UTF-8"))?;
    Ok(text.trim_start_matches('/').to_owned())
}

fn resolve(manifest: &Manifest, path: &str, follow: bool) -> io::Result<String> {
    let mut rest: Vec<String> = path
        .split('/')
        .filter(|part| !part.is_empty())
        .rev()
        .map(str::to_owned)
        .collect();
    let mut found: Vec<String> = Vec::new();
    let mut hops = 0;
    while let Some(part) = rest.pop() {
        match part.as_str() {
            "." => continue,
            ".." => {
                found.pop();
                continue;
            }
            _ => found.push(part),
        }
        let Some(Entry::Symlink { target, .. }) = manifest.get(&found.join("/")) else {
            continue;
        };
        if rest.is_empty() && !follow {
            break;
        }
        hops += 1;
        if hops > SYMLINK_HOPS {
            return Err(io::Error::other("Too many levels of symbolic links"));
        }
        found.pop();
        if target.starts_with('/') {
            found.clear();
        }
        rest.extend(
            target
                .split('/')
                .filter(|part| !part.is_empty())
                .rev()
                .map(str::to_owned),
        );
    }
    Ok(found.join("/"))
}

const fn stat_of(entry: &Entry) -> Stat {
    match entry {
        Entry::File {
            size,
            mode,
            mtime_ns,
            ..
        } => Stat {
            kind: Kind::File,
            size: *size,
            mode: *mode,
            mtime_ns: *mtime_ns,
        },
        Entry::Dir { mode, mtime_ns } => Stat {
            kind: Kind::Dir,
            size: 0,
            mode: *mode,
            mtime_ns: *mtime_ns,
        },
        Entry::Symlink { target, mtime_ns } => Stat {
            kind: Kind::Symlink,
            size: target.len() as u64,
            mode: LINK_MODE,
            mtime_ns: *mtime_ns,
        },
    }
}

fn dir_exists(manifest: &Manifest, path: &str) -> io::Result<()> {
    let Some(dir) = parent(path) else {
        return Ok(());
    };
    let dir = resolve(manifest, dir, true)?;
    if dir.is_empty() {
        return Ok(());
    }
    match manifest.get(&dir) {
        Some(Entry::Dir { .. }) => Ok(()),
        Some(_) => Err(io::ErrorKind::NotADirectory.into()),
        None => missing(),
    }
}

fn touch(ops: &mut Vec<Op>, path: &str, mtime_ns: i64) {
    if let Some(dir) = parent(path) {
        ops.push(Op::SetMtime {
            path: dir.to_owned(),
            mtime_ns,
        });
    }
}

impl WorkspaceFs {
    pub async fn open(
        store: Arc<dyn Store>,
        workspace: impl Into<String>,
        caps: Caps,
        machine: Rc<dyn Machine>,
    ) -> io::Result<Self> {
        let workspace = workspace.into();
        let manifest = store.load(&workspace).await?;
        Ok(Self {
            inner: Rc::new(Inner {
                store,
                workspace,
                caps,
                machine,
                manifest: RefCell::new(manifest),
                blobs: RefCell::new(BTreeMap::new()),
                gate: Semaphore::new(1),
            }),
        })
    }

    #[must_use]
    pub fn manifest(&self) -> Manifest {
        self.inner.manifest.borrow().clone()
    }

    #[must_use]
    pub fn version(&self) -> u64 {
        self.inner.manifest.borrow().version()
    }

    #[must_use]
    pub fn workspace(&self) -> &str {
        &self.inner.workspace
    }

    #[must_use]
    pub fn caps(&self) -> Caps {
        self.inner.caps
    }

    pub async fn refresh(&self) -> io::Result<u64> {
        let inner = &self.inner;
        let _turn = inner.gate.acquire().await.map_err(io::Error::other)?;
        inner.reload().await
    }

    pub async fn commit(&self, ops: Vec<Op>, fresh: Vec<Blob>) -> io::Result<u64> {
        let inner = &self.inner;
        let _turn = inner.gate.acquire().await.map_err(io::Error::other)?;
        inner.commit(&ops, fresh).await
    }

    pub async fn bytes(&self, path: &[u8]) -> io::Result<Bytes> {
        let (_, bytes) = self.inner.file(path).await?;
        Ok(bytes)
    }

    #[must_use]
    pub fn upload(&self, path: &[u8], expected: Digest, mode: u32) -> Upload {
        Upload {
            fs: self.clone(),
            path: path.to_vec(),
            expected,
            mode,
            hasher: Sha256::new(),
            data: BytesMut::new(),
            size: 0,
        }
    }

    pub async fn download(&self, path: &[u8]) -> io::Result<Download> {
        let (entry, bytes) = self.inner.file(path).await?;
        let Entry::File {
            digest,
            mode,
            mtime_ns,
            ..
        } = entry
        else {
            unreachable!("file() answers files only");
        };
        Ok(Download {
            digest,
            mode,
            mtime_ns,
            bytes,
        })
    }
}

impl Upload {
    pub fn push(&mut self, chunk: &[u8]) -> io::Result<()> {
        let size = self.size + chunk.len() as u64;
        let max = self.fs.inner.caps.file_bytes;
        if size > max {
            return Err(Refusal::FileTooLarge {
                path: String::from_utf8_lossy(&self.path).into_owned(),
                size,
                max,
            }
            .into());
        }
        self.size = size;
        self.hasher.update(chunk);
        self.data.extend_from_slice(chunk);
        Ok(())
    }

    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    pub async fn finish(self) -> io::Result<u64> {
        let digest = Digest(self.hasher.finalize());
        if digest != self.expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "uploaded bytes hash to {}, not the expected {}",
                    digest.hex(),
                    self.expected.hex()
                ),
            ));
        }
        let inner = &self.fs.inner;
        let _turn = inner.gate.acquire().await.map_err(io::Error::other)?;
        let path = key(&self.path)?;
        let now = inner.now();
        let ops = {
            let manifest = inner.manifest.borrow();
            let path = resolve(&manifest, &path, true)?;
            if path.is_empty() {
                return Err(io::ErrorKind::IsADirectory.into());
            }
            dir_exists(&manifest, &path)?;
            if manifest.get(&path).is_some_and(Entry::is_dir) {
                return Err(io::ErrorKind::IsADirectory.into());
            }
            let mut ops = vec![Op::Put {
                path: path.clone(),
                digest,
                size: self.size,
                mode: self.mode & MODE_BITS,
                mtime_ns: now,
            }];
            touch(&mut ops, &path, now);
            ops
        };
        let blob = Blob::verified(digest, self.data.freeze())?;
        inner.commit(&ops, vec![blob]).await
    }
}

impl Inner {
    fn now(&self) -> i64 {
        i64::try_from(self.machine.now().as_nanos()).unwrap_or(i64::MAX)
    }

    async fn reload(&self) -> io::Result<u64> {
        let manifest = self.store.load(&self.workspace).await?;
        let version = manifest.version();
        self.blobs
            .borrow_mut()
            .retain(|digest, _| manifest.holds(digest));
        *self.manifest.borrow_mut() = manifest;
        Ok(version)
    }

    async fn commit(&self, ops: &[Op], fresh: Vec<Blob>) -> io::Result<u64> {
        let (next, change) = {
            let manifest = self.manifest.borrow();
            manifest.apply(manifest.version(), ops, &self.caps)?
        };
        let fresh: BTreeMap<Digest, Blob> = fresh
            .into_iter()
            .map(|blob| (*blob.digest(), blob))
            .collect();
        let batch = {
            let cached = self.blobs.borrow();
            Batch::new(change, |digest| {
                fresh.get(digest).or_else(|| cached.get(digest)).cloned()
            })
            .map_err(Failure::Missing)?
        };
        match self.store.commit(&self.workspace, &batch).await {
            Ok(version) => {
                assert_eq!(version, next.version(), "a store commits the next version");
                let mut blobs = self.blobs.borrow_mut();
                for blob in batch.blobs() {
                    blobs.insert(*blob.digest(), blob.clone());
                }
                for digest in &batch.change().dropped {
                    blobs.remove(digest);
                }
                *self.manifest.borrow_mut() = next;
                Ok(version)
            }
            Err(failure) => {
                if let Failure::Refused(Refusal::Stale { .. }) | Failure::Backend(_) = &failure {
                    let _ = self.reload().await;
                }
                Err(failure.into())
            }
        }
    }

    async fn blob(&self, digest: &Digest) -> io::Result<Bytes> {
        if let Some(blob) = self.blobs.borrow().get(digest) {
            return Ok(blob.bytes().clone());
        }
        let blob = self.store.blob(digest).await?;
        if blob.digest() != digest {
            return Err(Failure::Missing(*digest).into());
        }
        let bytes = blob.bytes().clone();
        if self.manifest.borrow().holds(digest) {
            self.blobs.borrow_mut().insert(*digest, blob);
        }
        Ok(bytes)
    }

    fn lookup(&self, path: &[u8], follow: bool) -> io::Result<(String, Option<Entry>)> {
        let path = key(path)?;
        let manifest = self.manifest.borrow();
        let path = resolve(&manifest, &path, follow)?;
        let entry = manifest.get(&path).cloned();
        Ok((path, entry))
    }

    async fn file(&self, path: &[u8]) -> io::Result<(Entry, Bytes)> {
        match self.lookup(path, true)? {
            (path, _) if path.is_empty() => Err(io::ErrorKind::IsADirectory.into()),
            (_, Some(entry @ Entry::File { digest, .. })) => {
                let bytes = self.blob(&digest).await?;
                Ok((entry, bytes))
            }
            (_, Some(Entry::Dir { .. })) => Err(io::ErrorKind::IsADirectory.into()),
            (_, Some(Entry::Symlink { .. }) | None) => missing(),
        }
    }

    async fn write(&self, path: &[u8], data: &[u8], how: Write) -> io::Result<()> {
        let _turn = self.gate.acquire().await.map_err(io::Error::other)?;
        let (path, entry) = self.lookup(path, true)?;
        if path.is_empty() {
            return Err(io::ErrorKind::IsADirectory.into());
        }
        dir_exists(&self.manifest.borrow(), &path)?;
        let (bytes, mode) = match (entry, how) {
            (Some(Entry::Dir { .. }), _) => return Err(io::ErrorKind::IsADirectory.into()),
            (Some(Entry::Symlink { .. }), _) => return missing(),
            (Some(Entry::File { .. }), Write::New) => {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            (Some(Entry::File { mode, .. }), Write::Replace) => {
                (Bytes::copy_from_slice(data), mode)
            }
            (
                Some(Entry::File {
                    digest, mode, size, ..
                }),
                Write::Append,
            ) => {
                let grown = size + data.len() as u64;
                if grown > self.caps.file_bytes {
                    return Err(Refusal::FileTooLarge {
                        path,
                        size: grown,
                        max: self.caps.file_bytes,
                    }
                    .into());
                }
                let held = self.blob(&digest).await?;
                let mut bytes = BytesMut::with_capacity(held.len() + data.len());
                bytes.extend_from_slice(&held);
                bytes.extend_from_slice(data);
                (bytes.freeze(), mode)
            }
            (None, _) => (Bytes::copy_from_slice(data), FILE_MODE),
        };
        let blob = Blob::new(bytes);
        let now = self.now();
        let mut ops = vec![Op::Put {
            path: path.clone(),
            digest: *blob.digest(),
            size: blob.bytes().len() as u64,
            mode,
            mtime_ns: now,
        }];
        touch(&mut ops, &path, now);
        self.commit(&ops, vec![blob]).await.map(drop)
    }

    async fn mkdir(&self, path: &[u8], mode: u32) -> io::Result<()> {
        let _turn = self.gate.acquire().await.map_err(io::Error::other)?;
        let (path, entry) = self.lookup(path, false)?;
        if path.is_empty() || entry.is_some() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        dir_exists(&self.manifest.borrow(), &path)?;
        let now = self.now();
        let mut ops = vec![Op::Mkdir {
            path: path.clone(),
            mode: mode & MODE_BITS,
            mtime_ns: now,
        }];
        touch(&mut ops, &path, now);
        self.commit(&ops, Vec::new()).await.map(drop)
    }

    async fn remove(&self, path: &[u8], what: Remove) -> io::Result<()> {
        let _turn = self.gate.acquire().await.map_err(io::Error::other)?;
        let (path, entry) = self.lookup(path, false)?;
        if path.is_empty() {
            return Err(invalid("the workspace root cannot be removed"));
        }
        dir_exists(&self.manifest.borrow(), &path)?;
        match (entry, what) {
            (None, _) => return missing(),
            (Some(Entry::Dir { .. }), Remove::File) => {
                return Err(io::ErrorKind::IsADirectory.into());
            }
            (Some(Entry::File { .. } | Entry::Symlink { .. }), Remove::Dir) => {
                return Err(io::ErrorKind::NotADirectory.into());
            }
            _ => {}
        }
        let mut ops = vec![Op::Remove {
            path: path.clone(),
            recursive: false,
        }];
        touch(&mut ops, &path, self.now());
        self.commit(&ops, Vec::new()).await.map(drop)
    }

    async fn rename(&self, from: &[u8], to: &[u8]) -> io::Result<()> {
        let _turn = self.gate.acquire().await.map_err(io::Error::other)?;
        let (from, entry) = self.lookup(from, false)?;
        let (to, _) = self.lookup(to, false)?;
        if from.is_empty() || to.is_empty() || to.starts_with(&format!("{from}/")) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if entry.is_none() {
            return missing();
        }
        dir_exists(&self.manifest.borrow(), &from)?;
        dir_exists(&self.manifest.borrow(), &to)?;
        let now = self.now();
        let mut ops = vec![Op::Move {
            from: from.clone(),
            to: to.clone(),
        }];
        touch(&mut ops, &from, now);
        if parent(&from) != parent(&to) {
            touch(&mut ops, &to, now);
        }
        self.commit(&ops, Vec::new()).await.map(drop)
    }

    async fn symlink(&self, target: &[u8], link: &[u8]) -> io::Result<()> {
        let _turn = self.gate.acquire().await.map_err(io::Error::other)?;
        let target = std::str::from_utf8(target)
            .map_err(|_| invalid("workspace symlink targets are UTF-8"))?
            .to_owned();
        let (path, entry) = self.lookup(link, false)?;
        if path.is_empty() || entry.is_some() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        dir_exists(&self.manifest.borrow(), &path)?;
        let now = self.now();
        let mut ops = vec![Op::Symlink {
            path: path.clone(),
            target,
            mtime_ns: now,
        }];
        touch(&mut ops, &path, now);
        self.commit(&ops, Vec::new()).await.map(drop)
    }

    async fn set_mode(&self, path: &[u8], mode: u32) -> io::Result<()> {
        let _turn = self.gate.acquire().await.map_err(io::Error::other)?;
        match self.lookup(path, true)? {
            (path, _) if path.is_empty() => Err(invalid("the workspace root keeps its mode")),
            (path, Some(Entry::File { .. } | Entry::Dir { .. })) => {
                let ops = [Op::SetMode {
                    path,
                    mode: mode & MODE_BITS,
                }];
                self.commit(&ops, Vec::new()).await.map(drop)
            }
            (_, Some(Entry::Symlink { .. }) | None) => missing(),
        }
    }

    async fn set_mtime(&self, path: &[u8], mtime_ns: i64) -> io::Result<()> {
        let _turn = self.gate.acquire().await.map_err(io::Error::other)?;
        match self.lookup(path, true)? {
            (path, _) if path.is_empty() => Err(invalid("the workspace root keeps its time")),
            (path, Some(_)) => {
                let ops = [Op::SetMtime { path, mtime_ns }];
                self.commit(&ops, Vec::new()).await.map(drop)
            }
            (_, None) => missing(),
        }
    }
}

fn later<T: 'static>(
    inner: &Rc<Inner>,
    run: impl AsyncFnOnce(&Inner) -> io::Result<T> + 'static,
) -> Reply<T> {
    let inner = Rc::clone(inner);
    Reply::Later(Box::pin(async move { run(&inner).await }))
}

impl FileSystem for WorkspaceFs {
    fn stat(&self, path: &[u8], follow: bool) -> Reply<Stat> {
        Reply::Now(match self.inner.lookup(path, follow) {
            Ok((path, _)) if path.is_empty() => Ok(Stat {
                kind: Kind::Dir,
                size: 0,
                mode: ROOT_MODE,
                mtime_ns: 0,
            }),
            Ok((_, Some(entry))) => Ok(stat_of(&entry)),
            Ok((_, None)) => missing(),
            Err(error) => Err(error),
        })
    }

    fn read(&self, path: &[u8], limit: u64) -> Reply<Bytes> {
        let path = path.to_vec();
        later(&self.inner, async move |inner: &Inner| {
            let (entry, bytes) = inner.file(&path).await?;
            if entry.size() > limit {
                return Err(io::ErrorKind::FileTooLarge.into());
            }
            Ok(bytes)
        })
    }

    fn write(&self, path: &[u8], data: &[u8], how: Write) -> Reply<()> {
        let (path, data) = (path.to_vec(), data.to_vec());
        later(&self.inner, async move |inner: &Inner| {
            inner.write(&path, &data, how).await
        })
    }

    fn list(&self, path: &[u8]) -> Reply<Vec<Vec<u8>>> {
        Reply::Now(self.inner.lookup(path, true).and_then(|(path, entry)| {
            match entry {
                Some(Entry::Dir { .. }) => {}
                None if path.is_empty() => {}
                Some(_) => return Err(io::ErrorKind::NotADirectory.into()),
                None => return missing(),
            }
            Ok(self
                .inner
                .manifest
                .borrow()
                .children(&path)
                .map(|name| name.as_bytes().to_vec())
                .collect())
        }))
    }

    fn mkdir(&self, path: &[u8], mode: u32) -> Reply<()> {
        let path = path.to_vec();
        later(&self.inner, async move |inner: &Inner| {
            inner.mkdir(&path, mode).await
        })
    }

    fn remove(&self, path: &[u8], what: Remove) -> Reply<()> {
        let path = path.to_vec();
        later(&self.inner, async move |inner: &Inner| {
            inner.remove(&path, what).await
        })
    }

    fn rename(&self, from: &[u8], to: &[u8]) -> Reply<()> {
        let (from, to) = (from.to_vec(), to.to_vec());
        later(&self.inner, async move |inner: &Inner| {
            inner.rename(&from, &to).await
        })
    }

    fn symlink(&self, target: &[u8], link: &[u8]) -> Reply<()> {
        let (target, link) = (target.to_vec(), link.to_vec());
        later(&self.inner, async move |inner: &Inner| {
            inner.symlink(&target, &link).await
        })
    }

    fn read_link(&self, path: &[u8]) -> Reply<Vec<u8>> {
        Reply::Now(match self.inner.lookup(path, false) {
            Ok((_, Some(Entry::Symlink { target, .. }))) => Ok(target.into_bytes()),
            Ok((_, Some(_))) => Err(io::ErrorKind::InvalidInput.into()),
            Ok((path, None)) if path.is_empty() => Err(io::ErrorKind::InvalidInput.into()),
            Ok((_, None)) => missing(),
            Err(error) => Err(error),
        })
    }

    fn set_mode(&self, path: &[u8], mode: u32) -> Reply<()> {
        let path = path.to_vec();
        later(&self.inner, async move |inner: &Inner| {
            inner.set_mode(&path, mode).await
        })
    }

    fn set_mtime(&self, path: &[u8], mtime_ns: i64) -> Reply<()> {
        let path = path.to_vec();
        later(&self.inner, async move |inner: &Inner| {
            inner.set_mtime(&path, mtime_ns).await
        })
    }
}
