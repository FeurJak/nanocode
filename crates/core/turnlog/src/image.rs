use std::{
    collections::BTreeMap,
    io,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use nanocode_compression::checksum::crc32;
use nanocode_durability_core::StoreError;

use crate::{
    codec::SNAPSHOT,
    host::{Compaction, Extension},
};

const WORD: usize = size_of::<u64>();
const MAGIC: [u8; WORD] = *b"nctlog01";
const FIELDS: usize = 7;
const SLOT: usize = (1 + FIELDS + 1) * WORD;
const ENTRY: usize = 2 * WORD;
const ROW: u8 = 0;
const MOVE: u8 = 1;
const DROP: u8 = 2;
const ROW_HEAD: usize = 1 + 2 * WORD;
const MOVE_HEAD: usize = 1 + 6 * WORD;
const DROP_HEAD: usize = 1 + 2 * WORD;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FileIo {
    pub read_bytes: u64,
    pub reads: u64,
    pub writes: u64,
    pub written_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub end: u64,
    pub generation: u64,
    pub indexed: u64,
    pub next: u64,
    pub snapshot: Option<u64>,
    pub tail: u64,
}

pub struct Image<M> {
    counters: Arc<Counters>,
    data: M,
    failed: Option<StoreError>,
    header: Header,
    index: M,
    overlay: Option<Overlay>,
    sync: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ImageOptions {
    pub sync: bool,
}

pub trait Medium {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<usize>;

    fn sync(&mut self) -> io::Result<()>;

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()>;
}

#[derive(Default)]
pub(crate) struct Counters {
    read_bytes: AtomicU64,
    reads: AtomicU64,
    writes: AtomicU64,
    written_bytes: AtomicU64,
}

#[derive(Clone, Copy)]
enum Kind {
    Drop {
        end: u64,
        start: u64,
    },
    Move {
        cut: u64,
        from: u64,
        len: u64,
        prev: u64,
        seq: u64,
    },
    Row {
        seq: u64,
    },
}

struct Overlay {
    entries: BTreeMap<u64, (u64, u64)>,
}

struct Parsed<'a> {
    kind: Kind,
    len: u64,
    offset: u64,
    payload: &'a [u8],
}

enum Record<'a> {
    Drop(Range<u64>),
    Move {
        cut: u64,
        from: u64,
        len: u64,
        prev: u64,
        seq: u64,
        tail: &'a [u8],
    },
    Row {
        body: &'a [u8],
        seq: u64,
    },
}

impl Counters {
    pub(crate) fn snapshot(&self) -> FileIo {
        FileIo {
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            reads: self.reads.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            written_bytes: self.written_bytes.load(Ordering::Relaxed),
        }
    }
}

impl Header {
    pub const BYTES: usize = 2 * SLOT;
    pub const SLOT: usize = SLOT;

