use std::{collections::BTreeMap, sync::Arc};

use nanocode_digest::blake3;
use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_sync::Atom;

use crate::{
    Board, BoardError, Op, Rejection,
    messages::{FILE_BYTES, Refusal, Say},
};

pub trait FileStore: Send + Sync {
    fn put<'a>(&'a self, hash: &'a str, bytes: &'a [u8])
    -> StoreFuture<'a, Result<(), StoreError>>;
    fn get<'a>(&'a self, hash: &'a str) -> StoreFuture<'a, Result<Option<Vec<u8>>, StoreError>>;
}

#[derive(Default)]
pub struct MemoryFiles {
    files: Atom<BTreeMap<String, Arc<[u8]>>>,
}

impl MemoryFiles {
    pub fn new() -> Self {
        Self::default()
    }
}

impl FileStore for MemoryFiles {
    fn put<'a>(
        &'a self,
        hash: &'a str,
        bytes: &'a [u8],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.files.update(|files| {
                files
                    .entry(hash.to_owned())
                    .or_insert_with(|| Arc::from(bytes));
            });
            Ok(())
        })
    }

    fn get<'a>(&'a self, hash: &'a str) -> StoreFuture<'a, Result<Option<Vec<u8>>, StoreError>> {
        Box::pin(async move {
            Ok(self
                .files
                .read(|files| files.get(hash).map(|bytes| bytes.to_vec())))
        })
    }
}

pub fn hash(bytes: &[u8]) -> String {
    blake3(bytes).to_hex().as_str().to_owned()
}

impl Board {
    pub async fn upload(
        &mut self,
        files: &dyn FileStore,
        at_ms: u64,
        bytes: &[u8],
        media: &str,
    ) -> Result<String, BoardError> {
        let size = bytes.len() as u64;
        if size > FILE_BYTES {
            return Err(BoardError::Rejected(Rejection::Said(Refusal::FileSize(
                size,
            ))));
        }
        let hash = hash(bytes);
        files.put(&hash, bytes).await?;
        let say = Say::File {
            hash: hash.clone(),
            size,
            media: media.to_owned(),
        };
        self.submit_keyed(&format!("file:{hash}"), at_ms, Op::Say { say })
            .await?;
        Ok(hash)
    }

    pub async fn download(
        &self,
        files: &dyn FileStore,
        hash: &str,
    ) -> Result<Option<Vec<u8>>, BoardError> {
        let Some(file) = self.fold.file(hash) else {
            return Ok(None);
        };
        let Some(bytes) = files.get(hash).await? else {
            return Ok(None);
        };
        if bytes.len() as u64 != file.size || self::hash(&bytes) != hash {
            return Err(BoardError::Corrupt {
                seq: file.seq,
                detail: format!("the stored bytes of {hash} do not match it"),
            });
        }
        Ok(Some(bytes))
    }
}
