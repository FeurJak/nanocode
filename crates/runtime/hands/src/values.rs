use std::fmt;

use nanocode_cost_fold::Placement;
use nanocode_platform::MaybeSend;
use nanocode_tool_contracts::ToolOutput;

use crate::{HandRef, HostFuture};

pub const SHA256_BYTES: usize = 32;

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha256Digest(pub [u8; SHA256_BYTES]);

impl Sha256Digest {
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(nanocode_digest::Sha256::digest(bytes))
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HandPhase {
    Stopped,
    Starting { since_ms: u64 },
    Running { since_ms: u64 },
    Releasing { since_ms: u64 },
    Lost { reason: Box<str> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandStatus {
    pub hand: HandRef,
    pub tools: Vec<String>,
    pub phase: HandPhase,
}

pub enum StepOutcome {
    Done {
        output: ToolOutput,
        hand_ms: u64,
        usd: Option<u64>,
    },
    Uncertain {
        hand: HandRef,
        reason: Box<str>,
        hand_ms: u64,
        usd: Option<u64>,
    },
    Refused {
        reason: Box<str>,
    },
}

impl StepOutcome {
    #[must_use]
    pub fn placement(&self, hand: &HandRef) -> Option<Placement> {
        match self {
            Self::Done { hand_ms, usd, .. } | Self::Uncertain { hand_ms, usd, .. } => {
                Some(Placement {
                    hand: hand.to_string(),
                    hand_ms: *hand_ms,
                    hand_nano_usd: *usd,
                })
            }
            Self::Refused { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stored {
    pub size_bytes: u64,
    pub sha256: Sha256Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HandError {
    pub reason: Box<str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChunkError {
    Lost {
        reason: Box<str>,
    },
    Io {
        reason: Box<str>,
    },
    TooLarge {
        bytes: usize,
        bytes_max: usize,
    },
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    ShaMismatch {
        expected: Sha256Digest,
        actual: Sha256Digest,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PutError {
    Refused {
        reason: Box<str>,
    },
    Io {
        reason: Box<str>,
    },
    Source(ChunkError),
    ShaMismatch {
        expected: Sha256Digest,
        actual: Sha256Digest,
    },
    Uncertain {
        reason: Box<str>,
    },
    TooLarge {
        bytes_max: u64,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GetError {
    Refused { reason: Box<str> },
    NotFound,
    Io { reason: Box<str> },
    Lost { reason: Box<str> },
}

pub trait Chunks: MaybeSend {
    fn next(&mut self) -> HostFuture<'_, Result<Option<Vec<u8>>, ChunkError>>;
}

pub struct Fetch<'a> {
    pub size_bytes: u64,
    pub sha256: Sha256Digest,
    pub chunks: Box<dyn Chunks + 'a>,
}

impl fmt::Display for ChunkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lost { reason } => write!(formatter, "the source was lost mid-copy: {reason}"),
            Self::Io { reason } => write!(formatter, "reading the source failed: {reason}"),
            Self::TooLarge { bytes, bytes_max } => write!(
                formatter,
                "a chunk of {bytes} bytes exceeds the {bytes_max}-byte chunk size"
            ),
            Self::SizeMismatch { expected, actual } => write!(
                formatter,
                "the source sent {actual} bytes, not the {expected} it declared"
            ),
            Self::ShaMismatch { expected, actual } => write!(
                formatter,
                "the source's bytes hash to {actual}, not the declared {expected}"
            ),
        }
    }
}

impl fmt::Display for PutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { reason } => {
                write!(formatter, "the destination refused the file: {reason}")
            }
            Self::Io { reason } => write!(formatter, "writing the destination failed: {reason}"),
            Self::Source(error) => write!(formatter, "{error}; the destination is unchanged"),
            Self::ShaMismatch { expected, actual } => write!(
                formatter,
                "the bytes received hash to {actual}, not {expected}; the destination is unchanged"
            ),
            Self::Uncertain { reason } => write!(
                formatter,
                "the destination holds either its old or its new file, not known which: {reason}"
            ),
            Self::TooLarge { bytes_max } => write!(
                formatter,
                "the file exceeds the destination's {bytes_max}-byte limit; the destination is unchanged"
            ),
        }
    }
}

impl fmt::Display for GetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { reason } => write!(formatter, "the source refused the read: {reason}"),
            Self::NotFound => formatter.write_str("the source file does not exist"),
            Self::Io { reason } => write!(formatter, "reading the source failed: {reason}"),
            Self::Lost { reason } => write!(formatter, "the source Hand was lost: {reason}"),
        }
    }
}

impl fmt::Display for HandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for ChunkError {}
impl std::error::Error for PutError {}
impl std::error::Error for GetError {}
impl std::error::Error for HandError {}
