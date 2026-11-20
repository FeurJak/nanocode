use std::{
    cell::Cell,
    collections::BTreeMap,
    future::Future,
    num::NonZeroUsize,
    ops::Range,
    path::PathBuf,
    pin::Pin,
    process::ExitCode,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    },
    time::Duration,
};

use nanocode_code_mode::{ToolContext, ToolOutput, ToolOutputBody};
use nanocode_durability_core::StoreFuture;
use nanocode_hands::{
    ChunkError, Chunks, Clock as RouterClock, CopyError, Fetch, GetError, HandError, HandHost,
    HandPhase, HandRef, HandStatus, HostFuture, Placement, Placements, Policy, PutError, Router,
    Sha256Digest, StepOutcome, Stored, Target, Workspace as HandWorkspace, receive,
    sim::{self as hands_sim, Mutation as HandRule},
};
use nanocode_shell::{Commands, FileSystem, HostInfo, Identity, Limits, Machine, NoCommands};
use nanocode_sync::{Atom, block_on};
use nanocode_workspace::{
    Config, Workspace, WorkspaceHost,
    sim::{self as workspace_sim, Mutation as WorkspaceRule},
};
use nanocode_workspace_fs::{
    Batch, Blob, Caps, Digest, Failure, Manifest, MemStore, Store, WorkspaceFs,
    sim::{self as fs_sim, Mutation as FsRule},
};
use serde_json::{Value, json};

use crate::rng::{FNV_OFFSET, Rng, fnv};

const ROOT: &str = "/w";
const NAME: &str = "workspace";
const SESSION: &str = "session";
const STORE_KEY: &str = "stack";
const FILES: [&str; 4] = ["a.txt", "b.txt", "d/c.txt", "d/e/f.txt"];
const COPIES: [&str; 2] = ["copy/a.txt", "copy/b.txt"];
const SHELL_FILES: [&str; 2] = ["a.txt", "b.txt"];
const PIECES: [&str; 5] = ["a", "b", "ab", "\n", "c"];
const SLICE: u32 = 4096;
const DEPTH: u32 = 64;
const OPS_MIN: u64 = 12;
const OPS_MAX: u64 = 48;
const CHUNK_MAX: u64 = 6;
const KINDS: usize = 7;
const CRASH_IN: usize = 4;
const CORRUPT_IN: usize = 3;
const CUT_IN: usize = 2;
const UNCUT: usize = usize::MAX;
const IDLE_MS: u64 = 1_000;
const CAPS: Caps = Caps {
    file_bytes: 1 << 16,
    workspace_bytes: 1 << 20,
    paths: 256,
    ops: 256,
    path_bytes: 256,
};
const PLAN_NONE: u8 = 0;
const PLAN_BEFORE: u8 = 1;
const PLAN_AFTER: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceStackMutation {
    None,
    TornCommit,
    VisibleBeforeCommit,
    ShaUnchecked,
    PutsInPlace,
}

