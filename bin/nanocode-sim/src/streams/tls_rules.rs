use std::{
    cell::Cell,
    fmt,
    io::{IoSlice, Read, Write},
    sync::{Arc, OnceLock},
};

use graviola::signing::eddsa::{Ed25519SigningKey, Ed25519VerifyingKey};
use rustls::{
    AlertDescription, ClientConfig, ClientConnection, Connection, ContentType,
    DigitallySignedStruct, Error, HandshakeKind, PeerMisbehaved, ServerConfig, ServerConnection,
    SignatureAlgorithm, SignatureScheme, SupportedCipherSuite, SupportedProtocolVersion,
    Tls12CipherSuite, Tls13CipherSuite,
    client::{
        ClientSessionMemoryCache, Resumption,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::{
        CipherSuiteCommon, CryptoProvider,
        cipher::{
            AeadKey, Iv, MessageDecrypter, MessageEncrypter, OutboundChunks, OutboundOpaqueMessage,
            OutboundPlainMessage, Tls13AeadAlgorithm, UnsupportedOperationError,
        },
    },
    internal::mutation::{self as hook, Mutation as Hook},
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::{ClientHello, ResolvesServerCert},
    sign::{CertifiedKey, Signer, SigningKey},
};

use crate::rng::Rng;

const LIMITS: [u64; 5] = [8, 9, 13, 16, 31];
const TLS12_PPM: u64 = 400_000;
const FRAGMENT_PPM: u64 = 700_000;
const SETTLE_PPM: u64 = 500_000;
const FRAGMENT_MIN: u64 = 32;
const FRAGMENT_MAX: u64 = 600;
const CHUNK_MAX: u64 = 3_000;
const DATA_MAX: u64 = 20_000;
const SMALL_MAX: u64 = 200;
const WRITES_MIN: u64 = 3;
const WRITES_MAX: u64 = 8;
const TAG_BYTES: u64 = 16;
const TICKETS_PER_SERVER: u64 = 8;
const SERVERS_MAX: u64 = 4;
const EXTRA_NAMES_MAX: u64 = 3;
const SETTLE_ROUNDS_MAX: usize = 1_000;
const READ_BUFFER: usize = 4_096;
const BYTE_VALUES: u64 = 256;
const CERTIFICATE: &[u8] = b"nanocode-sim ed25519";
const CLIENT: usize = 0;
const SERVER: usize = 1;

thread_local! {
    static MOST_PER_KEY: Cell<u64> = const { Cell::new(0) };
    static INJECT_CCS: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bug {
    None,
    Tls12WithoutEd25519,
    Hook(Hook),
}

struct Counted(&'static dyn Tls13AeadAlgorithm);

impl Tls13AeadAlgorithm for Counted {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        Box::new(Counting {
            inner: self.0.encrypter(key, iv),
            sent: 0,
        })
    }

    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        self.0.decrypter(key, iv)
    }

    fn key_len(&self) -> usize {
        self.0.key_len()
    }

    fn extract_keys(
        &self,
        key: AeadKey,
        iv: Iv,
    ) -> Result<rustls::ConnectionTrafficSecrets, UnsupportedOperationError> {
        self.0.extract_keys(key, iv)
    }
}

struct Counting {
    inner: Box<dyn MessageEncrypter>,
    sent: u64,
}

impl MessageEncrypter for Counting {
    fn encrypt(
        &mut self,
        msg: OutboundPlainMessage<'_>,
        seq: u64,
    ) -> Result<OutboundOpaqueMessage, Error> {
        self.sent += 1;
        let sent = self.sent;
        MOST_PER_KEY.with(|most| most.set(most.get().max(sent)));
        if msg.typ == ContentType::ApplicationData
            && INJECT_CCS.with(|inject| inject.replace(false))
        {
            let forged = OutboundPlainMessage {
                typ: ContentType::ChangeCipherSpec,
                version: msg.version,
                payload: OutboundChunks::Single(&[1]),
            };
            return self.inner.encrypt(forged, seq);
        }
        self.inner.encrypt(msg, seq)
    }

    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        self.inner.encrypted_payload_len(payload_len)
    }
}

