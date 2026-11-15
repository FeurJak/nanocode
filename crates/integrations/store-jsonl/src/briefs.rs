use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::HashMap,
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use nanocode_durability_core::{BriefFilter, BriefState};
use nanocode_platform::TemporaryFile;
use serde::Deserialize;
use serde_json::value::RawValue;

use super::{
    archive, identity,
    listing::{self, Line, Stamp},
    sessions::{created, file_name, key_of},
    summaries::{self, Change, Journaled, Raw, created_at_ms},
};

pub(super) const FILE: &str = ".index-v1";
const LISTING: &str = ".listing";
const MAGIC: &[u8; 8] = b"ncindex1";
const LENGTH_BYTES: usize = 4;
const UUID_DASHES: [usize; 4] = [8, 13, 18, 23];
const UUID_DIGITS: [usize; 16] = [0, 2, 4, 6, 9, 11, 14, 16, 19, 21, 24, 26, 28, 30, 32, 34];
const HEX: &[u8; 16] = b"0123456789abcdef";

const KEY_UUID: u64 = 1;
const PLAIN_SEEN: u64 = 1 << 1;
const PLAIN_PRESENT: u64 = 1 << 2;
const ARCHIVED_SEEN: u64 = 1 << 3;
const ARCHIVED_PRESENT: u64 = 1 << 4;
const SUMMARY: u64 = 1 << 5;
const FILE_SOURCE: u64 = 1 << 6;
const LISTED: u64 = 1 << 7;
const UNREADABLE: u64 = 1 << 8;
const PREVIEW: u64 = 1 << 9;

#[derive(Clone, Hash, PartialEq, Eq)]
enum Key<'a> {
    Text(Cow<'a, str>),
    Uuid([u8; 16]),
}

#[derive(Clone, Default)]
struct Name<'a> {
    entry: Option<Entry<'a>>,
    position: Option<u64>,
}

#[derive(Clone)]
struct Entry<'a> {
    created: u128,
    length: u64,
    source: Cow<'a, str>,
}

#[derive(Clone)]
struct Known<'a> {
    brief: Brief<'a>,
    file: Cow<'a, str>,
    length: u64,
    revision: u64,
}

#[derive(Clone)]
enum Brief<'a> {
    Listed {
        preview: Option<Cow<'a, str>>,
        scope: Cow<'a, str>,
        updated_at_ms: u64,
    },
    Unlisted,
    Unreadable,
}

#[derive(Clone)]
struct Row<'a> {
    key: Key<'a>,
    names: [Name<'a>; 2],
    summary: Option<Known<'a>>,
}

struct Header<'a> {
    listing: Cow<'a, str>,
    listing_base: u64,
    listing_end: u64,
    next_position: u64,
    stamp: Stamp,
    summaries: Cow<'a, str>,
    summaries_end: u64,
}

pub(super) struct Index<'a> {
    changed: Vec<usize>,
    header: Header<'a>,
    keys: Option<HashMap<Key<'a>, usize>>,
    rows: Vec<Row<'a>>,
}

pub(super) struct Decoded<'a> {
    pub(super) base: usize,
    pub(super) end: usize,
    pub(super) index: Index<'a>,
}

#[derive(Deserialize)]
struct Head<'a> {
    #[serde(rename = "v", default, borrow)]
    preview: Option<Cow<'a, str>>,
    #[serde(rename = "s", default, borrow)]
    scope: Option<Cow<'a, str>>,
    #[serde(rename = "u")]
    updated_at_ms: u64,
    #[serde(rename = "r", borrow)]
    value: &'a RawValue,
}

#[derive(Deserialize)]
struct Record<'a> {
    #[serde(borrow)]
    lineage: Lineage<'a>,
    #[serde(borrow)]
    session_id: Cow<'a, str>,
    #[serde(default)]
    updated_at_ms: u64,
    #[serde(default, borrow)]
    workspace: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct Lineage<'a> {
    #[serde(borrow)]
    origin: Cow<'a, str>,
}

fn listed(state_id: &str, scope: &str, updated_at_ms: u64, value: &RawValue) -> bool {
    if value.get().starts_with('{') {
        return created_at_ms(state_id).is_some();
    }
    let Ok(text) = serde_json::from_str::<Cow<'_, str>>(value.get()) else {
        return false;
    };
    serde_json::from_str::<Record>(&text).is_ok_and(|record| {
        record.session_id == state_id
            && record.workspace.as_deref() == Some(scope)
            && record.lineage.origin == "root"
            && record.updated_at_ms == updated_at_ms
    })
}

const fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn uuid(text: &str) -> Option<[u8; 16]> {
    let bytes = text.as_bytes();
    if bytes.len() != 36 || UUID_DASHES.iter().any(|at| bytes[*at] != b'-') {
        return None;
    }
    let mut uuid = [0; 16];
    for (byte, at) in uuid.iter_mut().zip(UUID_DIGITS) {
        *byte = hex(bytes[at])? << 4 | hex(bytes[at + 1])?;
    }
    Some(uuid)
}

impl<'a> Key<'a> {
    fn of(text: Cow<'a, str>) -> Self {
        match uuid(&text) {
            Some(uuid) => Self::Uuid(uuid),
            None => Self::Text(text),
        }
    }

    fn with_text<T>(&self, read: impl FnOnce(&str) -> T) -> T {
        match self {
            Self::Text(text) => read(text),
            Self::Uuid(uuid) => {
                let mut text = [b'-'; 36];
                for (byte, at) in uuid.iter().zip(UUID_DIGITS) {
                    text[at] = HEX[usize::from(byte >> 4)];
                    text[at + 1] = HEX[usize::from(byte & 15)];
                }
                read(std::str::from_utf8(&text).unwrap_or_default())
            }
        }
    }
}

fn brief(state_id: &str, raw: Option<&Raw>) -> Brief<'static> {
    let Some(raw) = raw else {
        return Brief::Unlisted;
    };
    match serde_json::from_str::<Head>(raw.get()) {
        Ok(Head {
            preview,
            scope: Some(scope),
            updated_at_ms,
            value,
        }) if listed(state_id, &scope, updated_at_ms, value) => Brief::Listed {
            preview: preview.map(|preview| Cow::Owned(preview.into_owned())),
            scope: Cow::Owned(scope.into_owned()),
            updated_at_ms,
        },
        _ => Brief::Unreadable,
    }
}

impl<'a> Index<'a> {
    fn find(&self, key: &Key<'_>) -> Option<usize> {
        match &self.keys {
            Some(keys) => keys.get(key).copied(),
            None => self.rows.iter().position(|row| row.key == *key),
        }
    }

    fn row(&mut self, key: Key<'a>) -> usize {
        let at = match self.find(&key) {
            Some(at) => at,
            None => {
                if let Some(keys) = &mut self.keys {
                    keys.insert(key.clone(), self.rows.len());
                }
                self.rows.push(Row {
                    key,
                    names: Default::default(),
                    summary: None,
                });
                self.rows.len() - 1
            }
        };
        self.change(at);
        at
    }

    fn change(&mut self, at: usize) {
        if !self.changed.contains(&at) {
            self.changed.push(at);
        }
    }

    fn listing_line(&mut self, line: Line<'_>, base: bool) -> Option<Option<Stamp>> {
        let (name, entry) = match line {
            Line::Stamp(stamp) => return Some(Some(stamp)),
            Line::Removed(name) => (name, None),
            Line::Entry(entry) => (
                entry.name,
                Some(Entry {
                    created: listing::nanos(entry.created)?,
                    length: entry.length,
                    source: Cow::Owned(entry.source.to_owned()),
                }),
            ),
        };
        let slot = usize::from(archive::unarchived(name).is_some());
        let at = self.row(Key::of(Cow::Owned(key_of(name)?)));
        let next = &mut self.header.next_position;
        let named = &mut self.rows[at].names[slot];
        match (named.position, base) {
            (Some(_), false) => {}
            (None, true) if entry.is_some() => {
                named.position = Some(*next);
                *next += 1;
            }
            (None, false) => {
                named.position = Some(*next);
                *next += 1;
            }
            _ => return None,
        }
        named.entry = entry;
        Some(None)
    }

    fn replay_listing(&mut self, text: &str, mut stamp: Option<Stamp>) -> Option<Stamp> {
        let mut read = self.header.listing_end;
        for line in text.split_inclusive('\n') {
            read += line.len() as u64;
            let base = self.header.listing_base == 0;
            match self.listing_line(listing::line(line)?, base)? {
                Some(next) => {
                    if base {
                        self.header.listing_base = read;
                    }
                    stamp = Some(next);
                }
                None => stamp = None,
            }
        }
        self.header.listing_end = read;
        stamp
    }