impl WorkspaceStackMutation {
    const ALL: [Self; 5] = [
        Self::None,
        Self::TornCommit,
        Self::VisibleBeforeCommit,
        Self::ShaUnchecked,
        Self::PutsInPlace,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::TornCommit => "torn_commit",
            Self::VisibleBeforeCommit => "visible_before_commit",
            Self::ShaUnchecked => "sha_unchecked",
            Self::PutsInPlace => "puts_in_place",
        }
    }

    pub fn names() -> String {
        Self::ALL
            .iter()
            .skip(1)
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }

    const fn rules(self) -> (FsRule, HandRule, WorkspaceRule) {
        match self {
            Self::None => (FsRule::None, HandRule::None, WorkspaceRule::None),
            Self::TornCommit => (FsRule::TornCommit, HandRule::None, WorkspaceRule::None),
            Self::VisibleBeforeCommit => (
                FsRule::VisibleBeforeCommit,
                HandRule::None,
                WorkspaceRule::None,
            ),
            Self::ShaUnchecked => (FsRule::None, HandRule::ShaUnchecked, WorkspaceRule::None),
            Self::PutsInPlace => (FsRule::None, HandRule::None, WorkspaceRule::PutsInPlace),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct WorkspaceStackStats {
    pub seeds: u64,
    pub writes: u64,
    pub edits: u64,
    pub reads: u64,
    pub scripts: u64,
    pub routed: u64,
    pub copies: u64,
    pub copies_corrupt: u64,
    pub copies_cut: u64,
    pub restarts: u64,
    pub crashes_before: u64,
    pub crashes_after: u64,
}

impl WorkspaceStackStats {
    const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.writes += other.writes;
        self.edits += other.edits;
        self.reads += other.reads;
        self.scripts += other.scripts;
        self.routed += other.routed;
        self.copies += other.copies;
        self.copies_corrupt += other.copies_corrupt;
        self.copies_cut += other.copies_cut;
        self.restarts += other.restarts;
        self.crashes_before += other.crashes_before;
        self.crashes_after += other.crashes_after;
    }
}

pub struct WorkspaceStackOutcome {
    digest: u64,
    wrong: Option<String>,
    stats: WorkspaceStackStats,
    shape: String,
}

struct Crashing {
    inner: MemStore,
    plan: AtomicU8,
    fired: AtomicU8,
}

impl Store for Crashing {
    fn load<'a>(&'a self, workspace: &'a str) -> StoreFuture<'a, Result<Manifest, Failure>> {
        self.inner.load(workspace)
    }

    fn commit<'a>(
        &'a self,
        workspace: &'a str,
        batch: &'a Batch,
    ) -> StoreFuture<'a, Result<u64, Failure>> {
        let plan = self.plan.swap(PLAN_NONE, Ordering::SeqCst);
        if plan != PLAN_NONE {
            self.fired.store(plan, Ordering::SeqCst);
        }
        Box::pin(async move {
            match plan {
                PLAN_BEFORE => Err(Failure::Backend(
                    "the process died before the commit".to_owned(),
                )),
                PLAN_AFTER => {
                    self.inner.commit(workspace, batch).await?;
                    Err(Failure::Backend(
                        "the process died after the commit".to_owned(),
                    ))
                }
                _ => self.inner.commit(workspace, batch).await,
            }
        })
    }

    fn blob<'a>(&'a self, digest: &'a Digest) -> StoreFuture<'a, Result<Blob, Failure>> {
        self.inner.blob(digest)
    }
}

struct Clock {
    ticks: Cell<u64>,
}

impl Machine for Clock {
    fn now(&self) -> Duration {
        let ticks = self.ticks.get() + 1;
        self.ticks.set(ticks);
        Duration::from_millis(ticks)
    }

    fn sleep(&self, _duration: Duration) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(std::future::pending())
    }

    fn random(&self, bytes: &mut [u8]) {
        let ticks = self.ticks.get() + 1;
        self.ticks.set(ticks);
        let mut rng = Rng::derive(ticks, "workspace_stack_random");
        for byte in bytes {
            *byte = rng.draw() as u8;
        }
    }
}

struct Slices {
    bytes: Vec<u8>,
    sent: usize,
    size: usize,
}

impl Chunks for Slices {
    fn next(&mut self) -> HostFuture<'_, Result<Option<Vec<u8>>, ChunkError>> {
        let end = self.bytes.len().min(self.sent + self.size);
        let chunk = (self.sent < end).then(|| self.bytes[self.sent..end].to_vec());
        self.sent = end;
        Box::pin(async move { Ok(chunk) })
    }
}

struct Corrupt<'a> {
    inner: Box<dyn Chunks + 'a>,
    flipped: bool,
}

