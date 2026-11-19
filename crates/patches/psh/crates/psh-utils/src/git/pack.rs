use std::collections::HashMap;
use std::io;

use super::store::{Kind, Oid, apply_delta, corrupt, object_oid};
use crate::codec::{Format, Inflater, InputEnd, Progress, compress, crc32};
use crate::sum_hash::{Algo, Digest};

const SIGNATURE: &[u8] = b"PACK";
const READ_VERSIONS: [u32; 2] = [2, 3];
const WRITE_VERSION: u32 = 2;
const HEAD: usize = 12;
const OID: usize = 20;
const COMMIT: u8 = 1;
const TREE: u8 = 2;
const BLOB: u8 = 3;
const TAG: u8 = 4;
const OFS_DELTA: u8 = 6;
const REF_DELTA: u8 = 7;
const MORE: u8 = 0x80;
const LOW7: u8 = 0x7f;
const LOW4: u8 = 0x0f;
const IDX_MAGIC: [u8; 4] = [0xff, b't', b'O', b'c'];
const IDX_VERSION: u32 = 2;
const FANOUT: usize = 256;
const LARGE: u32 = 0x8000_0000;
const GIT_DEFAULT_ZLIB_LEVEL: u8 = 6;

#[derive(Clone, Copy)]
enum Base {
    Whole(Kind),
    Offset(usize),
    Named(Oid),
}

struct Entry {
    offset: usize,
    data: usize,
    end: usize,
    size: usize,
    base: Base,
    crc: u32,
}

pub struct Indexed {
    entries: Vec<Entry>,
    found: Vec<(Oid, Kind)>,
    by_oid: HashMap<Oid, usize>,
    pub checksum: Oid,
}

fn code(kind: Kind) -> u8 {
    match kind {
        Kind::Commit => COMMIT,
        Kind::Tree => TREE,
        Kind::Blob => BLOB,
        Kind::Tag => TAG,
    }
}

fn byte(pack: &[u8], at: &mut usize) -> io::Result<u8> {
    let b = *pack
        .get(*at)
        .ok_or_else(|| corrupt("the pack ends inside an entry"))?;
    *at += 1;
    Ok(b)
}

fn sha1(data: &[u8]) -> Oid {
    let mut digest = Digest::new(Algo::Sha1);
    digest.update(data);
    Oid::from_bytes(&digest.finish())
}

fn inflate(inflater: &mut Inflater, input: &[u8], size: usize) -> io::Result<(Vec<u8>, usize)> {
    inflater.reset();
    let mut out = Vec::new();
    match inflater.inflate(input, InputEnd::Final, &mut out, size) {
        Ok(Progress::Done { consumed }) if out.len() == size => Ok((out, consumed)),
        _ => Err(corrupt("a pack entry does not inflate to its size")),
    }
}

fn entry(pack: &[u8], offset: usize, end: usize) -> io::Result<(u8, Entry)> {
    let bad = || corrupt("a pack entry's header is malformed");
    let body = &pack[..end];
    let mut at = offset;
    let mut c = byte(body, &mut at)?;
    let kind = (c >> 4) & (LOW7 >> 4);
    let mut size = usize::from(c & LOW4);
    let mut shift = 4u32;
    while c & MORE != 0 {
        c = byte(body, &mut at)?;
        let part = usize::from(c & LOW7)
            .checked_shl(shift)
            .filter(|part| part >> shift == usize::from(c & LOW7))
            .ok_or_else(bad)?;
        size |= part;
        shift += 7;
    }
    let base = match kind {
        COMMIT => Base::Whole(Kind::Commit),
        TREE => Base::Whole(Kind::Tree),
        BLOB => Base::Whole(Kind::Blob),
        TAG => Base::Whole(Kind::Tag),
        OFS_DELTA => {
            let mut c = byte(body, &mut at)?;
            let mut back = usize::from(c & LOW7);
            while c & MORE != 0 {
                c = byte(body, &mut at)?;
                back = back
                    .checked_add(1)
                    .and_then(|b| b.checked_mul(usize::from(MORE)))
                    .ok_or_else(bad)?
                    | usize::from(c & LOW7);
            }
            match offset.checked_sub(back) {
                Some(base) if back > 0 => Base::Offset(base),
                _ => return Err(bad()),
            }
        }
        REF_DELTA => {
            let name = body.get(at..at + OID).ok_or_else(bad)?;
            at += OID;
            Base::Named(Oid::from_bytes(name))
        }
        _ => return Err(corrupt("a pack entry has an unknown type")),
    };
    Ok((
        kind,
        Entry {
            offset,
            data: at,
            end: at,
            size,
            base,
            crc: 0,
        },
    ))
}