fn suites_of(limit: u64, without_ed25519: bool) -> Vec<SupportedCipherSuite> {
    nanocode_tls::provider()
        .cipher_suites
        .iter()
        .map(|suite| match suite {
            SupportedCipherSuite::Tls13(inner) => {
                let aead_alg: &'static Counted = Box::leak(Box::new(Counted(inner.aead_alg)));
                let wrapped: &'static Tls13CipherSuite = Box::leak(Box::new(Tls13CipherSuite {
                    common: CipherSuiteCommon {
                        confidentiality_limit: limit,
                        ..inner.common
                    },
                    aead_alg,
                    ..**inner
                }));
                SupportedCipherSuite::Tls13(wrapped)
            }
            SupportedCipherSuite::Tls12(inner)
                if without_ed25519 && inner.sign.contains(&SignatureScheme::ED25519) =>
            {
                let sign: &'static [SignatureScheme] = Box::leak(
                    inner
                        .sign
                        .iter()
                        .copied()
                        .filter(|scheme| *scheme != SignatureScheme::ED25519)
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                );
                let stripped: &'static Tls12CipherSuite = Box::leak(Box::new(Tls12CipherSuite {
                    common: CipherSuiteCommon { ..inner.common },
                    prf_provider: inner.prf_provider,
                    kx: inner.kx,
                    sign,
                    aead_alg: inner.aead_alg,
                }));
                SupportedCipherSuite::Tls12(stripped)
            }
            other => *other,
        })
        .collect()
}

type Suites = Vec<[Vec<SupportedCipherSuite>; 2]>;

static SUITES: OnceLock<Suites> = OnceLock::new();

fn provider(limit: usize, without_ed25519: bool) -> Arc<CryptoProvider> {
    let suites = SUITES.get_or_init(|| {
        LIMITS
            .iter()
            .map(|limit| [suites_of(*limit, false), suites_of(*limit, true)])
            .collect()
    });
    Arc::new(CryptoProvider {
        cipher_suites: suites[limit][usize::from(without_ed25519)].clone(),
        ..nanocode_tls::provider()
    })
}

struct Ed(Arc<Ed25519SigningKey>);

impl fmt::Debug for Ed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Ed25519")
    }
}

impl SigningKey for Ed {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&SignatureScheme::ED25519)
            .then(|| Box::new(Self(Arc::clone(&self.0))) as Box<dyn Signer>)
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ED25519
    }
}

impl Signer for Ed {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        Ok(self.0.sign(message).to_vec())
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ED25519
    }
}

#[derive(Debug)]
struct Single(Arc<CertifiedKey>);

impl ResolvesServerCert for Single {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }
}

#[derive(Debug)]
struct Pinned([u8; 32]);