impl Chunks for Corrupt<'_> {
    fn next(&mut self) -> HostFuture<'_, Result<Option<Vec<u8>>, ChunkError>> {
        Box::pin(async move {
            let chunk = self.inner.next().await?;
            if self.flipped {
                return Ok(chunk);
            }
            self.flipped = true;
            Ok(Some(match chunk {
                Some(mut chunk) => {
                    chunk[0] ^= 1;
                    chunk
                }
                None => vec![0],
            }))
        })
    }
}

struct Cut<'a> {
    inner: Box<dyn Chunks + 'a>,
    left: usize,
}

impl Chunks for Cut<'_> {
    fn next(&mut self) -> HostFuture<'_, Result<Option<Vec<u8>>, ChunkError>> {
        Box::pin(async move {
            if self.left == 0 {
                return Err(ChunkError::Lost {
                    reason: "the Hand died mid-stream".into(),
                });
            }
            self.left -= 1;
            self.inner.next().await
        })
    }
}

fn cut<'a>(fetch: Fetch<'a>, after: &AtomicUsize) -> Fetch<'a> {
    match after.swap(UNCUT, Ordering::SeqCst) {
        UNCUT => fetch,
        left => Fetch {
            size_bytes: fetch.size_bytes,
            sha256: fetch.sha256,
            chunks: Box::new(Cut {
                inner: fetch.chunks,
                left,
            }),
        },
    }
}

fn damaged<'a>(fetch: Fetch<'a>, corrupt: &AtomicBool) -> Fetch<'a> {
    match corrupt.swap(false, Ordering::SeqCst) {
        false => fetch,
        true => Fetch {
            size_bytes: fetch.size_bytes,
            sha256: fetch.sha256,
            chunks: Box::new(Corrupt {
                inner: fetch.chunks,
                flipped: false,
            }),
        },
    }
}

struct Wire {
    host: WorkspaceHost,
    corrupt: Arc<AtomicBool>,
}

impl HandWorkspace for Wire {
    fn runs(&self, tool: &str) -> bool {
        self.host.runs(tool)
    }

    fn run<'a>(
        &'a self,
        session: &'a str,
        tool: &'a str,
        input: Value,
        context: ToolContext<'a>,
    ) -> HostFuture<'a, ToolOutput> {
        HandWorkspace::run(&self.host, session, tool, input, context)
    }

    fn put<'a>(
        &'a self,
        session: &'a str,
        path: &'a str,
        chunks: Box<dyn Chunks + 'a>,
        sha256: Sha256Digest,
    ) -> HostFuture<'a, Result<Stored, PutError>> {
        HandWorkspace::put(&self.host, session, path, chunks, sha256)
    }

    fn get<'a>(
        &'a self,
        session: &'a str,
        path: &'a str,
        chunk_bytes: NonZeroUsize,
    ) -> HostFuture<'a, Result<Fetch<'a>, GetError>> {
        Box::pin(async move {
            let fetch = HandWorkspace::get(&self.host, session, path, chunk_bytes).await?;
            Ok(damaged(fetch, &self.corrupt))
        })
    }
}

struct Hand {
    files: Atom<BTreeMap<String, Vec<u8>>>,
    corrupt: AtomicBool,
    cut: AtomicUsize,
}