pub fn index(pack: &[u8]) -> io::Result<Indexed> {
    if pack.len() < HEAD + OID || &pack[..SIGNATURE.len()] != SIGNATURE {
        return Err(corrupt("the remote did not send a pack"));
    }
    let version = u32::from_be_bytes([pack[4], pack[5], pack[6], pack[7]]);
    if !READ_VERSIONS.contains(&version) {
        return Err(corrupt("the pack is of a version this git does not read"));
    }
    let count = u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]) as usize;
    let end = pack.len() - OID;
    let checksum = Oid::from_bytes(&pack[end..]);
    if sha1(&pack[..end]) != checksum {
        return Err(corrupt("the pack's checksum does not match its content"));
    }
    let mut inflater = Inflater::new(Format::Zlib);
    let mut entries: Vec<Entry> = Vec::new();
    let mut found: Vec<Option<(Oid, Kind)>> = Vec::new();
    let mut by_offset = HashMap::new();
    let mut at = HEAD;
    for _ in 0..count {
        let (_, mut item) = entry(pack, at, end)?;
        let (data, consumed) = inflate(&mut inflater, &pack[item.data..end], item.size)?;
        item.end = item.data + consumed;
        item.crc = crc32(&pack[item.offset..item.end]);
        by_offset.insert(item.offset, entries.len());
        found.push(match item.base {
            Base::Whole(kind) => Some((object_oid(kind, &data), kind)),
            Base::Offset(_) | Base::Named(_) => None,
        });
        at = item.end;
        entries.push(item);
    }
    if at != end {
        return Err(corrupt("the pack holds more than its entries"));
    }
    let mut by_base: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut by_name: HashMap<Oid, Vec<usize>> = HashMap::new();
    for (i, item) in entries.iter().enumerate() {
        match item.base {
            Base::Whole(_) => {}
            Base::Offset(base) => {
                let base = *by_offset
                    .get(&base)
                    .ok_or_else(|| corrupt("a delta's base is not an entry of the pack"))?;
                by_base.entry(base).or_default().push(i);
            }
            Base::Named(name) => by_name.entry(name).or_default().push(i),
        }
    }
    for root in 0..entries.len() {
        let Base::Whole(kind) = entries[root].base else {
            continue;
        };
        let Some((oid, _)) = found[root] else {
            continue;
        };
        if !by_base.contains_key(&root) && !by_name.contains_key(&oid) {
            continue;
        }
        let item = &entries[root];
        let (data, _) = inflate(&mut inflater, &pack[item.data..item.end], item.size)?;
        let mut stack = vec![(root, oid, data)];
        while let Some((at, oid, data)) = stack.pop() {
            let mut children = by_base.remove(&at).unwrap_or_default();
            children.extend(by_name.remove(&oid).unwrap_or_default());
            for child in children {
                let item = &entries[child];
                let (delta, _) = inflate(&mut inflater, &pack[item.data..item.end], item.size)?;
                let whole = apply_delta(&data, &delta)?;
                let oid = object_oid(kind, &whole);
                found[child] = Some((oid, kind));
                stack.push((child, oid, whole));
            }
        }
    }
    let found: Vec<(Oid, Kind)> = found
        .into_iter()
        .collect::<Option<_>>()
        .ok_or_else(|| corrupt("the pack has deltas whose bases it does not hold"))?;
    let mut by_oid = HashMap::new();
    for (i, (oid, _)) in found.iter().enumerate() {
        by_oid.entry(*oid).or_insert(i);
    }
    Ok(Indexed {
        entries,
        found,
        by_oid,
        checksum,
    })
}