impl Pinned {
    fn check(
        &self,
        message: &[u8],
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        if dss.scheme != SignatureScheme::ED25519 {
            return Err(Error::General(format!("signed with {:?}", dss.scheme)));
        }
        Ed25519VerifyingKey::from_bytes(&self.0)
            .and_then(|key| key.verify(dss.signature(), message))
            .map(|()| HandshakeSignatureValid::assertion())
            .map_err(|error| Error::General(format!("bad Ed25519 signature: {error:?}")))
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if end_entity.as_ref() == CERTIFICATE {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(Error::General("not the simulator's certificate".to_owned()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        _cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.check(message, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        _cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.check(message, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

struct World {
    provider: Arc<CryptoProvider>,
    limit: u64,
    public: [u8; 32],
    server: Arc<ServerConfig>,
    fragment: Option<usize>,
    tls12: bool,
}

fn byte(rng: &mut Rng) -> u8 {
    u8::try_from(rng.below(BYTE_VALUES)).expect("a draw below 256 fits in a byte")
}

fn size(value: u64) -> usize {
    usize::try_from(value).expect("a simulated size fits in usize")
}

impl World {
    fn of(rng: &mut Rng, without_ed25519: bool) -> Result<Self, String> {
        let limit = rng.pick(LIMITS.len());
        let provider = provider(limit, without_ed25519);
        let seed: [u8; 32] = std::array::from_fn(|_| byte(rng));
        let key = Ed25519SigningKey::from_bytes(&seed).map_err(|error| format!("{error:?}"))?;
        let public = key.public_key().as_bytes();
        let certified = CertifiedKey::new(
            vec![CertificateDer::from(CERTIFICATE.to_vec())],
            Arc::new(Ed(Arc::new(key))),
        );
        let fragment = rng
            .chance(FRAGMENT_PPM)
            .then(|| size(rng.range(FRAGMENT_MIN, FRAGMENT_MAX)));
        let mut server = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(rustls::ALL_VERSIONS)
            .map_err(|error| error.to_string())?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Single(Arc::new(certified))));
        server.max_fragment_size = fragment;
        Ok(Self {
            provider,
            limit: LIMITS[limit],
            public,
            server: Arc::new(server),
            fragment,
            tls12: rng.chance(TLS12_PPM),
        })
    }

    fn client(&self, tls12: bool, cache: Option<usize>) -> Result<Arc<ClientConfig>, String> {
        let versions: &[&'static SupportedProtocolVersion] = if tls12 {
            &[&rustls::version::TLS12]
        } else {
            &[&rustls::version::TLS13]
        };
        let mut config = ClientConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_protocol_versions(versions)
            .map_err(|error| error.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(Pinned(self.public)))
            .with_no_client_auth();
        config.max_fragment_size = self.fragment;
        if let Some(size) = cache {
            config.resumption = Resumption::store(Arc::new(ClientSessionMemoryCache::new(size)));
        }
        Ok(Arc::new(config))
    }

    fn pair(&self, client: &Arc<ClientConfig>, name: &str, rng: &mut Rng) -> Result<Pair, String> {
        let name = ServerName::try_from(name.to_owned()).map_err(|error| error.to_string())?;
        let client =
            ClientConnection::new(Arc::clone(client), name).map_err(|error| error.to_string())?;
        let server =
            ServerConnection::new(Arc::clone(&self.server)).map_err(|error| error.to_string())?;
        Ok(Pair {
            ends: [Connection::Client(client), Connection::Server(server)],
            got: [Vec::new(), Vec::new()],
            rng: Rng::derive(rng.below(u64::MAX), "streams.tls_rules.pair"),
        })
    }
}

#[derive(Debug)]
struct Failed {
    side: usize,
    error: Error,
}

struct Pair {
    ends: [Connection; 2],
    got: [Vec<u8>; 2],
    rng: Rng,
}

const fn side_name(side: usize) -> &'static str {
    if side == CLIENT { "client" } else { "server" }
}

fn failed(what: &str, failed: &Failed) -> String {
    format!(
        "{what}: the {} failed: {:?}",
        side_name(failed.side),
        failed.error
    )
}

impl Pair {
    fn carry(&mut self, from: usize, flip_from_end: Option<u64>) -> Result<usize, Failed> {
        let to = 1 - from;
        let mut bytes = Vec::new();
        while self.ends[from].wants_write() {
            self.ends[from]
                .write_tls(&mut bytes)
                .map_err(|error| Failed {
                    side: from,
                    error: Error::General(error.to_string()),
                })?;
        }
        if let Some(back) = flip_from_end
            && let Some(index) = bytes.len().checked_sub(size(back))
        {
            bytes[index] ^= 1;
        }
        let mut rest = bytes.as_slice();
        while !rest.is_empty() {
            let piece = size(self.rng.range(1, CHUNK_MAX)).min(rest.len());
            let mut chunk = &rest[..piece];
            let read = self.ends[to].read_tls(&mut chunk).map_err(|error| Failed {
                side: to,
                error: Error::General(error.to_string()),
            })?;
            rest = &rest[read..];
            self.ends[to]
                .process_new_packets()
                .map_err(|error| Failed { side: to, error })?;
            self.drain(to);
        }
        Ok(bytes.len())
    }

    fn drain(&mut self, side: usize) {
        let mut buffer = [0; READ_BUFFER];
        while let Ok(read @ 1..) = self.ends[side].reader().read(&mut buffer) {
            self.got[side].extend_from_slice(&buffer[..read]);
        }
    }

    fn settle(&mut self) -> Result<(), Failed> {
        for _ in 0..SETTLE_ROUNDS_MAX {
            let moved = self.carry(CLIENT, None)? + self.carry(SERVER, None)?;
            if moved == 0 {
                return Ok(());
            }
        }
        Err(Failed {
            side: CLIENT,
            error: Error::General("the pair never settled".to_owned()),
        })
    }

    fn handshake(&mut self, what: &str) -> Result<(), String> {
        self.settle().map_err(|error| failed(what, &error))?;
        if self.ends.iter().any(|end| end.is_handshaking()) {
            return Err(format!("{what}: the handshake did not finish"));
        }
        Ok(())
    }

    fn write(&mut self, side: usize, data: &[u8]) -> Result<(), String> {
        let mut rest = data;
        for _ in 0..SETTLE_ROUNDS_MAX {
            if rest.is_empty() {
                return Ok(());
            }
            let written = self.ends[side]
                .writer()
                .write(rest)
                .map_err(|error| format!("{} write failed: {error}", side_name(side)))?;
            rest = &rest[written..];
            if written == 0 {
                self.settle()
                    .map_err(|error| failed("a blocked write", &error))?;
            }
        }
        Err(format!(
            "{} wrote {} of {} bytes",
            side_name(side),
            data.len() - rest.len(),
            data.len()
        ))
    }
}

fn data(rng: &mut Rng, max: u64) -> Vec<u8> {
    (0..rng.range(1, max)).map(|_| byte(rng)).collect()
}

fn exchange(world: &World, rng: &mut Rng, what: &str) -> Result<(), String> {
    let client = world.client(true, None)?;
    let mut pair = world.pair(&client, "ed25519.sim", rng)?;
    pair.handshake(what)?;
    let up = data(rng, SMALL_MAX);
    let down = data(rng, SMALL_MAX);
    pair.write(CLIENT, &up)?;
    pair.write(SERVER, &down)?;
    pair.settle().map_err(|error| failed(what, &error))?;
    if pair.got[SERVER] != up || pair.got[CLIENT] != down {
        return Err(format!("{what}: the data did not arrive intact"));
    }
    Ok(())
}

fn written_after_error(pair: &mut Pair, side: usize, what: &str) -> Result<(), String> {
    let name = side_name(side);
    let mut writer = pair.ends[side].writer();
    if let Ok(written) = writer.write(b"after") {
        return Err(format!(
            "{what}: the {name}'s write after a fatal error returned Ok({written})"
        ));
    }
    if let Ok(written) = writer.write_vectored(&[IoSlice::new(b"a"), IoSlice::new(b"b")]) {
        return Err(format!(
            "{what}: the {name}'s write_vectored after a fatal error returned Ok({written})"
        ));
    }
    if writer.flush().is_ok() {
        return Err(format!(
            "{what}: the {name}'s flush after a fatal error returned Ok"
        ));
    }
    Ok(())
}

fn fatal(world: &World, rng: &mut Rng, what: &str) -> Result<(), String> {
    let client = world.client(world.tls12, None)?;
    let mut pair = world.pair(&client, "fatal.sim", rng)?;
    pair.handshake(what)?;
    let from = rng.pick(2);
    let to = 1 - from;
    let payload = data(rng, SMALL_MAX);
    pair.write(from, &payload)?;
    match pair.carry(from, Some(rng.range(1, TAG_BYTES))) {
        Err(Failed {
            side,
            error: Error::DecryptError,
        }) if side == to => {}
        other => {
            return Err(format!(
                "{what}: a flipped tag byte from the {} ended {other:?}, not a decrypt error at the {}",
                side_name(from),
                side_name(to)
            ));
        }
    }
    written_after_error(&mut pair, to, what)?;
    match pair.carry(to, None) {
        Err(Failed {
            side,
            error: Error::AlertReceived(AlertDescription::BadRecordMac),
        }) if side == from => {}
        other => {
            return Err(format!(
                "{what}: the {}'s alert ended {other:?}, not bad_record_mac at the {}",
                side_name(to),
                side_name(from)
            ));
        }
    }
    written_after_error(&mut pair, from, what)
}

fn connect(
    world: &World,
    client: &Arc<ClientConfig>,
    name: &str,
    rng: &mut Rng,
    what: &str,
) -> Result<Option<HandshakeKind>, String> {
    let mut pair = world.pair(client, name, rng)?;
    pair.handshake(&format!("{what} {name}"))?;
    Ok(pair.ends[CLIENT].handshake_kind())
}

fn cache(world: &World, rng: &mut Rng, what: &str) -> Result<(), String> {
    let servers = rng.range(1, SERVERS_MAX);
    let sessions = servers * TICKETS_PER_SERVER - rng.below(TICKETS_PER_SERVER);
    let names_count = rng.range(1, servers + EXTRA_NAMES_MAX);
    let client = world.client(world.tls12, Some(size(sessions)))?;
    let names: Vec<String> = (0..names_count)
        .map(|index| format!("s{index}.sim"))
        .collect();
    for name in &names {
        let kind = connect(world, &client, name, rng, what)?;
        if kind != Some(HandshakeKind::Full) {
            return Err(format!(
                "{what}: the first handshake with {name} was {kind:?}"
            ));
        }
    }
    let first_kept = names.len() - size(names_count.min(servers));
    for name in &names[first_kept..] {
        let kind = connect(world, &client, name, rng, what)?;
        if kind != Some(HandshakeKind::Resumed) {
            return Err(format!(
                "{what}: a cache of {sessions} sessions ({servers} servers) after {names_count} servers did not resume {name}: {kind:?}"
            ));
        }
    }
    if let Some(evicted) = first_kept.checked_sub(1) {
        let name = &names[evicted];
        let kind = connect(world, &client, name, rng, what)?;
        if kind != Some(HandshakeKind::Full) {
            return Err(format!(
                "{what}: a cache of {sessions} sessions ({servers} servers) after {names_count} servers still held {name}: {kind:?}"
            ));
        }
    }
    Ok(())
}

fn ccs(world: &World, rng: &mut Rng, what: &str) -> Result<(), String> {
    let client = world.client(false, None)?;
    let mut pair = world.pair(&client, "ccs.sim", rng)?;
    pair.handshake(what)?;
    let from = rng.pick(2);
    let to = 1 - from;
    let payload = data(rng, SMALL_MAX);
    INJECT_CCS.with(|inject| inject.set(true));
    let written = pair.write(from, &payload);
    INJECT_CCS.with(|inject| inject.set(false));
    written?;
    match pair.carry(from, None) {
        Err(Failed {
            side,
            error: Error::PeerMisbehaved(PeerMisbehaved::IllegalMiddleboxChangeCipherSpec),
        }) if side == to => {}
        other => {
            return Err(format!(
                "{what}: a ChangeCipherSpec inside an encrypted TLS 1.3 record from the {} ended {other:?} at the {}",
                side_name(from),
                side_name(to)
            ));
        }
    }
    match pair.carry(to, None) {
        Err(Failed {
            side,
            error: Error::AlertReceived(AlertDescription::UnexpectedMessage),
        }) if side == from => Ok(()),
        other => Err(format!(
            "{what}: the {}'s alert ended {other:?}, not unexpected_message at the {}",
            side_name(to),
            side_name(from)
        )),
    }
}

fn key_updates(world: &World, rng: &mut Rng, what: &str) -> Result<(), String> {
    let client = world.client(false, None)?;
    let mut pair = world.pair(&client, "limit.sim", rng)?;
    pair.handshake(what)?;
    let mut sent = [Vec::new(), Vec::new()];
    for _ in 0..rng.range(WRITES_MIN, WRITES_MAX) {
        let side = rng.pick(2);
        let payload = data(rng, DATA_MAX);
        pair.write(side, &payload)?;
        sent[side].extend_from_slice(&payload);
        if rng.chance(SETTLE_PPM) {
            pair.settle().map_err(|error| failed(what, &error))?;
        }
    }
    pair.settle().map_err(|error| failed(what, &error))?;
    if pair.got[SERVER] != sent[CLIENT] || pair.got[CLIENT] != sent[SERVER] {
        return Err(format!(
            "{what}: sent {}B and {}B, got {}B and {}B",
            sent[CLIENT].len(),
            sent[SERVER].len(),
            pair.got[SERVER].len(),
            pair.got[CLIENT].len()
        ));
    }
    Ok(())
}

type Rule = fn(&World, &mut Rng, &str) -> Result<(), String>;

const RULES: [(&str, Rule); 5] = [
    ("tls_ed25519_tls12", exchange),
    ("tls_fatal_write", fatal),
    ("tls_session_cache", cache),
    ("tls_encrypted_ccs", ccs),
    ("tls_key_update", key_updates),
];

pub const RULE_COUNT: u64 = RULES.len() as u64;

pub fn check(seed: u64, bug: Bug) -> Result<(), String> {
    hook::set(match bug {
        Bug::Hook(mutation) => mutation,
        Bug::None | Bug::Tls12WithoutEd25519 => Hook::None,
    });
    let result = run(seed, bug == Bug::Tls12WithoutEd25519);
    hook::set(Hook::None);
    result
}

fn run(seed: u64, without_ed25519: bool) -> Result<(), String> {
    let mut rng = Rng::derive(seed, "streams.tls_rules");
    let world = World::of(&mut rng, without_ed25519)?;
    for (what, rule) in RULES {
        MOST_PER_KEY.with(|most| most.set(0));
        rule(&world, &mut rng, what)?;
        let most = MOST_PER_KEY.with(Cell::get);
        if most > world.limit {
            return Err(format!(
                "{what}: {most} records were encrypted under one key, past the confidentiality limit {}",
                world.limit
            ));
        }
    }
    Ok(())
}
