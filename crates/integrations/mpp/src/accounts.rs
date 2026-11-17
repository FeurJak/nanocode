use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    str::FromStr,
};

use nanocode_tempo::{
    Address, P256Signature, PrimitiveSignature, Signature, SignatureType, U256, WebAuthnSignature,
    hex,
};
use serde::{Deserialize, Deserializer, de};
use serde_json::Value;

const NANOCODE_HOME: &str = "NANOCODE_HOME";
const DEFAULT_NANOCODE_HOME: &str = ".nanocode";
const CREDENTIALS: &str = "tempo-id/credentials.json";
const HEX_RADIX: u64 = 16;
const DECIMAL_RADIX: u64 = 10;
const WORD: usize = 32;
const SELECTOR: usize = 4;

#[derive(Debug)]
pub enum AccountsError {
    HomeUnavailable,
    Read {
        path: PathBuf,
        source: io::Error,
    },
    Decode {
        path: PathBuf,
        source: serde_json::Error,
    },
    MissingChainId,
    AuthorizationResolutionRequired,
    MissingAccessKey {
        account: Address,
        chain_id: u64,
    },
    InvalidMetadata,
}

impl fmt::Display for AccountsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeUnavailable => f.write_str("home directory is unavailable"),
            Self::Read { path, source } => write!(
                f,
                "no Tempo ID sign-in at {}: {source}; /login tempo signs in",
                path.display()
            ),
            Self::Decode { path, source } => write!(
                f,
                "invalid Tempo ID sign-in at {}: {source}",
                path.display()
            ),
            Self::MissingChainId => f.write_str("the Tempo ID access key needs a chain ID"),
            Self::AuthorizationResolutionRequired => f.write_str(
                "the Tempo ID access key has an unresolved authorization; call prepare_request or use the payer as a provider filler",
            ),
            Self::MissingAccessKey { account, chain_id } => write!(
                f,
                "the Tempo ID sign-in has no usable access key for account {account} on chain {chain_id}"
            ),
            Self::InvalidMetadata => f.write_str("invalid Tempo ID transaction metadata"),
        }
    }
}

impl std::error::Error for AccountsError {}

#[derive(Clone, Copy)]
pub(crate) struct Hex(pub(crate) Address);

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        Address::from_str(&text)
            .map(Self)
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Word(pub(crate) [u8; WORD]);

impl<'de> Deserialize<'de> for Word {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        hex::decode_array(&text)
            .map(Self)
            .map_err(de::Error::custom)
    }
}

#[derive(Clone)]
pub(crate) struct HexBytes(pub(crate) Vec<u8>);

impl<'de> Deserialize<'de> for HexBytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        hex::decode(&text).map(Self).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Quantity(pub(crate) U256);

impl<'de> Deserialize<'de> for Quantity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = Quantity;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a non-negative integer or hex quantity")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(Quantity(U256::from_u64(value)))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                let parsed = match value.strip_prefix("0x") {
                    Some(digits) => U256::from_str_radix(digits, HEX_RADIX),
                    None => U256::from_str_radix(value, DECIMAL_RADIX),
                };
                parsed.map(Quantity).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Quantity64(pub(crate) u64);

impl<'de> Deserialize<'de> for Quantity64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Quantity::deserialize(deserializer)?;
        value
            .0
            .to_u64()
            .map(Self)
            .ok_or_else(|| de::Error::custom("integer exceeds u64"))
    }
}