impl HandHost for Hand {
    fn status<'a>(
        &'a self,
        _session: &'a str,
    ) -> HostFuture<'a, Result<Vec<HandStatus>, HandError>> {
        Box::pin(async move {
            Ok(vec![HandStatus {
                hand: HandRef::Hosted,
                tools: Vec::new(),
                phase: HandPhase::Running { since_ms: 0 },
            }])
        })
    }

    fn run<'a>(
        &'a self,
        _session: &'a str,
        _step: &'a str,
        _hand: &'a HandRef,
        tool: &'a str,
        _input: Value,
        _context: ToolContext<'a>,
    ) -> HostFuture<'a, StepOutcome> {
        Box::pin(async move {
            StepOutcome::Refused {
                reason: format!("this Hand runs no {tool}").into(),
            }
        })
    }

    fn put<'a>(
        &'a self,
        _session: &'a str,
        _hand: &'a HandRef,
        path: &'a str,
        mut chunks: Box<dyn Chunks + 'a>,
        sha256: Sha256Digest,
    ) -> HostFuture<'a, Result<Stored, PutError>> {
        Box::pin(async move {
            let (bytes, stored) = receive(chunks.as_mut(), sha256, u64::MAX).await?;
            self.files.update(|files| {
                files.insert(path.to_owned(), bytes.clone());
            });
            Ok(stored)
        })
    }

    fn get<'a>(
        &'a self,
        _session: &'a str,
        _hand: &'a HandRef,
        path: &'a str,
        chunk_bytes: NonZeroUsize,
    ) -> HostFuture<'a, Result<Fetch<'a>, GetError>> {
        Box::pin(async move {
            let bytes = self
                .files
                .read(|files| files.get(path).cloned())
                .ok_or(GetError::NotFound)?;
            let fetch = Fetch {
                size_bytes: bytes.len() as u64,
                sha256: Sha256Digest::of(&bytes),
                chunks: Box::new(Slices {
                    bytes,
                    sent: 0,
                    size: chunk_bytes.get(),
                }),
            };
            Ok(cut(damaged(fetch, &self.corrupt), &self.cut))
        })
    }

    fn release<'a>(
        &'a self,
        _session: &'a str,
        _hand: &'a HandRef,
    ) -> HostFuture<'a, Result<(), HandError>> {
        Box::pin(async move { Ok(()) })
    }
}

struct Still;

impl RouterClock for Still {
    fn now_ms(&self) -> u64 {
        0
    }
}

struct Unplaced;

impl Placements for Unplaced {
    fn placed(&self, _session: &str, _step: &str, _placement: Placement) {}
}

struct Stack {
    files: Workspace,
    host: WorkspaceHost,
    router: Router,
}

fn clock() -> Rc<dyn Machine> {
    Rc::new(Clock {
        ticks: Cell::new(0),
    })
}

fn open(
    store: &Arc<Crashing>,
    hand: &Arc<Hand>,
    corrupt: &Arc<AtomicBool>,
) -> Result<Stack, String> {
    let store: Arc<dyn Store> = store.clone();
    let files = Workspace::start(Config {
        files: Arc::new(move || -> Rc<dyn FileSystem> {
            let opened = block_on(WorkspaceFs::open(
                Arc::clone(&store),
                STORE_KEY,
                CAPS,
                clock(),
            ));
            Rc::new(opened.expect("a workspace opens over the in-memory store"))
        }),
        machine: Arc::new(clock),
        fetch: None,
        credentials: None,
        commands: Arc::new(|| -> Rc<dyn Commands> { Rc::new(NoCommands) }),
        identity: Identity {
            info: HostInfo::default(),
            pid: 1,
            user: b"user".to_vec(),
            group: b"user".to_vec(),
            home: ROOT.as_bytes().to_vec(),
        },
        limits: Limits {
            fuel: u64::MAX,
            output: u64::MAX,
            file: u64::MAX,
            operations: u64::MAX,
            depth: DEPTH,
        },
        env: Vec::new(),
        slice: SLICE,
    })
    .map_err(|error| format!("the workspace's worker starts: {error}"))?;
    let built = nanocode::claude::host_tools(
        PathBuf::from(ROOT),
        Arc::new(files.clone()),
        Arc::new(files.clone()),
    )
    .map_err(|error| format!("host tools: {error:?}"))?;
    let host = WorkspaceHost::new(
        NAME,
        ROOT,
        files.clone(),
        CAPS.file_bytes,
        built.registered_tools().cloned(),
    );
    let router = Router::new(
        Arc::new(Wire {
            host: host.clone(),
            corrupt: Arc::clone(corrupt),
        }),
        hand.clone(),
        Arc::new(Still),
        Arc::new(Unplaced),
        Policy { idle_ms: IDLE_MS },
    );
    Ok(Stack {
        files,
        host,
        router,
    })
}

