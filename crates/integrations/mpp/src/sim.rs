use std::sync::atomic::{AtomicU8, Ordering};

use nanocode_tempo::Address;

use super::session::wire::ChannelDescriptor;
use super::store::{ChannelStore, MemoryChannelStore, StoreFault, StoreLease, StoredChannelEntry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    SpentSaveRegresses,
    LeasesKept,
}

static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);

pub fn mutate(mutation: Mutation) {
    MUTATION.store(mutation as u8, Ordering::SeqCst);
}

pub fn mutated(mutation: Mutation) -> bool {
    MUTATION.load(Ordering::Relaxed) == mutation as u8
}

pub async fn open_channel(
    store: &ChannelStore,
    channel_id: [u8; 32],
    cumulative_amount: u128,
) -> Result<String, StoreFault> {
    let entry = StoredChannelEntry {
        channel_id,
        cumulative_amount,
        deposit: cumulative_amount,
        descriptor: ChannelDescriptor {
            payer: String::new(),
            payee: String::new(),
            operator: String::new(),
            token: String::new(),
            salt: String::new(),
            authorized_signer: String::new(),
            expiring_nonce_hash: String::new(),
        },
        settlement_route: None,
        escrow: Address::ZERO,
        chain_id: 0,
        opened: true,
        pending_open: None,
        origin: None,
    };
    store.set(&entry).await?;
    Ok(entry.key())
}

pub async fn save_server_spent(
    store: &ChannelStore,
    key: &str,
    channel_id: &[u8; 32],
    spent: u128,
) -> Result<bool, StoreFault> {
    store.set_server_spent(key, channel_id, spent).await
}

pub async fn server_spent(
    store: &ChannelStore,
    key: &str,
    channel_id: &[u8; 32],
) -> Result<Option<u128>, StoreFault> {
    store.server_spent(key, channel_id).await
}

pub struct Lease {
    _lease: StoreLease,
}

pub async fn hold_lease(store: &ChannelStore, key: &str) -> Result<Lease, StoreFault> {
    store
        .acquire(key)
        .await
        .map(|lease| Lease { _lease: lease })
}

pub fn leases(store: &MemoryChannelStore) -> usize {
    store.lease_count()
}
