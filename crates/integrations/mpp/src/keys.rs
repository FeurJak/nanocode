use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering},
    },
};

use nanocode_sync::Atom;
use nanocode_tempo::{
    Address, CallScope, ClientData, KeyAuthorization, KeychainSignature, P256Signature,
    PrimitiveSignature, SelectorRule, SignatureType, SignedKeyAuthorization, SigningKey,
    TempoSignature, TempoTransaction, sha256,
};
use serde::Deserialize;

use super::accounts::{
    self, AccountsError, PersistedCredentials, PersistedKeyError, PersistedKeyType, RpcCallScope,
    RpcKeyAuthorization,
};
use super::rpc::{Rpc, RpcFault};
use super::tx::{self, TempoTransactionRequest};

const WORD: usize = 32;
const SELECTOR: usize = 4;
const ADDRESS_OFFSET: usize = WORD - nanocode_tempo::ADDRESS_BYTES;
const TIP20_TOKEN_PREFIX: [u8; 12] = [0x20, 0xc0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
const TRANSFER_SELECTOR: &str = "transfer(address,uint256)";
const APPROVE_SELECTOR: &str = "approve(address,uint256)";
const TRANSFER_WITH_MEMO_SELECTOR: &str = "transferWithMemo(address,uint256,bytes32)";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum P256SignerError {
    InvalidPrivateKey,
}

impl fmt::Display for P256SignerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPrivateKey => f.write_str("invalid P-256 private key"),
        }
    }
}

impl std::error::Error for P256SignerError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyError {
    ChainMismatch {
        expected: u64,
        actual: u64,
    },
    SenderMismatch {
        expected: Address,
        actual: Address,
    },
    KeyMismatch {
        expected: Address,
        actual: Address,
    },
    SignatureTypeMismatch {
        expected: SignatureType,
        actual: SignatureType,
    },
    AuthorizationMismatch,
    InvalidAuthorization(&'static str),
    AuthorizationInFlight {
        chain_id: u64,
        account: Address,
        key_id: Address,
    },
    ReservationStateUnavailable,
    Signing(String),
    Build(String),
    Accounts(String),
}

const fn signature_type_debug(kind: SignatureType) -> &'static str {
    match kind {
        SignatureType::Secp256k1 => "Secp256k1",
        SignatureType::P256 => "P256",
        SignatureType::WebAuthn => "WebAuthn",
    }
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChainMismatch { expected, actual } => {
                write!(
                    f,
                    "Tempo access-key chain mismatch: expected {expected}, got {actual}"
                )
            }
            Self::SenderMismatch { expected, actual } => {
                write!(
                    f,
                    "Tempo access-key sender mismatch: expected {expected}, got {actual}"
                )
            }
            Self::KeyMismatch { expected, actual } => {
                write!(
                    f,
                    "Tempo access-key mismatch: expected {expected}, got {actual}"
                )
            }
            Self::SignatureTypeMismatch { expected, actual } => write!(
                f,
                "Tempo signature type mismatch: expected {}, got {}",
                signature_type_debug(*expected),
                signature_type_debug(*actual)
            ),
            Self::AuthorizationMismatch => f.write_str(
                "request key authorization does not match the selected Tempo ID access key",
            ),
            Self::InvalidAuthorization(reason) => {
                write!(f, "invalid pending Tempo key authorization: {reason}")
            }
            Self::AuthorizationInFlight {
                chain_id,
                account,
                key_id,
            } => write!(
                f,
                "Tempo key authorization for {key_id} on account {account} and chain {chain_id} is already in flight"
            ),
            Self::ReservationStateUnavailable => {
                f.write_str("Tempo key-authorization reservation state is unavailable")
            }
            Self::Signing(message) | Self::Build(message) | Self::Accounts(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for KeyError {}

#[derive(Debug, Clone)]
pub(crate) enum PrepareError {
    Rpc(RpcFault),
    Local(String),
}

impl PrepareError {
    fn local(error: impl fmt::Display) -> Self {
        Self::Local(error.to_string())
    }
}

impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(error) => write!(f, "{error}"),
            Self::Local(error) => write!(f, "local usage error: {error}"),
        }
    }
}

impl std::error::Error for PrepareError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAuthorizationError {
    MissingAccount,
    AuthorizationExpired,
    AuthorizationChainMismatch { expected: u64, actual: u64 },
    AuthorizationTypeMismatch,
    AuthorizationAccountMismatch { expected: Address, actual: Address },
    UnexpectedOnchainKey { expected: Address, actual: Address },
    Revoked { account: Address, key_id: Address },
    Expired { account: Address, key_id: Address },
}

