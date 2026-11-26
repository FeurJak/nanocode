use std::sync::Arc;

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_partitions::{Partition, Row};
use nanocode_sync::{Atom, OrdMap};
use nanocode_turnlog::parted::PartStore;

use crate::sim::{self, Mutation};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub scope: String,
    pub epoch: u64,
    pub seq: u64,
    pub parts: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fence {
    pub part: Partition,
    pub epoch: u64,
    pub holder: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Written {
    Landed,
    Stale,
}

pub trait Checkpoints: Send + Sync {
    fn latest<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Option<Manifest>, StoreError>>;

    fn parts<'a>(
        &'a self,
        digests: &'a [String],
    ) -> StoreFuture<'a, Result<Vec<(String, String)>, StoreError>>;

    fn write<'a>(
        &'a self,
        fence: &'a Fence,
        manifest: &'a Manifest,
        parts: &'a [(String, String)],
    ) -> StoreFuture<'a, Result<Written, StoreError>>;
}

#[must_use]
pub fn digest(body: &str) -> String {
    nanocode_digest::blake3(body.as_bytes())
        .to_hex()
        .as_str()
        .to_owned()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryStats {
    pub manifests: u64,
    pub manifests_stale: u64,
    pub parts_written: u64,
    pub part_bytes_written: u64,
}

#[derive(Clone, Default)]
struct Shelf {
    manifests: OrdMap<Arc<str>, Arc<Vec<Manifest>>>,
    parts: OrdMap<String, Arc<str>>,
    stats: MemoryStats,
}

pub struct MemoryCheckpoints<S: ?Sized> {
    fences: Arc<S>,
    shelf: Atom<Shelf>,
}

impl<S: PartStore + ?Sized> MemoryCheckpoints<S> {
    pub fn new(fences: Arc<S>) -> Self {
        Self {
            fences,
            shelf: Atom::default(),
        }
    }

    pub fn stats(&self) -> MemoryStats {
        self.shelf.read(|shelf| shelf.stats)
    }

    pub fn manifests(&self, scope: &str) -> Vec<Manifest> {
        self.shelf.read(|shelf| {
            shelf
                .manifests
                .get(scope)
                .map(|kept| kept.as_ref().clone())
                .unwrap_or_default()
        })
    }

    pub fn part_bytes(&self) -> u64 {
        self.shelf.read(|shelf| {
            shelf
                .parts
                .values()
                .map(|body| u64::try_from(body.len()).unwrap_or(u64::MAX))
                .sum()
        })
    }

    fn admit(&self, row: &Row<String>, fence: &Fence) -> bool {
        sim::mutated(Mutation::CheckpointSkipsEpoch)
            || matches!(row, Row::Held { holder, epoch, .. } if *holder == fence.holder && *epoch == fence.epoch)
    }

    fn land(&self, manifest: &Manifest, parts: &[(String, String)]) -> Written {
        self.shelf.update(|shelf| {
            let kept = shelf
                .manifests
                .get(manifest.scope.as_str())
                .cloned()
                .unwrap_or_default();
            if let Some(last) = kept.last() {
                if last.seq == manifest.seq && last.epoch == manifest.epoch {
                    return Written::Landed;
                }
                if last.seq >= manifest.seq || last.epoch > manifest.epoch {
                    shelf.stats.manifests_stale += 1;
                    return Written::Stale;
                }
            }
            for (digest, body) in parts {
                shelf.stats.parts_written += 1;
                shelf.stats.part_bytes_written += u64::try_from(body.len()).unwrap_or(u64::MAX);
                shelf.parts.insert(digest.clone(), Arc::from(body.as_str()));
            }
            let mut next = kept.as_ref().clone();
            next.push(manifest.clone());
            shelf
                .manifests
                .insert(Arc::from(manifest.scope.as_str()), Arc::new(next));
            shelf.stats.manifests += 1;
            Written::Landed
        })
    }
}

impl<S: PartStore + ?Sized> Checkpoints for MemoryCheckpoints<S> {
    fn latest<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Option<Manifest>, StoreError>> {
        let latest = self.shelf.read(|shelf| {
            shelf
                .manifests
                .get(scope)
                .and_then(|kept| kept.last().cloned())
        });
        Box::pin(async move { Ok(latest) })
    }

    fn parts<'a>(
        &'a self,
        digests: &'a [String],
    ) -> StoreFuture<'a, Result<Vec<(String, String)>, StoreError>> {
        let found = self.shelf.read(|shelf| {
            digests
                .iter()
                .filter_map(|digest| {
                    shelf
                        .parts
                        .get(digest)
                        .map(|body| (digest.clone(), body.as_ref().to_owned()))
                })
                .collect()
        });
        Box::pin(async move { Ok(found) })
    }

    fn write<'a>(
        &'a self,
        fence: &'a Fence,
        manifest: &'a Manifest,
        parts: &'a [(String, String)],
    ) -> StoreFuture<'a, Result<Written, StoreError>> {
        Box::pin(async move {
            let row = self.fences.fence(fence.part).await?;
            if !self.admit(&row, fence) {
                self.shelf.update(|shelf| shelf.stats.manifests_stale += 1);
                return Ok(Written::Stale);
            }
            Ok(self.land(manifest, parts))
        })
    }
}
