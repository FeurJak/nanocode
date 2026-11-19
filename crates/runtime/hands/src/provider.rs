use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use nanocode_sync::Atom;

use crate::{
    Clock, HostFuture,
    sim::{self, Mutation},
};

const DEFAULT: &str = "default";
const LARGE: &str = "large";
const MEMORY_PREFIX: &str = "memory-";

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HandSize {
    Default,
    Large,
}

impl HandSize {
    pub const ALL: [Self; 2] = [Self::Default, Self::Large];

    pub fn parse(wire: &str) -> Result<Self, ProviderError> {
        match wire {
            DEFAULT => Ok(Self::Default),
            LARGE => Ok(Self::Large),
            _ => Err(ProviderError::UnknownSize { wire: wire.into() }),
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Default => DEFAULT,
            Self::Large => LARGE,
        }
    }
}

impl fmt::Display for HandSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProvidedState {
    Starting,
    Running,
    Stopping,
    Lost { reason: Box<str> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvidedHand {
    pub id: Box<str>,
    pub size: HandSize,
    pub state: ProvidedState,
    pub started_ms: u64,
    pub nano_usd_per_hour: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderError {
    UnknownSize { wire: Box<str> },
    UnknownHand { id: Box<str> },
    Exhausted { hands_max: usize },
    Unavailable { reason: Box<str> },
    Uncertain { reason: Box<str> },
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSize { wire } => {
                write!(
                    formatter,
                    "`{wire}` is not a Hand size: use {DEFAULT} or {LARGE}"
                )
            }
            Self::UnknownHand { id } => write!(formatter, "no provided Hand has id `{id}`"),
            Self::Exhausted { hands_max } => {
                write!(formatter, "the provider already runs its {hands_max} Hands")
            }
            Self::Unavailable { reason } => {
                write!(formatter, "the provider is unavailable: {reason}")
            }
            Self::Uncertain { reason } => write!(
                formatter,
                "the provider may or may not have done it, not known which: {reason}"
            ),
        }
    }
}

impl std::error::Error for ProviderError {}

pub trait HandProvider: Send + Sync + 'static {
    fn start(&self, size: HandSize) -> HostFuture<'_, Result<ProvidedHand, ProviderError>>;

    fn stop<'a>(&'a self, id: &'a str) -> HostFuture<'a, Result<(), ProviderError>>;

    fn list(&self) -> HostFuture<'_, Result<Vec<ProvidedHand>, ProviderError>>;
}

pub struct MemoryProvider {
    clock: Arc<dyn Clock>,
    hands_max: usize,
    nano_usd_per_hour: [u64; HandSize::ALL.len()],
    started: AtomicU64,
    hands: Atom<BTreeMap<Box<str>, ProvidedHand>>,
}

impl MemoryProvider {
    #[must_use]
    pub fn new(
        clock: Arc<dyn Clock>,
        hands_max: usize,
        nano_usd_per_hour: [u64; HandSize::ALL.len()],
    ) -> Self {
        Self {
            clock,
            hands_max,
            nano_usd_per_hour,
            started: AtomicU64::new(0),
            hands: Atom::new(BTreeMap::new()),
        }
    }

    fn start_now(&self, size: HandSize) -> Result<ProvidedHand, ProviderError> {
        let started_ms = self.clock.now_ms();
        let nano_usd_per_hour = self.nano_usd_per_hour[size as usize];
        let hands_max = self.hands_max;
        let started = &self.started;
        let hand = self.hands.modify(|hands| {
            if hands.len() >= hands_max {
                return Err(ProviderError::Exhausted { hands_max });
            }
            let number = started.fetch_add(1, Ordering::SeqCst);
            let hand = ProvidedHand {
                id: format!("{MEMORY_PREFIX}{number}").into(),
                size,
                state: ProvidedState::Running,
                started_ms,
                nano_usd_per_hour,
            };
            let previous = hands.insert(hand.id.clone(), hand.clone());
            assert!(previous.is_none(), "a provided Hand id is never reused");
            Ok(hand)
        })?;
        assert_eq!(hand.size, size);
        Ok(hand)
    }

    fn stop_now(&self, id: &str) -> Result<(), ProviderError> {
        if sim::mutated(Mutation::StopsUnknown) {
            self.hands.update(|hands| hands.remove(id));
            return Ok(());
        }
        if sim::mutated(Mutation::ListKeepsStopped) {
            return self.hands.read(|hands| {
                hands
                    .contains_key(id)
                    .then_some(())
                    .ok_or_else(|| ProviderError::UnknownHand { id: id.into() })
            });
        }
        self.hands.modify(|hands| {
            hands
                .remove(id)
                .map(|_| ())
                .ok_or_else(|| ProviderError::UnknownHand { id: id.into() })
        })
    }
}

impl HandProvider for MemoryProvider {
    fn start(&self, size: HandSize) -> HostFuture<'_, Result<ProvidedHand, ProviderError>> {
        Box::pin(async move { self.start_now(size) })
    }

    fn stop<'a>(&'a self, id: &'a str) -> HostFuture<'a, Result<(), ProviderError>> {
        Box::pin(async move { self.stop_now(id) })
    }

    fn list(&self) -> HostFuture<'_, Result<Vec<ProvidedHand>, ProviderError>> {
        Box::pin(async move { Ok(self.hands.read(|hands| hands.values().cloned().collect())) })
    }
}