impl fmt::Display for KeyAuthorizationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingAccount => {
                f.write_str("Tempo access-key authorization requires a transaction sender")
            }
            Self::AuthorizationExpired => f.write_str("Tempo key authorization is expired"),
            Self::AuthorizationChainMismatch { expected, actual } => {
                write!(
                    f,
                    "Tempo key authorization chain is {actual}, expected {expected}"
                )
            }
            Self::AuthorizationTypeMismatch => f.write_str(
                "Tempo key authorization signature type does not match the selected signer",
            ),
            Self::AuthorizationAccountMismatch { expected, actual } => {
                write!(
                    f,
                    "Tempo key authorization account is {actual}, expected {expected}"
                )
            }
            Self::UnexpectedOnchainKey { expected, actual } => {
                write!(
                    f,
                    "Account Keychain returned key {actual}, expected {expected}"
                )
            }
            Self::Revoked { account, key_id } => {
                write!(
                    f,
                    "Tempo access key {key_id} for {account} has been revoked"
                )
            }
            Self::Expired { account, key_id } => {
                write!(f, "Tempo access key {key_id} for {account} has expired")
            }
        }
    }
}

#[derive(Clone)]
pub(crate) enum KeySigner {
    Secp256k1(SigningKey),
    P256 { key: SigningKey, pre_hash: bool },
}

impl fmt::Debug for KeySigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeySigner")
            .field("address", &self.address())
            .field("signature_type", &self.signature_type())
            .finish_non_exhaustive()
    }
}

impl KeySigner {
    pub(crate) fn secp256k1(secret: &[u8]) -> Result<Self, PersistedKeyError> {
        SigningKey::secp256k1(secret)
            .map(Self::Secp256k1)
            .map_err(|_| PersistedKeyError::InvalidPrivateKey)
    }

    pub(crate) fn p256(secret: &[u8], pre_hash: bool) -> Result<Self, P256SignerError> {
        SigningKey::p256(secret)
            .map(|key| Self::P256 { key, pre_hash })
            .map_err(|_| P256SignerError::InvalidPrivateKey)
    }

    pub(crate) const fn signature_type(&self) -> SignatureType {
        match self {
            Self::Secp256k1(_) => SignatureType::Secp256k1,
            Self::P256 { .. } => SignatureType::P256,
        }
    }

    pub(crate) fn address(&self) -> Address {
        match self {
            Self::Secp256k1(key) | Self::P256 { key, .. } => key.address(),
        }
    }