impl Indexed {
    pub fn kind(&self, oid: &Oid) -> Option<Kind> {
        self.by_oid.get(oid).map(|&i| self.found[i].1)
    }

    pub fn read(&self, pack: &[u8], oid: &Oid) -> io::Result<Option<(Kind, Vec<u8>)>> {
        let Some(&first) = self.by_oid.get(oid) else {
            return Ok(None);
        };
        let mut chain = vec![first];
        loop {
            let last = chain[chain.len() - 1];
            let next = match self.entries[last].base {
                Base::Whole(_) => break,
                Base::Offset(offset) => self
                    .entries
                    .binary_search_by_key(&offset, |item| item.offset)
                    .map_err(|_| corrupt("a delta's base is not an entry of the pack"))?,
                Base::Named(name) => *self
                    .by_oid
                    .get(&name)
                    .ok_or_else(|| corrupt("the pack has deltas whose bases it does not hold"))?,
            };
            chain.push(next);
        }
        let mut inflater = Inflater::new(Format::Zlib);
        let mut data = Vec::new();
        for (step, &i) in chain.iter().rev().enumerate() {
            let item = &self.entries[i];
            let (part, _) = inflate(&mut inflater, &pack[item.data..item.end], item.size)?;
            data = if step == 0 {
                part
            } else {
                apply_delta(&data, &part)?
            };
        }
        Ok(Some((self.found[first].1, data)))
    }

    pub fn idx(&self) -> Vec<u8> {
        let mut sorted: Vec<(Oid, u32, u64)> = self
            .by_oid
            .values()
            .map(|&i| {
                let item = &self.entries[i];
                (self.found[i].0, item.crc, item.offset as u64)
            })
            .collect();
        sorted.sort_by(|a, b| a.0.0.cmp(&b.0.0));
        let mut out = Vec::new();
        out.extend_from_slice(&IDX_MAGIC);
        out.extend_from_slice(&IDX_VERSION.to_be_bytes());
        let mut counts = [0u32; FANOUT];
        for (oid, _, _) in &sorted {
            counts[usize::from(oid.0[0])] += 1;
        }
        let mut total = 0u32;
        for count in counts {
            total += count;
            out.extend_from_slice(&total.to_be_bytes());
        }
        for (oid, _, _) in &sorted {
            out.extend_from_slice(&oid.0);
        }
        for (_, crc, _) in &sorted {
            out.extend_from_slice(&crc.to_be_bytes());
        }
        let mut large = Vec::new();
        for (_, _, offset) in &sorted {
            match u32::try_from(*offset) {
                Ok(small) if small < LARGE => out.extend_from_slice(&small.to_be_bytes()),
                _ => {
                    let slot = LARGE | large.len() as u32;
                    out.extend_from_slice(&slot.to_be_bytes());
                    large.push(*offset);
                }
            }
        }
        for offset in large {
            out.extend_from_slice(&offset.to_be_bytes());
        }
        out.extend_from_slice(&self.checksum.0);
        let own = sha1(&out);
        out.extend_from_slice(&own.0);
        out
    }
}

pub fn write(objects: &[(Kind, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(SIGNATURE);
    out.extend_from_slice(&WRITE_VERSION.to_be_bytes());
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    for (kind, data) in objects {
        let mut size = data.len();
        let mut c = (code(*kind) << 4) | (size as u8 & LOW4);
        size >>= 4;
        while size > 0 {
            out.push(c | MORE);
            c = size as u8 & LOW7;
            size >>= 7;
        }
        out.push(c);
        out.extend_from_slice(&compress(data, GIT_DEFAULT_ZLIB_LEVEL, Format::Zlib));
    }
    let own = sha1(&out);
    out.extend_from_slice(&own.0);
    out
}