fn text(output: &ToolOutput) -> String {
    match &output.output {
        ToolOutputBody::Text(text) => text.clone(),
        body => serde_json::to_string(body).unwrap_or_default(),
    }
}

fn absolute(path: &str) -> String {
    format!("{ROOT}/{path}")
}

fn words(rng: &mut Rng, most: u64) -> String {
    (0..rng.range(0, most + 1))
        .map(|_| PIECES[rng.pick(PIECES.len())])
        .collect()
}

fn quoted(content: &str) -> String {
    format!("'{}'", content.replace('\'', "'\\''"))
}

fn shown(bytes: Option<&[u8]>) -> String {
    bytes.map_or_else(
        || "absent".to_owned(),
        |bytes| format!("{:?}", String::from_utf8_lossy(bytes)),
    )
}

async fn holds(files: &Workspace, path: &str) -> Result<Option<Vec<u8>>, String> {
    files
        .read(absolute(path).as_bytes())
        .await
        .map_err(|error| format!("read {path}: {error}"))
}

async fn agrees(
    files: &Workspace,
    model: &BTreeMap<String, Vec<u8>>,
    when: &str,
    digest: &mut u64,
) -> Result<(), String> {
    for path in FILES.iter().chain(COPIES.iter()) {
        let held = holds(files, path)
            .await
            .map_err(|error| format!("{when}: {error}"))?;
        let expected = model.get(*path);
        if held.as_ref() != expected {
            return Err(format!(
                "{when}: {path} holds {}, the model holds {}",
                shown(held.as_deref()),
                shown(expected.map(Vec::as_slice))
            ));
        }
        *digest = fnv(*digest, held.as_deref().unwrap_or(b"-"));
    }
    Ok(())
}

type Touched = (String, Option<Vec<u8>>, Option<Vec<u8>>);

fn settle(
    op: u64,
    fired: u8,
    (path, before, after): &Touched,
    live: Option<Vec<u8>>,
    stats: &mut WorkspaceStackStats,
) -> Result<Option<Vec<u8>>, String> {
    match fired {
        PLAN_BEFORE => {
            stats.crashes_before += 1;
            if live != *before {
                return Err(format!(
                    "op {op}: a commit of {path} died before the store took it, yet the workspace shows {} (before {})",
                    shown(live.as_deref()),
                    shown(before.as_deref())
                ));
            }
            Ok(live)
        }
        PLAN_AFTER => {
            stats.crashes_after += 1;
            if live != *before && live != *after {
                return Err(format!(
                    "op {op}: {path} shows {} after a crash, neither the old {} nor the new {}",
                    shown(live.as_deref()),
                    shown(before.as_deref()),
                    shown(after.as_deref())
                ));
            }
            Ok(live)
        }
        _ => Ok(after.clone()),
    }
}

