use std::{fmt, num::NonZeroUsize};

use crate::{
    ChunkError, Chunks, Fetch, GetError, HandRef, HostFuture, PutError, Stored, Target,
    router::Router,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CopyError {
    Get(GetError),
    Put(PutError),
}

impl fmt::Display for CopyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Get(error) => write!(formatter, "{error}; nothing was copied"),
            Self::Put(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CopyError {}

struct Bounded<'a> {
    inner: Box<dyn Chunks + 'a>,
    bytes_max: usize,
}

impl Chunks for Bounded<'_> {
    fn next(&mut self) -> HostFuture<'_, Result<Option<Vec<u8>>, ChunkError>> {
        Box::pin(async move {
            let chunk = self.inner.next().await?;
            match chunk {
                Some(chunk) if chunk.len() > self.bytes_max => Err(ChunkError::TooLarge {
                    bytes: chunk.len(),
                    bytes_max: self.bytes_max,
                }),
                chunk => Ok(chunk),
            }
        })
    }
}

impl Router {
    pub async fn copy(
        &self,
        session: &str,
        from: (&Target, &str),
        to: (&Target, &str),
        chunk_bytes: NonZeroUsize,
    ) -> Result<Stored, CopyError> {
        let hands: Vec<&HandRef> = [from.0, to.0]
            .into_iter()
            .filter_map(|target| match target {
                Target::Workspace => None,
                Target::Hand(hand) => Some(hand),
            })
            .collect();
        for hand in &hands {
            self.hold(session, hand).await;
        }
        let copied = self.copy_held(session, from, to, chunk_bytes).await;
        for hand in &hands {
            self.unhold(session, hand);
        }
        copied
    }

    async fn copy_held(
        &self,
        session: &str,
        (from, from_path): (&Target, &str),
        (to, to_path): (&Target, &str),
        chunk_bytes: NonZeroUsize,
    ) -> Result<Stored, CopyError> {
        let fetch: Fetch<'_> = match from {
            Target::Workspace => self.workspace.get(session, from_path, chunk_bytes).await,
            Target::Hand(hand) => self.host.get(session, hand, from_path, chunk_bytes).await,
        }
        .map_err(CopyError::Get)?;
        let chunks = Box::new(Bounded {
            inner: fetch.chunks,
            bytes_max: chunk_bytes.get(),
        });
        match to {
            Target::Workspace => {
                self.workspace
                    .put(session, to_path, chunks, fetch.sha256)
                    .await
            }
            Target::Hand(hand) => {
                self.host
                    .put(session, hand, to_path, chunks, fetch.sha256)
                    .await
            }
        }
        .map_err(CopyError::Put)
    }
}
