use std::{collections::BTreeSet, io, sync::OnceLock};

use nanocode_compression::inflate::{InflateStatus, inflate_all};

use crate::{Kind, Stat};

include!(concat!(env!("OUT_DIR"), "/site_data.rs"));

fn root() -> &'static [u8] {
    static ROOT: OnceLock<Vec<u8>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let (major, minor) = (rustpython_vm::version::MAJOR, rustpython_vm::version::MINOR);
        format!("/usr/local/lib/python{major}.{minor}/site-packages").into_bytes()
    })
}

fn relative(path: &[u8]) -> Option<&str> {
    let root = root();
    let rest = path.strip_prefix(root)?;
    let rest = if rest.is_empty() {
        rest
    } else {
        rest.strip_prefix(b"/")?
    };
    let rest = rest.strip_suffix(b"/").unwrap_or(rest);
    std::str::from_utf8(rest).ok()
}

fn file(path: &[u8]) -> Option<&'static (&'static str, u32, &'static [u8])> {
    let name = relative(path)?;
    FILES
        .binary_search_by(|entry| entry.0.cmp(name))
        .ok()
        .map(|index| &FILES[index])
}

/// Whether `path` is a directory the table holds files under, the site-packages root and its ancestors included.
fn dir(path: &[u8]) -> bool {
    let path = path.strip_suffix(b"/").unwrap_or(path);
    if root().starts_with(path)
        && (path.is_empty() || root().len() == path.len() || root()[path.len()] == b'/')
    {
        return true;
    }
    relative(path).is_some_and(|name| {
        name.is_empty()
            || FILES.iter().any(|entry| {
                entry.0.len() > name.len()
                    && entry.0.starts_with(name)
                    && entry.0.as_bytes()[name.len()] == b'/'
            })
    })
}

pub fn stat(path: &[u8]) -> Option<Stat> {
    if let Some(entry) = file(path) {
        return Some(Stat {
            kind: Kind::File,
            size: entry.1.into(),
            mode: 0o444,
            mtime_ns: 0,
        });
    }
    dir(path).then_some(Stat {
        kind: Kind::Dir,
        size: 0,
        mode: 0o555,
        mtime_ns: 0,
    })
}

pub fn read(path: &[u8]) -> Option<io::Result<Vec<u8>>> {
    let entry = file(path)?;
    Some(match inflate_all(-15, entry.2, entry.1 as usize) {
        Ok((InflateStatus::StreamEnd, data)) if data.len() == entry.1 as usize => Ok(data),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "corrupt embedded site data",
        )),
    })
}

/// The names the table holds directly under `path`, or None when it holds nothing there.
pub fn list(path: &[u8]) -> Option<BTreeSet<Vec<u8>>> {
    if !dir(path) {
        return None;
    }
    let path = path.strip_suffix(b"/").unwrap_or(path);
    let mut names = BTreeSet::new();
    if root().len() > path.len() {
        let next = &root()[path.len()..];
        let next = next.strip_prefix(b"/").unwrap_or(next);
        names.insert(
            next.split(|byte| *byte == b'/')
                .next()
                .unwrap_or(next)
                .to_vec(),
        );
        return Some(names);
    }
    let name = relative(path)?;
    for entry in FILES {
        let rest = if name.is_empty() {
            Some(entry.0)
        } else {
            entry
                .0
                .strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('/'))
        };
        if let Some(rest) = rest {
            names.insert(rest.split('/').next().unwrap_or(rest).as_bytes().to_vec());
        }
    }
    Some(names)
}