#[derive(Clone, Deserialize)]
pub(crate) struct PersistedCredentials {
    pub(crate) account: Hex,
    pub(crate) chain_id: u64,
    pub(crate) key_type: PersistedKeyType,
    pub(crate) private_key: Word,
    pub(crate) key_authorization: RpcKeyAuthorization,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub(crate) enum PersistedKeyType {
    #[serde(rename = "secp256k1")]
    Secp256k1,
    #[serde(rename = "p256")]
    P256,
    #[serde(rename = "webAuthn")]
    WebAuthn,
    #[serde(other)]
    Unsupported,
}

impl PersistedKeyType {
    pub(crate) const fn signature_type(self) -> Result<SignatureType, PersistedKeyError> {
        match self {
            Self::Secp256k1 => Ok(SignatureType::Secp256k1),
            Self::P256 => Ok(SignatureType::P256),
            Self::WebAuthn => Ok(SignatureType::WebAuthn),
            Self::Unsupported => Err(PersistedKeyError::UnsupportedKeyType),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PersistedSelector(pub(crate) [u8; SELECTOR]);

impl<'de> Deserialize<'de> for PersistedSelector {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        hex::decode_array(&encoded)
            .map(Self)
            .map_err(|_| de::Error::custom("call selector must be four-byte hex"))
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RpcKeyAuthorization {
    #[serde(default)]
    pub(crate) account: Option<Hex>,
    #[serde(default)]
    pub(crate) allowed_calls: Option<Vec<RpcCallScope>>,
    pub(crate) chain_id: Quantity64,
    #[serde(default)]
    pub(crate) expiry: Option<Quantity64>,
    pub(crate) key_id: Hex,
    pub(crate) key_type: PersistedKeyType,
    #[serde(default)]
    pub(crate) limits: Option<Vec<RpcTokenLimit>>,
    pub(crate) signature: Value,
    #[serde(default)]
    pub(crate) witness: Option<Word>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RpcCallScope {
    pub(crate) target: Hex,
    #[serde(default)]
    pub(crate) selector_rules: Vec<RpcSelectorRule>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct RpcSelectorRule {
    pub(crate) selector: PersistedSelector,
    #[serde(default)]
    pub(crate) recipients: Vec<Hex>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct RpcTokenLimit {
    pub(crate) token: Hex,
    pub(crate) limit: Quantity,
    #[serde(default)]
    pub(crate) period: Option<Quantity64>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum RpcPrimitiveSignature {
    #[serde(rename = "secp256k1")]
    Secp256k1 {
        r: Quantity,
        s: Quantity,
        #[serde(rename = "yParity")]
        y_parity: Quantity,
    },
    #[serde(rename = "p256")]
    P256 {
        r: Quantity,
        s: Quantity,
        #[serde(rename = "pubKeyX")]
        public_x: Quantity,
        #[serde(rename = "pubKeyY")]
        public_y: Quantity,
        #[serde(default, rename = "preHash")]
        pre_hash: bool,
    },
    #[serde(rename = "webAuthn")]
    WebAuthn {
        r: Quantity,
        s: Quantity,
        #[serde(rename = "pubKeyX")]
        public_x: Quantity,
        #[serde(rename = "pubKeyY")]
        public_y: Quantity,
        #[serde(rename = "webauthnData")]
        data: HexBytes,
    },
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PersistedKeyError {
    UnsupportedKeyType,
    InvalidPrivateKey,
    P256(super::keys::P256SignerError),
    UnsupportedAuthorizationSignature,
    ZeroAuthorizationExpiry,
    InvalidCallScope,
    InvalidRecipientConstraint,
    InvalidYParity,
}

impl fmt::Display for PersistedKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedKeyType => "unsupported access-key type",
            Self::InvalidPrivateKey => "invalid local private key",
            Self::P256(error) => return write!(f, "invalid P-256 key: {error}"),
            Self::UnsupportedAuthorizationSignature => {
                "unsupported key-authorization signature type"
            }
            Self::ZeroAuthorizationExpiry => "key-authorization expiry must be non-zero",
            Self::InvalidCallScope => "call-scope targets and selectors must be unique and nonzero",
            Self::InvalidRecipientConstraint => {
                "recipient constraints require a TIP-20 transfer or approve selector"
            }
            Self::InvalidYParity => "invalid secp256k1 y parity",
        })
    }
}

impl std::error::Error for PersistedKeyError {}

impl From<RpcTokenLimit> for nanocode_tempo::TokenLimit {
    fn from(value: RpcTokenLimit) -> Self {
        Self {
            token: value.token.0,
            limit: value.limit.0,
            period: value.period.map_or(0, |period| period.0),
        }
    }
}

fn parity(value: U256) -> Result<bool, PersistedKeyError> {
    match value {
        value if value == U256::ZERO => Ok(false),
        value if value == U256::ONE => Ok(true),
        _ => Err(PersistedKeyError::InvalidYParity),
    }
}

pub(crate) fn rpc_signature(value: &Value) -> Result<PrimitiveSignature, PersistedKeyError> {
    let signature = RpcPrimitiveSignature::deserialize(value)
        .map_err(|_| PersistedKeyError::UnsupportedAuthorizationSignature)?;
    match signature {
        RpcPrimitiveSignature::Secp256k1 { r, s, y_parity } => {
            Ok(PrimitiveSignature::Secp256k1(Signature {
                r: r.0.to_be_bytes(),
                s: s.0.to_be_bytes(),
                y_parity: parity(y_parity.0)?,
            }))
        }
        RpcPrimitiveSignature::P256 {
            r,
            s,
            public_x,
            public_y,
            pre_hash,
        } => Ok(PrimitiveSignature::P256(P256Signature {
            r: r.0.to_be_bytes(),
            s: s.0.to_be_bytes(),
            public_x: public_x.0.to_be_bytes(),
            public_y: public_y.0.to_be_bytes(),
            pre_hash,
        })),
        RpcPrimitiveSignature::WebAuthn {
            r,
            s,
            public_x,
            public_y,
            data,
        } => Ok(PrimitiveSignature::WebAuthn(WebAuthnSignature {
            r: r.0.to_be_bytes(),
            s: s.0.to_be_bytes(),
            public_x: public_x.0.to_be_bytes(),
            public_y: public_y.0.to_be_bytes(),
            data: data.0,
        })),
        RpcPrimitiveSignature::Unsupported => {
            Err(PersistedKeyError::UnsupportedAuthorizationSignature)
        }
    }
}

#[cfg(target_os = "linux")]
fn process_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            nanocode_platform::unix::user_entry(nanocode_platform::unix::user_id())
                .ok()
                .flatten()
                .map(|user| user.home)
        })
}

#[cfg(not(target_os = "linux"))]
fn process_home() -> Option<PathBuf> {
    std::env::home_dir()
}

pub fn default_credentials_path() -> Result<PathBuf, AccountsError> {
    if let Some(directory) = std::env::var_os(NANOCODE_HOME).filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(directory).join(CREDENTIALS));
    }
    process_home()
        .map(|directory| directory.join(DEFAULT_NANOCODE_HOME).join(CREDENTIALS))
        .ok_or(AccountsError::HomeUnavailable)
}

pub(crate) fn load_credentials(path: &Path) -> Result<PersistedCredentials, AccountsError> {
    let bytes = fs::read(path).map_err(|source| AccountsError::Read {
        path: path.to_owned(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| AccountsError::Decode {
        path: path.to_owned(),
        source,
    })
}

pub fn default_active_account() -> Result<Address, AccountsError> {
    load_credentials(&default_credentials_path()?).map(|credentials| credentials.account.0)
}
