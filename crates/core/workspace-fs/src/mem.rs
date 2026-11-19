use crate::{
    model::{Digest, Manifest, Refusal},
    store::{Batch, Blob, Failure, Store},
};
use nanocode_durability_core::StoreFuture;
use nanocode_sync::Atom;
use std::collections::BTreeMap;

#[derive(Clone, Default)]
struct Held {
    heads: BTreeMap<String, Manifest>,
    blobs: BTreeMap<Digest, (Blob, u64)>,
}

#[derive(Default)]
pub struct MemStore {
    held: Atom<Held>,
}

impl MemStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn blob_count(&self) -> usize {
        self.held.read(|held| held.blobs.len())
    }
}

impl Store for MemStore {
    fn load<'a>(&'a self, workspace: &'a str) -> StoreFuture<'a, Result<Manifest, Failure>> {
        let manifest = self
            .held
            .read(|held| held.heads.get(workspace).cloned().unwrap_or_default());
        Box::pin(std::future::ready(Ok(manifest)))
    }

    fn commit<'a>(
        &'a self,
        workspace: &'a str,
        batch: &'a Batch,
    ) -> StoreFuture<'a, Result<u64, Failure>> {
        let change = batch.change();
        let result = self.held.modify(|held| {
            let mut head = held.heads.get(workspace).cloned().unwrap_or_default();
            if head.version() != change.base {
                return Err(Failure::Refused(Refusal::Stale {
                    base: change.base,
                    head: head.version(),
                }));
            }
            head.advance(change);
            for blob in batch.blobs() {
                held.blobs
                    .entry(*blob.digest())
                    .or_insert_with(|| (blob.clone(), 0))
                    .1 += 1;
            }
            for digest in &change.dropped {
                let Some((_, count)) = held.blobs.get_mut(digest) else {
                    return Err(Failure::Missing(*digest));
                };
                *count -= 1;
                if *count == 0 {
                    held.blobs.remove(digest);
                }
            }
            held.heads.insert(workspace.to_owned(), head);
            Ok(change.version)
        });
        Box::pin(std::future::ready(result))
    }

    fn blob<'a>(&'a self, digest: &'a Digest) -> StoreFuture<'a, Result<Blob, Failure>> {
        let blob = self
            .held
            .read(|held| held.blobs.get(digest).map(|(blob, _)| blob.clone()))
            .ok_or(Failure::Missing(*digest));
        Box::pin(std::future::ready(blob))
    }
}