    pub(crate) fn sign_hash(&self, hash: &[u8; WORD]) -> Result<PrimitiveSignature, KeyError> {
        match self {
            Self::Secp256k1(key) => key
                .sign_prehash(hash)
                .map(PrimitiveSignature::Secp256k1)
                .map_err(|error| KeyError::Signing(error.to_string())),
            Self::P256 { key, pre_hash } => {
                let digest = match pre_hash {
                    true => sha256(hash),
                    false => *hash,
                };
                let signature = key
                    .sign_prehash(&digest)
                    .map_err(|error| KeyError::Signing(error.to_string()))?;
                let (x, y) = key.public_key();
                Ok(PrimitiveSignature::P256(P256Signature {
                    r: signature.r,
                    s: signature.s,
                    public_x: *x,
                    public_y: *y,
                    pre_hash: *pre_hash,
                }))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
struct AuthorizationIdentity {
    chain_id: u64,
    account: Address,
    key_id: Address,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct AuthorizationReservation {
    chain_id: u64,
    account: Address,
    key_id: Address,
    generation: u64,
}

impl AuthorizationReservation {
    const fn identity(&self) -> AuthorizationIdentity {
        AuthorizationIdentity {
            chain_id: self.chain_id,
            account: self.account,
            key_id: self.key_id,
        }
    }

    pub(crate) const fn account(&self) -> Address {
        self.account
    }

    pub(crate) const fn key_id(&self) -> Address {
        self.key_id
    }
}

#[derive(Clone, Default)]
struct ReservationState {
    next_generation: u64,
    reservations: BTreeMap<AuthorizationIdentity, u64>,
    observed: BTreeMap<u64, BTreeSet<AuthorizationReservation>>,
}

static RESERVATIONS: LazyLock<Atom<ReservationState>> = LazyLock::new(Atom::default);
static OWNERS: AtomicU64 = AtomicU64::new(0);

struct Owner(u64);

impl Drop for Owner {
    fn drop(&mut self) {
        let owner = self.0;
        RESERVATIONS.update(|state| {
            state.observed.remove(&owner);
        });
    }
}

#[derive(Clone)]
struct Reservations(Arc<Owner>);

impl Default for Reservations {
    fn default() -> Self {
        Self(Arc::new(Owner(OWNERS.fetch_add(1, Ordering::Relaxed))))
    }
}

impl Reservations {
    fn owner(&self) -> u64 {
        self.0.0
    }

    fn reserve(&self, identity: AuthorizationIdentity) -> Result<(), KeyError> {
        let owner = self.owner();
        RESERVATIONS.modify(|state| {
            if state.reservations.contains_key(&identity) {
                return Err(KeyError::AuthorizationInFlight {
                    chain_id: identity.chain_id,
                    account: identity.account,
                    key_id: identity.key_id,
                });
            }
            let generation = state
                .next_generation
                .checked_add(1)
                .ok_or(KeyError::ReservationStateUnavailable)?;
            state.next_generation = generation;
            state.reservations.insert(identity, generation);
            state
                .observed
                .entry(owner)
                .or_default()
                .insert(AuthorizationReservation {
                    chain_id: identity.chain_id,
                    account: identity.account,
                    key_id: identity.key_id,
                    generation,
                });
            Ok(())
        })
    }

    fn release_owned(&self, reservation: AuthorizationReservation) {
        let owner = self.owner();
        RESERVATIONS.update(|state| {
            let removed = state
                .observed
                .get_mut(&owner)
                .is_some_and(|observed| observed.remove(&reservation));
            if removed
                && state.reservations.get(&reservation.identity()) == Some(&reservation.generation)
            {
                state.reservations.remove(&reservation.identity());
            }
        });
    }

    fn resolve_on_chain(&self, identity: AuthorizationIdentity) {
        let owner = self.owner();
        RESERVATIONS.update(|state| {
            state.reservations.remove(&identity);
            if let Some(observed) = state.observed.get_mut(&owner) {
                observed.retain(|reservation| reservation.identity() != identity);
            }
        });
    }

    fn in_flight(&self) -> Vec<AuthorizationReservation> {
        let owner = self.owner();
        RESERVATIONS.update(|state| {
            let ReservationState {
                reservations,
                observed,
                ..
            } = state;
            match observed.get_mut(&owner) {
                Some(observed) => {
                    observed.retain(|reservation| {
                        reservations.get(&reservation.identity()) == Some(&reservation.generation)
                    });
                    observed.iter().copied().collect()
                }
                None => Vec::new(),
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SignedTransaction {
    pub(crate) transaction: TempoTransaction,
    pub(crate) signature: TempoSignature,
}

impl SignedTransaction {
    pub(crate) fn encoded_2718(&self) -> Vec<u8> {
        self.transaction.encode_signed(&self.signature)
    }
}

#[derive(Clone)]
pub(crate) struct AccessKey {
    account: Address,
    address: Address,
    chain_id: u64,
    signer: KeySigner,
    key_authorization: Option<Box<SignedKeyAuthorization>>,
    reservations: Reservations,
}

impl fmt::Debug for AccessKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessKey")
            .field("account", &self.account)
            .field("address", &self.address)
            .field("chain_id", &self.chain_id)
            .finish_non_exhaustive()
    }
}

impl AccessKey {
    pub(crate) const fn account(&self) -> Address {
        self.account
    }

    pub(crate) const fn address(&self) -> Address {
        self.address
    }

    pub(crate) const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub(crate) const fn signature_type(&self) -> SignatureType {
        self.signer.signature_type()
    }

    pub(crate) fn key_authorization(&self) -> Option<&SignedKeyAuthorization> {
        self.key_authorization.as_deref()
    }

    pub(crate) fn sign_hash(&self, hash: &[u8; WORD]) -> Result<PrimitiveSignature, KeyError> {
        self.signer.sign_hash(hash)
    }

    pub(crate) fn validate_pending_authorization(&self) -> Result<(), KeyError> {
        let Some(signed) = self.key_authorization.as_deref() else {
            return Ok(());
        };
        let authorization = &signed.authorization;
        if authorization.key_id != self.address {
            return Err(KeyError::InvalidAuthorization(
                "key ID does not match the selected signer",
            ));
        }
        if authorization.key_type != self.signature_type() {
            return Err(KeyError::InvalidAuthorization(
                "key type does not match the selected signer",
            ));
        }
        if authorization.chain_id != self.chain_id {
            return Err(KeyError::InvalidAuthorization(
                "chain ID does not match the selected key",
            ));
        }
        if authorization
            .account
            .is_some_and(|account| account != self.account)
        {
            return Err(KeyError::InvalidAuthorization(
                "account does not match the selected key",
            ));
        }
        if authorization.account.is_none() && recover_signer(signed) != Some(self.account) {
            return Err(KeyError::InvalidAuthorization(
                "signature does not recover the root account",
            ));
        }
        Ok(())
    }

    pub(crate) fn fill_request(
        &self,
        request: &mut TempoTransactionRequest,
    ) -> Result<(), KeyError> {
        self.validate_pending_authorization()?;
        if let Some(chain_id) = request.chain_id
            && chain_id != self.chain_id
        {
            return Err(KeyError::ChainMismatch {
                expected: self.chain_id,
                actual: chain_id,
            });
        }
        request.chain_id = Some(self.chain_id);
        if let Some(from) = request.from
            && from != self.account
        {
            return Err(KeyError::SenderMismatch {
                expected: self.account,
                actual: from,
            });
        }
        request.from = Some(self.account);
        if let Some(key_id) = request.key_id
            && key_id != self.address
        {
            return Err(KeyError::KeyMismatch {
                expected: self.address,
                actual: key_id,
            });
        }
        request.key_id = Some(self.address);
        let signature_type = self.signer.signature_type();
        if let Some(key_type) = request.key_type
            && key_type != signature_type
        {
            return Err(KeyError::SignatureTypeMismatch {
                expected: signature_type,
                actual: key_type,
            });
        }
        request.key_type = Some(signature_type);
        if let Some(expected) = self.key_authorization.as_deref() {
            if request
                .key_authorization
                .as_ref()
                .is_some_and(|actual| actual != expected)
            {
                return Err(KeyError::AuthorizationMismatch);
            }
            request.key_authorization = Some(expected.clone());
        }
        Ok(())
    }

    pub(crate) fn sign_aa(
        &self,
        sender: Address,
        mut transaction: TempoTransaction,
    ) -> Result<SignedTransaction, KeyError> {
        if transaction.chain_id != self.chain_id {
            return Err(KeyError::ChainMismatch {
                expected: self.chain_id,
                actual: transaction.chain_id,
            });
        }
        if sender != self.account {
            return Err(KeyError::SenderMismatch {
                expected: self.account,
                actual: sender,
            });
        }
        self.validate_pending_authorization()?;
        if let Some(expected) = self.key_authorization.as_deref() {
            if transaction
                .key_authorization
                .as_ref()
                .is_some_and(|actual| actual != expected)
            {
                return Err(KeyError::AuthorizationMismatch);
            }
            transaction.key_authorization = Some(expected.clone());
        }
        let signing_hash =
            KeychainSignature::signing_hash(&transaction.signature_hash(), &self.account);
        let primitive = self.signer.sign_hash(&signing_hash)?;
        match transaction.key_authorization.as_ref() {
            Some(authorization) => self.reservations.reserve(AuthorizationIdentity {
                chain_id: self.chain_id,
                account: self.account,
                key_id: authorization.authorization.key_id,
            })?,
            None => self.reservations.resolve_on_chain(AuthorizationIdentity {
                chain_id: self.chain_id,
                account: self.account,
                key_id: self.address,
            }),
        }
        Ok(SignedTransaction {
            transaction,
            signature: TempoSignature::Keychain(KeychainSignature {
                account: self.account,
                signature: primitive,
            }),
        })
    }

    pub(crate) fn sign_request(
        &self,
        mut request: TempoTransactionRequest,
    ) -> Result<SignedTransaction, KeyError> {
        self.fill_request(&mut request)?;
        let sender = request.from.unwrap_or(self.account);
        let transaction = tx::transaction_from_request(&request)
            .map_err(|missing| KeyError::Build(missing.to_owned()))?;
        self.sign_aa(sender, transaction)
    }
}

#[derive(Clone)]
enum Source {
    Store {
        path: PathBuf,
        fallback_account: Address,
    },
    Local {
        account: Address,
        signer: KeySigner,
        signer_chain_id: Option<u64>,
        key_authorization: Option<Box<SignedKeyAuthorization>>,
    },
}

#[derive(Clone)]
pub(crate) struct Accounts {
    source: Source,
    chain_id: Option<u64>,
    reservations: Reservations,
}

impl fmt::Debug for Accounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("Accounts");
        match &self.source {
            Source::Store {
                path,
                fallback_account,
            } => {
                debug
                    .field("source", &"store")
                    .field("path", path)
                    .field("fallback_account", fallback_account);
            }
            Source::Local {
                account, signer, ..
            } => {
                debug
                    .field("source", &"local")
                    .field("account", account)
                    .field("key_id", &signer.address());
            }
        }
        debug.field("chain_id", &self.chain_id).finish()
    }
}

impl Accounts {
    pub(crate) fn from_store(path: impl Into<PathBuf>) -> Result<Self, AccountsError> {
        let path = path.into();
        let fallback_account = accounts::load_credentials(&path)?.account.0;
        Ok(Self {
            source: Source::Store {
                path,
                fallback_account,
            },
            chain_id: None,
            reservations: Reservations::default(),
        })
    }

    pub(crate) fn from_default_store() -> Result<Self, AccountsError> {
        Self::from_store(accounts::default_credentials_path()?)
    }

    pub(crate) fn from_access_key(selected: AccessKey) -> Self {
        Self {
            chain_id: Some(selected.chain_id),
            reservations: selected.reservations,
            source: Source::Local {
                account: selected.account,
                signer: selected.signer,
                signer_chain_id: Some(selected.chain_id),
                key_authorization: selected.key_authorization,
            },
        }
    }

    pub(crate) const fn with_chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = Some(chain_id);
        self
    }

    pub(crate) const fn account(&self) -> Address {
        match &self.source {
            Source::Store {
                fallback_account, ..
            } => *fallback_account,
            Source::Local { account, .. } => *account,
        }
    }

    pub(crate) fn active_account(&self) -> Result<Address, AccountsError> {
        match &self.source {
            Source::Store { path, .. } => Ok(accounts::load_credentials(path)?.account.0),
            Source::Local { account, .. } => Ok(*account),
        }
    }

    pub(crate) fn authorization_reservation(&self) -> Option<AuthorizationReservation> {
        let Source::Local {
            account,
            key_authorization: Some(authorization),
            ..
        } = &self.source
        else {
            return None;
        };
        let identity = AuthorizationIdentity {
            chain_id: self.chain_id?,
            account: *account,
            key_id: authorization.authorization.key_id,
        };
        self.reservations
            .in_flight()
            .into_iter()
            .find(|reservation| reservation.identity() == identity)
    }

    pub(crate) fn release_authorization(&self, reservation: AuthorizationReservation) {
        self.reservations.release_owned(reservation);
    }

    fn local_key(
        &self,
        account: Address,
        chain_id: u64,
        signer: &KeySigner,
        key_authorization: &Option<Box<SignedKeyAuthorization>>,
    ) -> AccessKey {
        AccessKey {
            account,
            address: signer.address(),
            chain_id,
            signer: signer.clone(),
            key_authorization: key_authorization.clone(),
            reservations: self.reservations.clone(),
        }
    }

    pub(crate) fn active_access_key(&self) -> Result<AccessKey, AccountsError> {
        match &self.source {
            Source::Store { path, .. } => {
                let state = accounts::load_credentials(path)?;
                let account = state.account.0;
                let chain_id = self.chain_id.unwrap_or(state.chain_id);
                select_access_key(
                    &state,
                    account,
                    chain_id,
                    (None, None),
                    None,
                    unix_now(),
                    &self.reservations,
                )
            }
            Source::Local {
                account,
                signer,
                signer_chain_id,
                key_authorization,
            } => {
                let chain_id = self
                    .chain_id
                    .or(*signer_chain_id)
                    .ok_or(AccountsError::MissingChainId)?;
                Ok(self.local_key(*account, chain_id, signer, key_authorization))
            }
        }
    }

    fn select_for_request(
        &self,
        request: &TempoTransactionRequest,
    ) -> Result<AccessKey, AccountsError> {
        match &self.source {
            Source::Store { path, .. } => {
                let state = accounts::load_credentials(path)?;
                let account = request.from.unwrap_or(state.account.0);
                let chain_id = request
                    .chain_id
                    .or(self.chain_id)
                    .ok_or(AccountsError::MissingChainId)?;
                let calls = request_calls(request);
                select_access_key(
                    &state,
                    account,
                    chain_id,
                    (request.key_id, request.key_type),
                    calls.as_deref(),
                    unix_now(),
                    &self.reservations,
                )
            }
            Source::Local {
                account,
                signer,
                signer_chain_id,
                key_authorization,
            } => {
                let chain_id = request
                    .chain_id
                    .or(self.chain_id)
                    .or(*signer_chain_id)
                    .ok_or(AccountsError::MissingChainId)?;
                Ok(self.local_key(*account, chain_id, signer, key_authorization))
            }
        }
    }

    pub(crate) async fn prepare_request(
        &self,
        rpc: &Rpc,
        request: &mut TempoTransactionRequest,
    ) -> Result<Self, PrepareError> {
        if request.chain_id.is_none() {
            let chain_id = match self.chain_id {
                Some(chain_id) => chain_id,
                None => tx::chain_id(rpc).await.map_err(PrepareError::Rpc)?,
            };
            request.chain_id = Some(chain_id);
        }
        let (selected, key_authorization) = self.prepare_selected(rpc, request).await?;
        request.key_authorization = key_authorization;
        selected
            .fill_request(request)
            .map_err(PrepareError::local)?;
        Ok(Self::from_access_key(selected))
    }

    async fn prepare_selected(
        &self,
        rpc: &Rpc,
        request: &TempoTransactionRequest,
    ) -> Result<(AccessKey, Option<SignedKeyAuthorization>), PrepareError> {
        let mut selected = self
            .select_for_request(request)
            .map_err(PrepareError::local)?;
        let mut resolved = request.clone();
        selected
            .fill_request(&mut resolved)
            .map_err(PrepareError::local)?;
        let key_authorization = resolve_key_authorization(rpc, &resolved)
            .await?
            .unwrap_or(resolved.key_authorization);
        if selected.key_authorization.is_some() {
            selected.key_authorization = key_authorization.clone().map(Box::new);
        }
        Ok((selected, key_authorization))
    }

    fn fill_metadata(
        &self,
        request: &mut TempoTransactionRequest,
    ) -> Result<AccessKey, AccountsError> {
        if request.chain_id.is_none() {
            request.chain_id = Some(self.chain_id.ok_or(AccountsError::MissingChainId)?);
        }
        let selected = self.select_for_request(request)?;
        selected
            .fill_request(request)
            .map_err(|_| AccountsError::InvalidMetadata)?;
        Ok(selected)
    }

    const fn ensure_direct_signing_ready(&self, selected: &AccessKey) -> Result<(), AccountsError> {
        match matches!(&self.source, Source::Store { .. }) && selected.key_authorization.is_some() {
            true => Err(AccountsError::AuthorizationResolutionRequired),
            false => Ok(()),
        }
    }

    pub(crate) fn sign_request(
        &self,
        mut request: TempoTransactionRequest,
    ) -> Result<SignedTransaction, KeyError> {
        let selected = self
            .fill_metadata(&mut request)
            .map_err(|error| KeyError::Accounts(error.to_string()))?;
        self.ensure_direct_signing_ready(&selected)
            .map_err(|error| KeyError::Accounts(error.to_string()))?;
        selected.sign_request(request)
    }
}

pub(crate) async fn resolve_key_authorization(
    rpc: &Rpc,
    request: &TempoTransactionRequest,
) -> Result<Option<Option<SignedKeyAuthorization>>, PrepareError> {
    let Some(signed) = request.key_authorization.as_ref() else {
        return Ok(None);
    };
    let account = request
        .from
        .ok_or_else(|| PrepareError::local(KeyAuthorizationError::MissingAccount))?;
    validate_authorization(request, account, &signed.authorization).map_err(PrepareError::local)?;
    let Some(key_id) = request
        .key_id
        .filter(|key_id| *key_id == signed.authorization.key_id)
    else {
        return Ok(Some(Some(signed.clone())));
    };
    if let Some(key_type) = request.key_type
        && signed.authorization.key_type != key_type
    {
        return Err(PrepareError::local(
            KeyAuthorizationError::AuthorizationTypeMismatch,
        ));
    }
    let key = tx::get_key(rpc, account, key_id)
        .await
        .map_err(|error| match error {
            tx::CallError::Rpc(error) => PrepareError::Rpc(error),
            error => PrepareError::local(error),
        })?;
    if key.is_revoked {
        return Err(PrepareError::local(KeyAuthorizationError::Revoked {
            account,
            key_id,
        }));
    }
    if key.key_id == Address::ZERO {
        return Ok(Some(Some(signed.clone())));
    }
    if key.key_id != key_id {
        return Err(PrepareError::local(
            KeyAuthorizationError::UnexpectedOnchainKey {
                expected: key_id,
                actual: key.key_id,
            },
        ));
    }
    if key.expiry <= unix_now() {
        return Err(PrepareError::local(KeyAuthorizationError::Expired {
            account,
            key_id,
        }));
    }
    Ok(Some(None))
}

fn validate_authorization(
    request: &TempoTransactionRequest,
    account: Address,
    authorization: &KeyAuthorization,
) -> Result<(), KeyAuthorizationError> {
    if let Some(chain_id) = request.chain_id
        && authorization.chain_id != chain_id
    {
        return Err(KeyAuthorizationError::AuthorizationChainMismatch {
            expected: chain_id,
            actual: authorization.chain_id,
        });
    }
    if let Some(authorized) = authorization.account
        && authorized != account
    {
        return Err(KeyAuthorizationError::AuthorizationAccountMismatch {
            expected: account,
            actual: authorized,
        });
    }
    if authorization
        .expiry
        .is_some_and(|expiry| expiry <= unix_now())
    {
        return Err(KeyAuthorizationError::AuthorizationExpired);
    }
    Ok(())
}

#[derive(Deserialize)]
struct ClientDataJson<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    challenge: &'a str,
}

pub(crate) fn recover_primitive(
    signature: &PrimitiveSignature,
    hash: &[u8; WORD],
) -> Option<Address> {
    match signature {
        PrimitiveSignature::WebAuthn(webauthn) => {
            let json = webauthn.client_data_json().ok()?;
            let client: ClientDataJson<'_> = serde_json::from_slice(json).ok()?;
            webauthn
                .recover(
                    hash,
                    &ClientData {
                        kind: client.kind,
                        challenge: client.challenge,
                    },
                )
                .ok()
        }
        signature => signature.recover(hash).ok(),
    }
}

pub(crate) fn recover_signer(signed: &SignedKeyAuthorization) -> Option<Address> {
    recover_primitive(&signed.signature, &signed.authorization.signature_hash())
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[derive(Clone, Copy)]
struct IntentCall<'a> {
    to: Option<Address>,
    input: &'a [u8],
}

fn request_calls(request: &TempoTransactionRequest) -> Option<Vec<IntentCall<'_>>> {
    let mut calls: Vec<IntentCall<'_>> = request
        .calls
        .iter()
        .map(|call| IntentCall {
            to: Some(call.to),
            input: &call.input,
        })
        .collect();
    if let Some(to) = request.to {
        calls.push(IntentCall {
            to: Some(to),
            input: request.input.as_deref().unwrap_or_default(),
        });
    }
    (!calls.is_empty()).then_some(calls)
}

pub(crate) fn call_scopes_allow(
    scopes: Option<&[CallScope]>,
    target: &Address,
    input: &[u8],
) -> bool {
    (|| {
        let Some(scopes) = scopes else {
            return Some(true);
        };
        let scope = scopes.iter().find(|scope| scope.target == *target)?;
        if scope.selector_rules.is_empty() {
            return Some(true);
        }
        let selector: [u8; SELECTOR] = input.get(..SELECTOR)?.try_into().ok()?;
        let rule = scope
            .selector_rules
            .iter()
            .find(|rule| rule.selector == selector)?;
        if rule.recipients.is_empty() {
            return Some(true);
        }
        let word: [u8; WORD] = input.get(SELECTOR..SELECTOR + WORD)?.try_into().ok()?;
        if word[..ADDRESS_OFFSET].iter().any(|byte| *byte != 0) {
            return Some(false);
        }
        let recipient = Address::from_slice(&word[ADDRESS_OFFSET..]).ok()?;
        Some(rule.recipients.contains(&recipient))
    })()
    .unwrap_or(false)
}

fn scopes_match(scopes: Option<&[CallScope]>, calls: Option<&[IntentCall<'_>]>) -> bool {
    if scopes.is_none() {
        return true;
    }
    let Some(calls) = calls else {
        return false;
    };
    calls.iter().all(|call| {
        call.to
            .is_some_and(|target| call_scopes_allow(scopes, &target, call.input))
    })
}

fn is_tip20(address: &Address) -> bool {
    address.as_bytes().starts_with(&TIP20_TOKEN_PREFIX)
}

fn rpc_call_scopes(scopes: Vec<RpcCallScope>) -> Result<Vec<CallScope>, PersistedKeyError> {
    let recipient_selectors = [
        nanocode_tempo::abi::selector(TRANSFER_SELECTOR),
        nanocode_tempo::abi::selector(APPROVE_SELECTOR),
        nanocode_tempo::abi::selector(TRANSFER_WITH_MEMO_SELECTOR),
    ];
    let mut targets = BTreeSet::new();
    let mut grouped = Vec::with_capacity(scopes.len());
    for scope in scopes {
        let target = scope.target.0;
        if target.is_zero() || !targets.insert(target) {
            return Err(PersistedKeyError::InvalidCallScope);
        }
        let mut selectors = BTreeSet::new();
        let mut selector_rules = Vec::with_capacity(scope.selector_rules.len());
        for rule in scope.selector_rules {
            if !selectors.insert(rule.selector.0) {
                return Err(PersistedKeyError::InvalidCallScope);
            }
            let recipients: Vec<Address> = rule
                .recipients
                .into_iter()
                .map(|recipient| recipient.0)
                .collect();
            if !recipients.is_empty() {
                let mut unique = BTreeSet::new();
                if !is_tip20(&target)
                    || !recipient_selectors.contains(&rule.selector.0)
                    || recipients
                        .iter()
                        .any(|recipient| recipient.is_zero() || !unique.insert(*recipient))
                {
                    return Err(PersistedKeyError::InvalidRecipientConstraint);
                }
            }
            selector_rules.push(SelectorRule {
                selector: rule.selector.0,
                recipients,
            });
        }
        grouped.push(CallScope {
            target,
            selector_rules,
        });
    }
    Ok(grouped)
}

pub(crate) fn rpc_authorization(
    value: RpcKeyAuthorization,
) -> Result<SignedKeyAuthorization, PersistedKeyError> {
    let expiry = value
        .expiry
        .map(|expiry| match expiry.0 {
            0 => Err(PersistedKeyError::ZeroAuthorizationExpiry),
            expiry => Ok(expiry),
        })
        .transpose()?;
    let authorization = KeyAuthorization {
        chain_id: value.chain_id.0,
        key_type: value.key_type.signature_type()?,
        key_id: value.key_id.0,
        expiry,
        limits: value
            .limits
            .map(|limits| limits.into_iter().map(Into::into).collect()),
        allowed_calls: value.allowed_calls.map(rpc_call_scopes).transpose()?,
        witness: value.witness.map(|witness| witness.0),
        is_admin: false,
        account: value.account.map(|account| account.0),
    };
    Ok(SignedKeyAuthorization {
        authorization,
        signature: accounts::rpc_signature(&value.signature)?,
    })
}

fn credential_signer(credentials: &PersistedCredentials) -> Result<KeySigner, PersistedKeyError> {
    match credentials.key_type {
        PersistedKeyType::Secp256k1 => KeySigner::secp256k1(&credentials.private_key.0),
        PersistedKeyType::P256 => {
            KeySigner::p256(&credentials.private_key.0, false).map_err(PersistedKeyError::P256)
        }
        PersistedKeyType::WebAuthn | PersistedKeyType::Unsupported => {
            Err(PersistedKeyError::UnsupportedKeyType)
        }
    }
}

fn select_access_key(
    credentials: &PersistedCredentials,
    account: Address,
    chain_id: u64,
    requested_key: (Option<Address>, Option<SignatureType>),
    calls: Option<&[IntentCall<'_>]>,
    now: u64,
    reservations: &Reservations,
) -> Result<AccessKey, AccountsError> {
    let missing = AccountsError::MissingAccessKey { account, chain_id };
    let (preferred, signature_type) = requested_key;
    if credentials.account.0 != account || credentials.chain_id != chain_id {
        return Err(missing);
    }
    let Ok(signer) = credential_signer(credentials) else {
        return Err(missing);
    };
    let Ok(signed) = rpc_authorization(credentials.key_authorization.clone()) else {
        return Err(missing);
    };
    let authorization = &signed.authorization;
    if preferred.is_some_and(|preferred| preferred != signer.address())
        || signature_type.is_some_and(|expected| signer.signature_type() != expected)
        || authorization.key_id != signer.address()
        || authorization.chain_id != chain_id
        || authorization.key_type != signer.signature_type()
        || authorization.expiry.is_some_and(|expiry| expiry <= now)
        || authorization
            .account
            .is_some_and(|authorized| authorized != account)
        || (authorization.account.is_none() && recover_signer(&signed) != Some(account))
    {
        return Err(missing);
    }
    let should_match_scopes = preferred.is_none() || calls.is_some();
    if should_match_scopes && !scopes_match(authorization.allowed_calls.as_deref(), calls) {
        return Err(missing);
    }
    Ok(AccessKey {
        account,
        address: signer.address(),
        chain_id,
        signer,
        key_authorization: Some(Box::new(signed)),
        reservations: reservations.clone(),
    })
}