fn simulate(seed: u64, mutation: WorkspaceStackMutation) -> WorkspaceStackOutcome {
    let mut rng = Rng::derive(seed, "workspace_stack");
    let ops = rng.range(OPS_MIN, OPS_MAX);
    let shape = format!("ops={ops}");
    let mut stats = WorkspaceStackStats {
        seeds: 1,
        ..WorkspaceStackStats::default()
    };
    let mut digest = FNV_OFFSET;
    let store = Arc::new(Crashing {
        inner: MemStore::new(),
        plan: AtomicU8::new(PLAN_NONE),
        fired: AtomicU8::new(PLAN_NONE),
    });
    let hand = Arc::new(Hand {
        files: Atom::new(BTreeMap::new()),
        corrupt: AtomicBool::new(false),
        cut: AtomicUsize::new(UNCUT),
    });
    let corrupt = Arc::new(AtomicBool::new(false));
    let (fs_rule, hand_rule, workspace_rule) = mutation.rules();
    let wrong = block_on(async {
        let mut stack = open(&store, &hand, &corrupt)?;
        stack
            .files
            .write(absolute(".keep").as_bytes(), b"")
            .await
            .map_err(|error| format!("setup: {error}"))?;
        fs_sim::mutate(fs_rule);
        hands_sim::mutate(hand_rule);
        workspace_sim::mutate(workspace_rule);
        let mut model: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut on_hand: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for op in 0..ops {
            let step = format!("step-{op}");
            let context = ToolContext::new("", SESSION, &step, &[], usize::MAX);
            let crash = match rng.pick(CRASH_IN) {
                0 => match rng.pick(2) {
                    0 => PLAN_BEFORE,
                    _ => PLAN_AFTER,
                },
                _ => PLAN_NONE,
            };
            let mut touched: Option<Touched> = None;
            let said = match rng.pick(KINDS) {
                0 => {
                    let path = FILES[rng.pick(FILES.len())];
                    let content = words(&mut rng, 6);
                    let input = json!({"file_path": absolute(path), "content": content});
                    store.plan.store(crash, Ordering::SeqCst);
                    let output = stack.host.call("Write", input, context).await;
                    stats.writes += 1;
                    touched = Some((
                        path.to_owned(),
                        model.get(path).cloned(),
                        Some(content.into_bytes()),
                    ));
                    format!("{}:{}", output.success, text(&output))
                }
                1 if !model.is_empty() => {
                    let paths: Vec<String> = model.keys().cloned().collect();
                    let path = paths[rng.pick(paths.len())].clone();
                    let old = PIECES[rng.pick(PIECES.len())];
                    let new = PIECES[rng.pick(PIECES.len())];
                    let before = String::from_utf8_lossy(&model[&path]).into_owned();
                    let unique = old != new && before.matches(old).count() == 1;
                    let input = json!({
                        "file_path": absolute(&path),
                        "old_string": old,
                        "new_string": new,
                    });
                    store.plan.store(crash, Ordering::SeqCst);
                    let output = stack.host.call("Edit", input, context).await;
                    stats.edits += 1;
                    if output.success && !unique {
                        return Err(format!(
                            "op {op}: Edit {old:?} -> {new:?} on {before:?} succeeded without exactly one match"
                        ));
                    }
                    let after = match unique {
                        true => before.replacen(old, new, 1),
                        false => before.clone(),
                    };
                    touched = Some((path, Some(before.into_bytes()), Some(after.into_bytes())));
                    format!("{}:{}", output.success, text(&output))
                }
                2 if !model.is_empty() => {
                    let paths: Vec<String> = model.keys().cloned().collect();
                    let path = &paths[rng.pick(paths.len())];
                    let output = stack
                        .host
                        .call("Read", json!({"file_path": absolute(path)}), context)
                        .await;
                    stats.reads += 1;
                    if !output.success {
                        return Err(format!("op {op}: Read {path} failed: {}", text(&output)));
                    }
                    text(&output)
                }
                3 => {
                    let mut lines = Vec::new();
                    for _ in 0..rng.range(1, 4) {
                        let path = SHELL_FILES[rng.pick(SHELL_FILES.len())];
                        let line = match rng.pick(3) {
                            0 => {
                                let content = words(&mut rng, 3);
                                model.insert(path.to_owned(), content.clone().into_bytes());
                                format!("printf '%s' {} > {}", quoted(&content), absolute(path))
                            }
                            1 => {
                                let content = words(&mut rng, 3);
                                model
                                    .entry(path.to_owned())
                                    .or_default()
                                    .extend(content.bytes());
                                format!("printf '%s' {} >> {}", quoted(&content), absolute(path))
                            }
                            _ => {
                                model.remove(path);
                                format!("rm -f {}", absolute(path))
                            }
                        };
                        lines.push(line);
                    }
                    let input = json!({"command": lines.join("\n")});
                    let output = match rng.pick(2) {
                        0 => stack.host.call("Bash", input, context).await,
                        _ => {
                            stats.routed += 1;
                            stack.router.route(NAME, "Bash", input, context).await
                        }
                    };
                    stats.scripts += 1;
                    if !output.success {
                        return Err(format!(
                            "op {op}: script {lines:?} failed: {}",
                            text(&output)
                        ));
                    }
                    text(&output)
                }
                4 | 5 if !model.is_empty() => {
                    let paths: Vec<String> = model.keys().cloned().collect();
                    let from = paths[rng.pick(paths.len())].clone();
                    let parked = format!("parked/{}", rng.pick(FILES.len()));
                    let chunk = NonZeroUsize::new(rng.range(1, CHUNK_MAX + 1) as usize)
                        .unwrap_or(NonZeroUsize::MIN);
                    let out_corrupt = rng.pick(CORRUPT_IN) == 0;
                    corrupt.store(out_corrupt, Ordering::SeqCst);
                    let out = stack
                        .router
                        .copy(
                            SESSION,
                            (&Target::Workspace, &from),
                            (&Target::Hand(HandRef::Hosted), &parked),
                            chunk,
                        )
                        .await;
                    corrupt.store(false, Ordering::SeqCst);
                    stats.copies += 1;
                    match (out_corrupt, &out) {
                        (false, Ok(stored))
                            if stored.sha256 == Sha256Digest::of(&model[&from]) =>
                        {
                            on_hand.insert(parked.clone(), model[&from].clone());
                        }
                        (true, Err(CopyError::Put(PutError::ShaMismatch { .. }))) => {
                            stats.copies_corrupt += 1;
                        }
                        _ => {
                            return Err(format!(
                                "op {op}: copy {from} -> hand {parked} corrupt={out_corrupt} gave {out:?}"
                            ));
                        }
                    }
                    let held = hand.files.read(|files| files.get(&parked).cloned());
                    if held.as_ref() != on_hand.get(&parked) {
                        return Err(format!(
                            "op {op}: the Hand's {parked} holds {}, it was sent {}",
                            shown(held.as_deref()),
                            shown(on_hand.get(&parked).map(Vec::as_slice))
                        ));
                    }
                    match on_hand.get(&parked).cloned() {
                        Some(sent) => {
                            let to = COPIES[rng.pick(COPIES.len())];
                            let back_corrupt = rng.pick(CORRUPT_IN) == 0;
                            let pieces = sent.len().div_ceil(chunk.get());
                            let back_cut = (!back_corrupt && pieces > 0 && rng.pick(CUT_IN) == 0)
                                .then(|| rng.pick(pieces));
                            hand.corrupt.store(back_corrupt, Ordering::SeqCst);
                            hand.cut.store(back_cut.unwrap_or(UNCUT), Ordering::SeqCst);
                            store.plan.store(crash, Ordering::SeqCst);
                            let back = stack
                                .router
                                .copy(
                                    SESSION,
                                    (&Target::Hand(HandRef::Hosted), &parked),
                                    (&Target::Workspace, to),
                                    chunk,
                                )
                                .await;
                            hand.corrupt.store(false, Ordering::SeqCst);
                            hand.cut.store(UNCUT, Ordering::SeqCst);
                            stats.copies += 1;
                            let before = model.get(to).cloned();
                            let after = match (back_corrupt, back_cut) {
                                (false, Some(after)) => {
                                    stats.copies_cut += 1;
                                    let lost = matches!(
                                        back,
                                        Err(CopyError::Put(PutError::Source(ChunkError::Lost { .. })))
                                    );
                                    let crashed = store.fired.load(Ordering::SeqCst) != PLAN_NONE;
                                    if !lost && !(crashed && back.is_err()) {
                                        return Err(format!(
                                            "op {op}: hand {parked} -> {to} died after {after} of {pieces} chunks, yet the copy gave {back:?}"
                                        ));
                                    }
                                    before.clone()
                                }
                                (true, _) => {
                                    stats.copies_corrupt += 1;
                                    if back.is_ok() {
                                        return Err(format!(
                                            "op {op}: a corrupted copy of hand {parked} -> {to} was stored: {back:?}"
                                        ));
                                    }
                                    before.clone()
                                }
                                (false, None) => Some(sent),
                            };
                            touched = Some((to.to_owned(), before, after));
                            format!("{out:?} {back:?}")
                        }
                        None => format!("{out:?}"),
                    }
                }
                _ => {
                    stats.restarts += 1;
                    drop(stack);
                    stack = open(&store, &hand, &corrupt)?;
                    "restarted".to_owned()
                }
            };
            store.plan.store(PLAN_NONE, Ordering::SeqCst);
            let fired = store.fired.swap(PLAN_NONE, Ordering::SeqCst);
            digest = fnv(digest, said.as_bytes());
            if let Some(touched) = touched {
                let live = holds(&stack.files, &touched.0)
                    .await
                    .map_err(|error| format!("op {op}: {error}"))?;
                match settle(op, fired, &touched, live, &mut stats)? {
                    Some(bytes) => model.insert(touched.0, bytes),
                    None => model.remove(&touched.0),
                };
                if fired != PLAN_NONE {
                    stats.restarts += 1;
                    drop(stack);
                    stack = open(&store, &hand, &corrupt)?;
                    let when = format!("op {op}: after the restart");
                    agrees(&stack.files, &model, &when, &mut digest).await?;
                }
            }
            agrees(&stack.files, &model, &format!("op {op}"), &mut digest).await?;
        }
        stats.restarts += 1;
        drop(stack);
        let stack = open(&store, &hand, &corrupt)?;
        agrees(&stack.files, &model, "at the last restart", &mut digest).await
    })
    .err();
    fs_sim::mutate(FsRule::None);
    hands_sim::mutate(HandRule::None);
    workspace_sim::mutate(WorkspaceRule::None);
    WorkspaceStackOutcome {
        digest,
        wrong,
        stats,
        shape,
    }
}