    fn replay_summaries(&mut self, bytes: &[u8]) {
        let mut applied = 0;
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            if !line.ends_with(b"\n") {
                break;
            }
            let Ok(change) = serde_json::from_slice::<Change>(line) else {
                break;
            };
            applied += line.len() as u64;
            match change {
                Change::Remember(remembered) => {
                    for (state_id, summary) in remembered {
                        let known = Known {
                            brief: brief(&state_id, summary.summary.as_ref()),
                            file: Cow::Owned(summary.file),
                            length: summary.length,
                            revision: summary.revision,
                        };
                        let at = self.row(Key::of(Cow::Owned(state_id)));
                        self.rows[at].summary = Some(known);
                    }
                }
                Change::Moved {
                    from, to, length, ..
                } => {
                    for at in 0..self.rows.len() {
                        let Some(known) = &mut self.rows[at].summary else {
                            continue;
                        };
                        if known.file == from {
                            known.file = Cow::Owned(to.clone());
                            known.length = length;
                            self.change(at);
                        }
                    }
                }
            }
        }
        self.header.summaries_end += applied;
    }
}

fn directory_stamp(directory: &Path) -> Option<Stamp> {
    listing::stamp(&fs::metadata(directory).ok()?)
}

fn tail(file: &mut File, from: u64, to: u64) -> Option<Vec<u8>> {
    let mut bytes = vec![0; usize::try_from(to.checked_sub(from)?).ok()?];
    file.seek(SeekFrom::Start(from)).ok()?;
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

pub(super) fn build<'a>(directory: &Path, summaries: Journaled<'a>) -> Option<Index<'a>> {
    let (journal, end, entries) = summaries;
    let mut file = File::open(directory.join(LISTING)).ok()?;
    let metadata = file.metadata().ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    let mut index = Index {
        changed: Vec::new(),
        header: Header {
            listing: Cow::Owned(identity(&metadata).ok()?),
            listing_base: 0,
            listing_end: 0,
            next_position: 0,
            stamp: Stamp {
                directory: String::new(),
                modified: 0,
            },
            summaries: Cow::Borrowed(journal),
            summaries_end: end,
        },
        keys: Some(HashMap::with_capacity(entries.len())),
        rows: Vec::with_capacity(entries.len()),
    };
    for (state_id, summary) in entries {
        let key = Key::of(Cow::Borrowed(state_id.as_str()));
        if let Some(keys) = &mut index.keys {
            keys.insert(key.clone(), index.rows.len());
        }
        index.rows.push(Row {
            key,
            names: Default::default(),
            summary: Some(Known {
                brief: brief(state_id, summary.summary.as_ref()),
                file: Cow::Borrowed(&summary.file),
                length: summary.length,
                revision: summary.revision,
            }),
        });
    }
    let stamp = index.replay_listing(&text, None)?;
    let base = index.header.listing_base;
    if base == 0 || index.header.listing_end - base > base || directory_stamp(directory)? != stamp {
        return None;
    }
    index.header.stamp = stamp;
    index.changed.clear();
    index.keys = None;
    Some(index)
}

pub(super) fn refresh(index: &mut Index<'_>, directory: &Path) -> Option<u64> {
    let mut replayed = 0;
    let mut file = File::open(directory.join(LISTING)).ok()?;
    let metadata = file.metadata().ok()?;
    if identity(&metadata).ok()? != index.header.listing {
        return None;
    }
    let length = metadata.len();
    let start = index.header.listing_end;
    if length != start {
        let bytes = tail(&mut file, start, length)?;
        let stamp = index.replay_listing(
            std::str::from_utf8(&bytes).ok()?,
            Some(index.header.stamp.clone()),
        )?;
        let base = index.header.listing_base;
        if index.header.listing_end - base > base {
            return None;
        }
        index.header.stamp = stamp;
        replayed += length - start;
    }
    if directory_stamp(directory)? != index.header.stamp {
        return None;
    }
    let mut file = File::open(directory.join(summaries::FILE)).ok()?;
    let metadata = file.metadata().ok()?;
    if identity(&metadata).ok()? != index.header.summaries {
        return None;
    }
    let length = metadata.len();
    let start = index.header.summaries_end;
    if length != start {
        let bytes = tail(&mut file, start, length)?;
        index.replay_summaries(&bytes);
        replayed += index.header.summaries_end - start;
    }
    Some(replayed)
}