    pub const fn empty() -> Self {
        let start = Self::BYTES as u64;
        Self {
            end: start,
            generation: 0,
            indexed: start,
            next: 0,
            snapshot: None,
            tail: start,
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, StoreError> {
        let first = bytes.get(..SLOT);
        let slots =
            [first, bytes.get(SLOT..Self::BYTES)].map(|slot| slot.and_then(Self::decode_slot));
        match slots {
            [Some(first), Some(second)] => Ok(if first.generation > second.generation {
                first
            } else {
                second
            }),
            [Some(header), None] | [None, Some(header)] => Ok(header),
            [None, None] if first.is_none_or(|slot| slot.iter().all(|byte| *byte == 0)) => {
                Ok(Self::empty())
            }
            [None, None] => Err(StoreError::Backend(
                "turn log file has no valid header slot".to_owned(),
            )),
        }
    }

    pub fn decode_slot(slot: &[u8]) -> Option<Self> {
        let body = slot.get(..SLOT - WORD)?;
        if body.get(..WORD)? != MAGIC || u64::from(crc32(0, body)) != word(slot, SLOT - WORD) {
            return None;
        }
        let snapshot = match word(slot, 6 * WORD) {
            0 => None,
            1 => Some(word(slot, 7 * WORD)),
            _ => return None,
        };
        Some(Self {
            end: word(slot, 2 * WORD),
            generation: word(slot, WORD),
            indexed: word(slot, 3 * WORD),
            next: word(slot, 4 * WORD),
            snapshot,
            tail: word(slot, 5 * WORD),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(SLOT);
        bytes.extend_from_slice(&MAGIC);
        let fields: [u64; FIELDS] = [
            self.generation,
            self.end,
            self.indexed,
            self.next,
            self.tail,
            u64::from(self.snapshot.is_some()),
            self.snapshot.unwrap_or_default(),
        ];
        for value in fields {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let checksum = u64::from(crc32(0, &bytes));
        bytes.extend_from_slice(&checksum.to_le_bytes());
        bytes
    }

    pub const fn offset(&self) -> u64 {
        (self.generation % 2) * SLOT as u64
    }
}

impl<M: Medium> Image<M> {
    pub fn open(data: M, index: M, options: ImageOptions) -> Result<Self, StoreError> {
        Self::counted(data, index, options, Arc::default())
    }

    pub(crate) fn counted(
        data: M,
        index: M,
        options: ImageOptions,
        counters: Arc<Counters>,
    ) -> Result<Self, StoreError> {
        let bytes = read(&counters, &data, 0, Header::BYTES)?;
        Ok(Self {
            header: Header::decode(&bytes)?,
            counters,
            data,
            failed: None,
            index,
            overlay: None,
            sync: options.sync,
        })
    }

    pub const fn data(&self) -> &M {
        &self.data
    }

    pub const fn header(&self) -> Header {
        self.header
    }

    pub const fn index(&self) -> &M {
        &self.index
    }

    pub fn io(&self) -> FileIo {
        self.counters.snapshot()
    }

    pub fn load(&mut self) -> Result<Vec<(u64, String)>, StoreError> {
        self.usable()?;
        let header = self.header;
        let region = self.region(header.tail)?;
        let records = records(&region, header.tail)?;
        if self.overlay.is_none() && header.tail <= header.indexed {
            self.overlay = Some(overlay(&records, header.indexed));
        }
        let rows = self.apply(&records)?;
        let derived = newest(&rows);
        if derived != header.snapshot {
            return Err(StoreError::Backend(format!(
                "turn log file header names snapshot row {:?} but its tail holds {derived:?}",
                header.snapshot
            )));
        }
        finish(rows, derived)
    }

    pub fn replay(&self) -> Result<Vec<(u64, String)>, StoreError> {
        self.usable()?;
        let region = self.region(Header::BYTES as u64)?;
        let rows = self.apply(&records(&region, Header::BYTES as u64)?)?;
        let derived = newest(&rows);
        finish(rows, derived)
    }

    pub fn scan(&mut self, from: u64, to: u64) -> Result<Vec<(u64, String)>, StoreError> {
        self.usable()?;
        let to = to.min(self.header.next);
        if from >= to {
            return Ok(Vec::new());
        }
        let live: Vec<(u64, u64, u64)> = self
            .entries(from..to)?
            .into_iter()
            .zip(from..)
            .filter(|((offset, _), _)| *offset != 0)
            .map(|((offset, len), seq)| (seq, offset, len))
            .collect();
        let mut rows = Vec::with_capacity(live.len());
        let mut at = 0;
        while let Some(&(_, start, _)) = live.get(at) {
            let mut stop = at + 1;
            while let (Some((_, offset, len)), Some((_, next, _))) =
                (live.get(stop - 1), live.get(stop))
                && offset + len == *next
            {
                stop += 1;
            }
            let end = live
                .get(stop - 1)
                .map_or(start, |(_, offset, len)| offset + len);
            let buffer = self.read_data(start, span(start, end)?)?;
            for &(seq, offset, len) in &live[at..stop] {
                let bytes = usize::try_from(offset - start)
                    .ok()
                    .and_then(|from| {
                        buffer.get(from..from.checked_add(usize::try_from(len).ok()?)?)
                    })
                    .ok_or_else(|| corrupt(offset))?;
                let parsed = parse(bytes, offset).ok_or_else(|| corrupt(offset))?;
                let (stored, body) = match parsed.kind {
                    Kind::Row { seq } => (seq, parsed.payload.to_vec()),
                    Kind::Move { .. } => self.resolve(offset)?,
                    Kind::Drop { .. } => return Err(corrupt(offset)),
                };
                if stored != seq {
                    return Err(corrupt(offset));
                }
                rows.push((seq, text(body)?));
            }
            at = stop;
        }
        Ok(rows)
    }

    pub fn append(
        &mut self,
        rows: &[(u64, String)],
        compaction: &Compaction<'_>,
    ) -> Result<(), StoreError> {
        self.usable()?;
        let header = self.header;
        if let (Some(first), Some(last)) = (
            rows.iter().map(|(seq, _)| *seq).min(),
            rows.iter().map(|(seq, _)| *seq).max(),
        ) {
            let entries = self.entries(first..last + 1)?;
            let taken = rows.iter().any(|(seq, _)| {
                usize::try_from(seq - first)
                    .ok()
                    .and_then(|at| entries.get(at))
                    .is_some_and(|(offset, _)| *offset != 0)
            });
            if taken {
                return Err(StoreError::Fenced);
            }
        }
        let written = || compaction.archived.iter().chain(rows);
        let newest = written()
            .filter(|(_, body)| body.starts_with(SNAPSHOT))
            .map(|(seq, _)| *seq)
            .max()
            .filter(|seq| header.snapshot.is_none_or(|snapshot| *seq >= snapshot));
        if newest.is_none()
            && let Some(snapshot) = header.snapshot
        {
            let replaced =
                written().any(|(seq, body)| *seq == snapshot && !body.starts_with(SNAPSHOT));
            let dropped = std::iter::once(&compaction.dropped)
                .chain(compaction.superseded)
                .any(|range| range.contains(&snapshot));
            if replaced || dropped {
                return Err(StoreError::Backend(format!(
                    "turn log compaction removes snapshot row {snapshot} and writes no newer one"
                )));
            }
        }
        let records: Vec<Record<'_>> = written()
            .map(|(seq, body)| Record::Row {
                body: body.as_bytes(),
                seq: *seq,
            })
            .chain(
                std::iter::once(&compaction.dropped)
                    .chain(compaction.superseded)
                    .filter(|range| !range.is_empty())
                    .map(|range| Record::Drop(range.clone())),
            )
            .collect();
        self.commit(&records, newest.or(header.snapshot), newest)
    }

    pub fn extend(&mut self, extension: &Extension<'_>) -> Result<(), StoreError> {
        self.usable()?;
        let header = self.header;
        let [(taken, _)] = self.entries(extension.seq..extension.seq + 1)?[..] else {
            return Err(corrupt(0));
        };
        let [(prev, prev_len)] = self.entries(extension.row..extension.row + 1)?[..] else {
            return Err(corrupt(0));
        };
        if taken != 0 || prev == 0 {
            return Err(StoreError::Fenced);
        }
        let bytes = self.read_data(prev, span(prev, prev + prev_len)?.min(MOVE_HEAD))?;
        let len = match parse_head(&bytes) {
            Some((Kind::Row { .. }, _, payload)) => payload,
            Some((Kind::Move { len, .. }, _, _)) => len,
            _ => return Err(corrupt(prev)),
        };
        let cut = extension.cut as u64;
        if cut > len {
            return Err(StoreError::Backend(format!(
                "turn log row {} is shorter than its {}-byte seal",
                extension.row, extension.cut
            )));
        }
        let moved = extension.seq > header.snapshot.unwrap_or_default()
            && self.first(prev)? == Some(SNAPSHOT as u8);
        let snapshot = if header.snapshot == Some(extension.row) || moved {
            Some(extension.seq)
        } else {
            header.snapshot
        };
        let record = Record::Move {
            cut,
            from: extension.row,
            len: len - cut + extension.tail.len() as u64,
            prev,
            seq: extension.seq,
            tail: extension.tail.as_bytes(),
        };
        self.commit(&[record], snapshot, None)
    }

    fn usable(&self) -> Result<(), StoreError> {
        self.failed.clone().map_or(Ok(()), Err)
    }

    fn read_data(&self, offset: u64, len: usize) -> Result<Vec<u8>, StoreError> {
        let bytes = read(&self.counters, &self.data, offset, len)?;
        if bytes.len() == len {
            Ok(bytes)
        } else {
            Err(StoreError::Backend(format!(
                "turn log file ends inside the {len} bytes at {offset}"
            )))
        }
    }

    fn region(&self, start: u64) -> Result<Vec<u8>, StoreError> {
        self.read_data(start, span(start, self.header.end)?)
    }

    fn overlaid(&mut self) -> Result<(), StoreError> {
        if self.overlay.is_none() {
            let indexed = self.header.indexed;
            let region = self.region(indexed)?;
            self.overlay = Some(overlay(&records(&region, indexed)?, indexed));
        }
        Ok(())
    }

    fn entries(&mut self, seqs: Range<u64>) -> Result<Vec<(u64, u64)>, StoreError> {
        self.overlaid()?;
        let count =
            usize::try_from(seqs.end - seqs.start).map_err(|error| failed("index", error))?;
        let start = to_offset(seqs.start)?;
        let bytes = read(&self.counters, &self.index, start, count * ENTRY)?;
        let mut entries: Vec<(u64, u64)> = (0..count)
            .map(|at| (word(&bytes, at * ENTRY), word(&bytes, at * ENTRY + WORD)))
            .collect();
        if let Some(overlay) = &self.overlay {
            for (seq, entry) in overlay.entries.range(seqs.clone()) {
                if let Some(slot) = usize::try_from(seq - seqs.start)
                    .ok()
                    .and_then(|at| entries.get_mut(at))
                {
                    *slot = *entry;
                }
            }
        }
        Ok(entries)
    }

    fn first(&self, offset: u64) -> Result<Option<u8>, StoreError> {
        let mut at = offset;
        loop {
            let head = self.read_data(at, span(at, self.header.end)?.min(MOVE_HEAD))?;
            match parse_head(&head).ok_or_else(|| corrupt(at))? {
                (Kind::Row { .. }, _, 0) => return Ok(None),
                (Kind::Row { .. }, used, _) => {
                    return Ok(self.read_data(at + used as u64, 1)?.first().copied());
                }
                (Kind::Move { prev, .. }, _, _) if prev < at => at = prev,
                _ => return Err(corrupt(at)),
            }
        }
    }

    fn resolve(&self, offset: u64) -> Result<(u64, Vec<u8>), StoreError> {
        let mut tails: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut seq = None;
        let mut at = offset;
        loop {
            let head = self.read_data(at, span(at, self.header.end)?.min(MOVE_HEAD))?;
            let (kind, used, payload) = parse_head(&head).ok_or_else(|| corrupt(at))?;
            let payload = self.read_data(at + used as u64, to_len(payload)?)?;
            match kind {
                Kind::Row { seq: stored } => {
                    let mut body = payload;
                    for (cut, tail) in tails.into_iter().rev() {
                        splice(&mut body, cut, &tail, at)?;
                    }
                    return Ok((seq.unwrap_or(stored), body));
                }
                Kind::Move {
                    cut,
                    prev,
                    seq: stored,
                    ..
                } if prev < at => {
                    tails.push((cut, payload));
                    seq.get_or_insert(stored);
                    at = prev;
                }
                _ => return Err(corrupt(at)),
            }
        }
    }

    fn apply(&self, records: &[Parsed<'_>]) -> Result<BTreeMap<u64, Vec<u8>>, StoreError> {
        let mut rows = BTreeMap::new();
        for record in records {
            match record.kind {
                Kind::Row { seq } => {
                    rows.insert(seq, record.payload.to_vec());
                }
                Kind::Move {
                    cut,
                    from,
                    prev,
                    seq,
                    ..
                } => {
                    let mut body = match rows.remove(&from) {
                        Some(body) => body,
                        None => self.resolve(prev)?.1,
                    };
                    splice(&mut body, cut, record.payload, record.offset)?;
                    rows.insert(seq, body);
                }
                Kind::Drop { end, start } => {
                    let doomed: Vec<u64> = rows.range(start..end).map(|(seq, _)| *seq).collect();
                    for seq in doomed {
                        rows.remove(&seq);
                    }
                }
            }
        }
        Ok(rows)
    }

    fn commit(
        &mut self,
        records: &[Record<'_>],
        snapshot: Option<u64>,
        tail: Option<u64>,
    ) -> Result<(), StoreError> {
        let result = self.committing(records, snapshot, tail);
        if let Err(error) = &result {
            self.failed = Some(error.clone());
        }
        result
    }

    fn committing(
        &mut self,
        records: &[Record<'_>],
        snapshot: Option<u64>,
        tail: Option<u64>,
    ) -> Result<(), StoreError> {
        let header = self.header;
        let mut buffer = Vec::new();
        let mut entries = BTreeMap::new();
        let mut next = header.next;
        let mut start = None;
        for record in records {
            let offset = header.end + buffer.len() as u64;
            encode(record, &mut buffer);
            let len = header.end + buffer.len() as u64 - offset;
            match record {
                Record::Row { seq, .. } | Record::Move { seq, .. } => {
                    next = next.max(seq + 1);
                    if tail.is_some_and(|tail| *seq >= tail) {
                        start = Some(start.map_or(offset, |start: u64| start.min(offset)));
                    }
                }
                Record::Drop(_) => {}
            }
            index(record, offset, len, &mut entries);
        }
        let tail = match tail {
            Some(seq) => {
                let stored = self
                    .entries(seq..header.next.max(seq))?
                    .into_iter()
                    .zip(seq..)
                    .filter(|((offset, _), at)| *offset != 0 && !entries.contains_key(at))
                    .map(|((offset, _), _)| offset)
                    .min();
                [start, stored].into_iter().flatten().min().ok_or_else(|| {
                    StoreError::Backend(format!("turn log snapshot row {seq} has no record"))
                })?
            }
            None => header.tail,
        };
        self.overlaid()?;
        write(&self.counters, &mut self.data, header.end, &buffer)?;
        if let Some(overlay) = &self.overlay {
            write_entries(&self.counters, &mut self.index, &overlay.entries)?;
        }
        if self.sync {
            self.data.sync().map_err(|error| failed("sync", error))?;
            self.index.sync().map_err(|error| failed("sync", error))?;
        }
        let committed = Header {
            end: header.end + buffer.len() as u64,
            generation: header.generation + 1,
            indexed: header.end,
            next,
            snapshot,
            tail,
        };
        write(
            &self.counters,
            &mut self.data,
            committed.offset(),
            &committed.encode(),
        )?;
        if self.sync {
            self.data.sync().map_err(|error| failed("sync", error))?;
        }
        self.header = committed;
        write_entries(&self.counters, &mut self.index, &entries)?;
        self.overlay = Some(Overlay { entries });
        if self.sync {
            self.index.sync().map_err(|error| failed("sync", error))?;
        }
        let indexed = Header {
            generation: committed.generation + 1,
            indexed: committed.end,
            ..committed
        };
        write(
            &self.counters,
            &mut self.data,
            indexed.offset(),
            &indexed.encode(),
        )?;
        self.header = indexed;
        self.overlay = Some(Overlay {
            entries: BTreeMap::new(),
        });
        Ok(())
    }
}

impl Medium for Vec<u8> {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
        let start = usize::try_from(offset).map_or(self.len(), |start| start.min(self.len()));
        let available = &self[start..];
        let len = available.len().min(buffer.len());
        buffer[..len].copy_from_slice(&available[..len]);
        Ok(len)
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        let end = start + bytes.len();
        if self.len() < end {
            self.resize(end, 0);
        }
        self[start..end].copy_from_slice(bytes);
        Ok(())
    }
}

fn read<M: Medium>(
    counters: &Counters,
    medium: &M,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, StoreError> {
    let mut buffer = vec![0; len];
    let read = medium
        .read_at(offset, &mut buffer)
        .map_err(|error| failed("read", error))?;
    counters.reads.fetch_add(1, Ordering::Relaxed);
    counters
        .read_bytes
        .fetch_add(read as u64, Ordering::Relaxed);
    buffer.truncate(read);
    Ok(buffer)
}

fn write<M: Medium>(
    counters: &Counters,
    medium: &mut M,
    offset: u64,
    bytes: &[u8],
) -> Result<(), StoreError> {
    medium
        .write_at(offset, bytes)
        .map_err(|error| failed("write", error))?;
    counters.writes.fetch_add(1, Ordering::Relaxed);
    counters
        .written_bytes
        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
    Ok(())
}

fn write_entries<M: Medium>(
    counters: &Counters,
    medium: &mut M,
    entries: &BTreeMap<u64, (u64, u64)>,
) -> Result<(), StoreError> {
    let mut run: Option<(u64, Vec<u8>)> = None;
    for (seq, (offset, len)) in entries {
        if let Some((start, bytes)) = &run
            && *start + (bytes.len() / ENTRY) as u64 != *seq
        {
            write(counters, medium, to_offset(*start)?, bytes)?;
            run = None;
        }
        let (_, bytes) = run.get_or_insert_with(|| (*seq, Vec::new()));
        bytes.extend_from_slice(&offset.to_le_bytes());
        bytes.extend_from_slice(&len.to_le_bytes());
    }
    match run {
        Some((start, bytes)) => write(counters, medium, to_offset(start)?, &bytes),
        None => Ok(()),
    }
}

fn index(record: &Record<'_>, offset: u64, len: u64, entries: &mut BTreeMap<u64, (u64, u64)>) {
    match record {
        Record::Row { seq, .. } => {
            entries.insert(*seq, (offset, len));
        }
        Record::Move { from, seq, .. } => {
            entries.insert(*from, (0, 0));
            entries.insert(*seq, (offset, len));
        }
        Record::Drop(range) => {
            for seq in range.clone() {
                entries.insert(seq, (0, 0));
            }
        }
    }
}

fn overlay(records: &[Parsed<'_>], indexed: u64) -> Overlay {
    let mut entries = BTreeMap::new();
    for record in records.iter().filter(|record| record.offset >= indexed) {
        let shaped = match record.kind {
            Kind::Row { seq } => Record::Row { body: &[], seq },
            Kind::Move {
                cut,
                from,
                len,
                prev,
                seq,
            } => Record::Move {
                cut,
                from,
                len,
                prev,
                seq,
                tail: &[],
            },
            Kind::Drop { end, start } => Record::Drop(start..end),
        };
        index(&shaped, record.offset, record.len, &mut entries);
    }
    Overlay { entries }
}

fn encode(record: &Record<'_>, buffer: &mut Vec<u8>) {
    match record {
        Record::Row { body, seq } => {
            buffer.push(ROW);
            for value in [*seq, body.len() as u64] {
                buffer.extend_from_slice(&value.to_le_bytes());
            }
            buffer.extend_from_slice(body);
        }
        Record::Move {
            cut,
            from,
            len,
            prev,
            seq,
            tail,
        } => {
            buffer.push(MOVE);
            for value in [*seq, *from, *prev, *cut, *len, tail.len() as u64] {
                buffer.extend_from_slice(&value.to_le_bytes());
            }
            buffer.extend_from_slice(tail);
        }
        Record::Drop(range) => {
            buffer.push(DROP);
            for value in [range.start, range.end] {
                buffer.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
}

fn parse_head(bytes: &[u8]) -> Option<(Kind, usize, u64)> {
    match *bytes.first()? {
        ROW => {
            let head = bytes.get(..ROW_HEAD)?;
            Some((
                Kind::Row { seq: word(head, 1) },
                ROW_HEAD,
                word(head, 1 + WORD),
            ))
        }
        MOVE => {
            let head = bytes.get(..MOVE_HEAD)?;
            Some((
                Kind::Move {
                    cut: word(head, 1 + 3 * WORD),
                    from: word(head, 1 + WORD),
                    len: word(head, 1 + 4 * WORD),
                    prev: word(head, 1 + 2 * WORD),
                    seq: word(head, 1),
                },
                MOVE_HEAD,
                word(head, 1 + 5 * WORD),
            ))
        }
        DROP => {
            let head = bytes.get(..DROP_HEAD)?;
            Some((
                Kind::Drop {
                    end: word(head, 1 + WORD),
                    start: word(head, 1),
                },
                DROP_HEAD,
                0,
            ))
        }
        _ => None,
    }
}

fn parse(bytes: &[u8], offset: u64) -> Option<Parsed<'_>> {
    let (kind, head, payload) = parse_head(bytes)?;
    let end = head.checked_add(usize::try_from(payload).ok()?)?;
    Some(Parsed {
        kind,
        len: end as u64,
        offset,
        payload: bytes.get(head..end)?,
    })
}

fn records(region: &[u8], base: u64) -> Result<Vec<Parsed<'_>>, StoreError> {
    let mut parsed = Vec::new();
    let mut at = 0;
    while let Some(rest) = region.get(at..).filter(|rest| !rest.is_empty()) {
        let offset = base + at as u64;
        let record = parse(rest, offset).ok_or_else(|| corrupt(offset))?;
        at += to_len(record.len)?;
        parsed.push(record);
    }
    Ok(parsed)
}

fn newest(rows: &BTreeMap<u64, Vec<u8>>) -> Option<u64> {
    rows.iter()
        .rev()
        .find(|(_, body)| body.first() == Some(&(SNAPSHOT as u8)))
        .map(|(seq, _)| *seq)
}

fn finish(
    mut rows: BTreeMap<u64, Vec<u8>>,
    snapshot: Option<u64>,
) -> Result<Vec<(u64, String)>, StoreError> {
    rows.split_off(&snapshot.unwrap_or(0))
        .into_iter()
        .map(|(seq, body)| Ok((seq, text(body)?)))
        .collect()
}

fn splice(body: &mut Vec<u8>, cut: u64, tail: &[u8], offset: u64) -> Result<(), StoreError> {
    let kept = body
        .len()
        .checked_sub(to_len(cut)?)
        .ok_or_else(|| corrupt(offset))?;
    body.truncate(kept);
    body.extend_from_slice(tail);
    Ok(())
}

fn word(bytes: &[u8], at: usize) -> u64 {
    bytes
        .get(at..at + WORD)
        .and_then(|bytes| bytes.try_into().ok())
        .map_or(0, u64::from_le_bytes)
}

fn span(start: u64, end: u64) -> Result<usize, StoreError> {
    end.checked_sub(start)
        .and_then(|len| usize::try_from(len).ok())
        .ok_or_else(|| corrupt(start))
}

fn to_len(value: u64) -> Result<usize, StoreError> {
    usize::try_from(value).map_err(|error| failed("length", error))
}

fn to_offset(seq: u64) -> Result<u64, StoreError> {
    seq.checked_mul(ENTRY as u64)
        .ok_or_else(|| StoreError::Backend(format!("turn log row {seq} has no index offset")))
}

fn text(body: Vec<u8>) -> Result<String, StoreError> {
    String::from_utf8(body).map_err(|error| failed("decode", error))
}

#[cold]
fn corrupt(offset: u64) -> StoreError {
    StoreError::Backend(format!("turn log file record at {offset} does not decode"))
}

#[cold]
pub(crate) fn failed(action: &str, error: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(format!("turn log file {action} failed: {error}"))
}