fn run_seed(seed: u64, mutation: WorkspaceStackMutation) -> WorkspaceStackOutcome {
    std::panic::catch_unwind(|| simulate(seed, mutation)).unwrap_or_else(|panic| {
        fs_sim::mutate(FsRule::None);
        hands_sim::mutate(HandRule::None);
        workspace_sim::mutate(WorkspaceRule::None);
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default();
        WorkspaceStackOutcome {
            digest: 0,
            wrong: Some(format!("panic: {message}")),
            stats: WorkspaceStackStats::default(),
            shape: String::new(),
        }
    })
}

pub fn judge(
    seed: u64,
    mutation: WorkspaceStackMutation,
    twice: bool,
    verbose: bool,
    totals: &mut WorkspaceStackStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!(
            "seed {seed} digest {:016x} {} stats={:?}",
            first.digest, first.shape, first.stats
        );
    }
    if first.wrong.is_some() || !twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let second = run_seed(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: WorkspaceStackMutation) -> String {
    let mut command = format!("nanocode-sim --scenario workspace_stack --seed {seed}");
    if mutation != WorkspaceStackMutation::None {
        command.push_str(" --mutation ");
        command.push_str(mutation.name());
    }
    command
}

pub fn report(
    seeds: &Range<u64>,
    passed: u64,
    failed: u64,
    twice: bool,
    mutation: WorkspaceStackMutation,
    totals: &WorkspaceStackStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=workspace_stack mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        WorkspaceStackMutation::None if failed == 0 => ExitCode::SUCCESS,
        WorkspaceStackMutation::None => ExitCode::FAILURE,
        _ if failed > 0 => {
            println!("MUTATION {} caught by {failed} seeds", mutation.name());
            ExitCode::SUCCESS
        }
        _ => {
            println!("MUTATION {} NOT caught", mutation.name());
            ExitCode::FAILURE
        }
    }
}