type Present<'b> = (Cow<'b, str>, u64, u128, u64);

fn present<'b>(row: &'b Row<'_>, directory: &Path) -> Option<Option<Present<'b>>> {
    match (&row.names[0].entry, &row.names[1].entry) {
        (Some(_), Some(_)) => None,
        (None, None) => Some(None),
        (None, Some(entry)) => Some(Some((
            Cow::Borrowed(&*entry.source),
            entry.length,
            entry.created,
            row.names[1].position?,
        ))),
        (Some(_), None) => {
            let metadata = row
                .key
                .with_text(|key| fs::symlink_metadata(directory.join(file_name(key))))
                .ok()
                .filter(Metadata::is_file)?;
            Some(Some((
                Cow::Owned(identity(&metadata).ok()?),
                metadata.len(),
                listing::nanos(created(&metadata))?,
                row.names[0].position?,
            )))
        }
    }
}

pub(super) fn query(
    index: &Index<'_>,
    directory: &Path,
    scope: Option<&str>,
    limit: usize,
    keep: &BriefFilter,
) -> Option<Vec<BriefState>> {
    let mut candidates = Vec::new();
    let mut same = None::<(&str, bool)>;
    for (at, row) in index.rows.iter().enumerate() {
        let Some((source, length, created, position)) = present(row, directory)? else {
            continue;
        };
        if !row.key.with_text(|state_id| keep(state_id)) {
            continue;
        }
        let known = row
            .summary
            .as_ref()
            .filter(|known| known.file == source && known.length == length)?;
        let (listed, updated_at_ms) = match &known.brief {
            Brief::Unlisted => continue,
            Brief::Unreadable => return None,
            Brief::Listed {
                scope: listed,
                updated_at_ms,
                ..
            } => (&**listed, *updated_at_ms),
        };
        if let Some(scope) = scope {
            let matched = match same {
                Some((last, matched)) if last == listed => matched,
                _ => {
                    let matched = listed == scope || Path::new(listed) == Path::new(scope);
                    same = Some((listed, matched));
                    matched
                }
            };
            if !matched {
                continue;
            }
        }
        candidates.push((Reverse(updated_at_ms), Reverse(created), position, at));
    }
    candidates.sort_unstable();
    candidates.truncate(limit);
    Some(
        candidates
            .into_iter()
            .filter_map(|(Reverse(updated_at_ms), _, _, at)| {
                let row = &index.rows[at];
                let Some(Known {
                    brief: Brief::Listed { preview, scope, .. },
                    ..
                }) = &row.summary
                else {
                    return None;
                };
                Some(BriefState {
                    preview: preview.as_deref().map(str::to_owned),
                    scope: Some(scope.clone().into_owned()),
                    state_id: row.key.with_text(str::to_owned),
                    updated_at_ms,
                })
            })
            .collect(),
    )
}

