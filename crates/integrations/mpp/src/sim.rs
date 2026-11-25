use std::sync::atomic::{AtomicU8, Ordering};

use nanocode_tempo::Address;

use super::session::wire::ChannelDescriptor;
use super::store::{ChannelStore, StoreFault, StoredChannelEntry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mutation {
    None,
    SpentSaveRegresses,
    ReserveSkipsCompare,
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
    let entry = entry(channel_id, cumulative_amount);
    store.set(&entry).await?;
    Ok(entry.key())
}

fn entry(channel_id: [u8; 32], cumulative_amount: u128) -> StoredChannelEntry {
    StoredChannelEntry {
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
    }
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

pub async fn cumulative(store: &ChannelStore, key: &str) -> Result<Option<u128>, StoreFault> {
    Ok(store.get(key).await?.map(|entry| entry.cumulative_amount))
}

pub async fn reserve(
    store: &ChannelStore,
    channel_id: [u8; 32],
    base: u128,
    cumulative_amount: u128,
) -> Result<bool, StoreFault> {
    store
        .reserve(&entry(channel_id, cumulative_amount), base)
        .await
}