fn varint(out: &mut Vec<u8>, value: impl Into<u128>) {
    let mut value = value.into();
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn text(out: &mut Vec<u8>, text: &str) {
    varint(out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

const fn updated(row: &Row<'_>) -> u64 {
    match &row.summary {
        Some(Known {
            brief: Brief::Listed { updated_at_ms, .. },
            ..
        }) => *updated_at_ms,
        _ => 0,
    }
}

pub(super) fn encode(index: &Index<'_>, end: usize, all: bool) -> Option<Vec<u8>> {
    let mut rows = if all {
        (0..index.rows.len()).collect::<Vec<_>>()
    } else {
        index.changed.clone()
    };
    if all {
        rows.sort_by_key(|at| Reverse(updated(&index.rows[*at])));
    }
    let mut scopes = Vec::<&str>::new();
    let mut scoped = HashMap::<&str, u64>::new();
    for at in &rows {
        if let Some(Known {
            brief: Brief::Listed { scope, .. },
            ..
        }) = &index.rows[*at].summary
            && !scoped.contains_key(&**scope)
        {
            scoped.insert(scope, scopes.len() as u64);
            scopes.push(scope);
        }
    }
    let mut out = Vec::new();
    if all {
        out.extend_from_slice(MAGIC);
    }
    let start = out.len();
    out.extend_from_slice(&[0; LENGTH_BYTES]);
    let header = &index.header;
    varint(&mut out, (if all { MAGIC.len() } else { end }) as u64);
    text(&mut out, &header.listing);
    varint(&mut out, header.listing_base);
    varint(&mut out, header.listing_end);
    text(&mut out, &header.stamp.directory);
    varint(&mut out, header.stamp.modified);
    text(&mut out, &header.summaries);
    varint(&mut out, header.summaries_end);
    varint(&mut out, header.next_position);
    varint(&mut out, scopes.len() as u64);
    for scope in &scopes {
        text(&mut out, scope);
    }
    varint(&mut out, rows.len() as u64);
    for at in rows {
        let row = &index.rows[at];
        let source = row
            .names
            .iter()
            .find_map(|name| name.entry.as_ref())
            .map(|entry| &*entry.source);
        let mut flags = 0;
        if matches!(row.key, Key::Uuid(_)) {
            flags |= KEY_UUID;
        }
        for (name, seen, present) in [
            (&row.names[0], PLAIN_SEEN, PLAIN_PRESENT),
            (&row.names[1], ARCHIVED_SEEN, ARCHIVED_PRESENT),
        ] {
            if name.position.is_some() {
                flags |= seen;
            }
            if name.entry.is_some() {
                flags |= present;
            }
        }
        if let Some(known) = &row.summary {
            flags |= SUMMARY;
            if source == Some(&*known.file) {
                flags |= FILE_SOURCE;
            }
            match &known.brief {
                Brief::Listed { preview, .. } => {
                    flags |= LISTED;
                    if preview.is_some() {
                        flags |= PREVIEW;
                    }
                }
                Brief::Unlisted => {}
                Brief::Unreadable => flags |= UNREADABLE,
            }
        }
        varint(&mut out, flags);
        match &row.key {
            Key::Uuid(uuid) => out.extend_from_slice(uuid),
            Key::Text(key) => text(&mut out, key),
        }
        for name in &row.names {
            if let Some(position) = name.position {
                varint(&mut out, position);
            }
        }
        for name in &row.names {
            if let Some(entry) = &name.entry {
                varint(&mut out, entry.created);
                varint(&mut out, entry.length);
                text(&mut out, &entry.source);
            }
        }
        if let Some(known) = &row.summary {
            if flags & FILE_SOURCE == 0 {
                text(&mut out, &known.file);
            }
            varint(&mut out, known.length);
            varint(&mut out, known.revision);
            if let Brief::Listed {
                preview,
                scope,
                updated_at_ms,
            } = &known.brief
            {
                varint(&mut out, scoped[&**scope]);
                varint(&mut out, *updated_at_ms);
                if let Some(preview) = preview {
                    text(&mut out, preview);
                }
            }
        }
    }
    let length = u32::try_from(out.len() - start - LENGTH_BYTES).ok()?;
    out[start..start + LENGTH_BYTES].copy_from_slice(&length.to_le_bytes());
    Some(out)
}

struct Reader<'a> {
    at: usize,
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Option<u128> {
        let first = *self.bytes.get(self.at)?;
        if first < 0x80 {
            self.at += 1;
            return Some(u128::from(first));
        }
        let mut value = 0u128;
        let mut shift = 0;
        loop {
            let byte = *self.bytes.get(self.at)?;
            self.at += 1;
            if shift > 127 {
                return None;
            }
            value |= u128::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
        }
    }

    fn number(&mut self) -> Option<u64> {
        u64::try_from(self.varint()?).ok()
    }

    fn bytes(&mut self, length: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(length)?;
        let bytes = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(bytes)
    }

    fn text(&mut self) -> Option<&'a str> {
        let length = usize::try_from(self.number()?).ok()?;
        std::str::from_utf8(self.bytes(length)?).ok()
    }

    fn entry(&mut self) -> Option<Entry<'a>> {
        Some(Entry {
            created: self.varint()?,
            length: self.number()?,
            source: Cow::Borrowed(self.text()?),
        })
    }

    fn row(&mut self, scopes: &[&'a str]) -> Option<Row<'a>> {
        let flags = self.number()?;
        let key = if flags & KEY_UUID != 0 {
            Key::Uuid(self.bytes(16)?.try_into().ok()?)
        } else {
            Key::Text(Cow::Borrowed(self.text()?))
        };
        let mut names: [Name<'a>; 2] = Default::default();
        if flags & PLAIN_SEEN != 0 {
            names[0].position = Some(self.number()?);
        }
        if flags & ARCHIVED_SEEN != 0 {
            names[1].position = Some(self.number()?);
        }
        if flags & PLAIN_PRESENT != 0 {
            names[0].entry = Some(self.entry()?);
        }
        if flags & ARCHIVED_PRESENT != 0 {
            names[1].entry = Some(self.entry()?);
        }
        if flags & SUMMARY == 0 {
            return Some(Row {
                key,
                names,
                summary: None,
            });
        }
        let file = if flags & FILE_SOURCE != 0 {
            names
                .iter()
                .find_map(|name| name.entry.as_ref())?
                .source
                .clone()
        } else {
            Cow::Borrowed(self.text()?)
        };
        let length = self.number()?;
        let revision = self.number()?;
        let brief = if flags & LISTED != 0 {
            let scope = *scopes.get(usize::try_from(self.number()?).ok()?)?;
            let updated_at_ms = self.number()?;
            let preview = if flags & PREVIEW != 0 {
                Some(Cow::Borrowed(self.text()?))
            } else {
                None
            };
            Brief::Listed {
                preview,
                scope: Cow::Borrowed(scope),
                updated_at_ms,
            }
        } else if flags & UNREADABLE != 0 {
            Brief::Unreadable
        } else {
            Brief::Unlisted
        };
        Some(Row {
            key,
            names,
            summary: Some(Known {
                brief,
                file,
                length,
                revision,
            }),
        })
    }

    fn record(&mut self) -> Option<(Header<'a>, Vec<Row<'a>>)> {
        let header = Header {
            listing: Cow::Borrowed(self.text()?),
            listing_base: self.number()?,
            listing_end: self.number()?,
            stamp: Stamp {
                directory: self.text()?.to_owned(),
                modified: self.varint()?,
            },
            summaries: Cow::Borrowed(self.text()?),
            summaries_end: self.number()?,
            next_position: self.number()?,
        };
        let scopes = (0..self.number()?)
            .map(|_| self.text())
            .collect::<Option<Vec<_>>>()?;
        let count = usize::try_from(self.number()?).ok()?;
        let mut rows = Vec::with_capacity(count);
        for _ in 0..count {
            rows.push(self.row(&scopes)?);
        }
        (self.at == self.bytes.len()).then_some((header, rows))
    }
}

pub(super) fn decode(bytes: &[u8]) -> Option<Decoded<'_>> {
    if !bytes.starts_with(MAGIC) {
        return None;
    }
    let mut at = MAGIC.len();
    let mut decoded = None::<Decoded<'_>>;
    while let Some(prefix) = bytes.get(at..at + LENGTH_BYTES) {
        let length = usize::try_from(u32::from_le_bytes(prefix.try_into().ok()?)).ok()?;
        let end = at + LENGTH_BYTES + length;
        let Some(payload) = bytes.get(at + LENGTH_BYTES..end) else {
            break;
        };
        let mut reader = Reader {
            at: 0,
            bytes: payload,
        };
        if reader.number() != Some(at as u64) {
            break;
        }
        let Some((header, rows)) = reader.record() else {
            break;
        };
        match &mut decoded {
            None => {
                decoded = Some(Decoded {
                    base: end,
                    end,
                    index: Index {
                        changed: Vec::new(),
                        header,
                        keys: None,
                        rows,
                    },
                });
            }
            Some(decoded) => {
                decoded.end = end;
                decoded.index.header = header;
                for row in rows {
                    match decoded.index.find(&row.key) {
                        Some(found) => decoded.index.rows[found] = row,
                        None => decoded.index.rows.push(row),
                    }
                }
            }
        }
        at = end;
    }
    decoded
}

pub(super) fn append(directory: &Path, bytes: &[u8], expected: u64) -> Option<()> {
    let mut file = OpenOptions::new()
        .append(true)
        .open(directory.join(FILE))
        .ok()?;
    file.try_lock().ok()?;
    if file.metadata().ok()?.len() != expected {
        return None;
    }
    file.write_all(bytes).ok()
}

pub(super) fn replace(directory: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut temporary = TemporaryFile::create_in(directory, FILE, None)?;
    temporary.write_all(bytes)?;
    temporary
        .persist(directory.join(FILE))
        .map_err(|error| error.error)?;
    Ok(())
}
