use std::{
    cell::{Cell, RefCell},
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    ops::Range,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use nanocode_http::{header, wire};
use nanocode_sandbox::egress::{
    Caching, Cidr, Credential, Dial, Dialing, Fixed, Grant, Identity, Injecting, Limits, Proxy,
    Reach, Resolving, Rules, Texts, Tls, Upstream,
};
use nanocode_sync::Atom;
use rustls::RootCertStore;

use crate::{
    executor::{Clock, Ended, Executor, Task},
    rng::{FNV_OFFSET, Rng, fnv},
    socket::{self, ServerSide},
};

pub const RUN_MS_MAX: u64 = 30 * 60 * 1000;
const REQUESTS_MAX: u64 = 5;
const PAYLOAD_BYTES_MAX: u64 = 6000;
const PATH_SEGMENTS_MAX: u64 = 3;
const CONNECT_PPM: u64 = 450_000;
const POST_PPM: u64 = 400_000;
const PUBLIC_POLICY_PPM: u64 = 500_000;
const LITERAL_PPM: u64 = 300_000;
const MALFORMED_PPM: u64 = 150_000;
const REFUSED_LITERAL_PPM: u64 = 500_000;
const TRAILING_DOT_PPM: u64 = 100_000;
const SPECIAL_ANSWERS_MAX: u64 = 3;
const MIXED_REFUSED_MAX: u64 = 2;
const PIECE_MAX: u64 = 4096;
const TINY_PIECES_PPM: u64 = 200_000;
const TINY_PIECE_MAX: u64 = 7;
const LATENCY_MS_MAX: u64 = 400;
const CORRUPT_BITS: u8 = 0x10;
const READ_BYTES: usize = 8192;
const BYTE_VALUES: u64 = 256;
const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_HEADERS: Duration = Duration::from_secs(30);
const IDLE_TUNNEL: Duration = Duration::from_secs(120);
const CONNECTIONS_MAX: usize = 64;
const TUNNEL_PORT: u16 = 443;
const PLAIN_PORT: u16 = 80;
const SOURCE: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const STATUS_OK: u16 = 200;
const STATUS_FORBIDDEN: u16 = 403;
const STATUS_BAD_GATEWAY: u16 = 502;
const ORIGIN_STATUSES: [u16; 4] = [200, 201, 404, 500];
const HOP_HEADER: &str = "proxy-connection";
const SIM_BLOCK: (Ipv4Addr, u32) = (Ipv4Addr::new(93, 184, 0, 0), 24);
const EVIL: Ipv4Addr = Ipv4Addr::new(93, 184, 0, 66);
const MIXED_PUBLIC: Ipv4Addr = Ipv4Addr::new(93, 184, 0, 3);
const REBIND_PUBLIC: Ipv4Addr = Ipv4Addr::new(93, 184, 0, 4);

const REFUSED_V4: [(Ipv4Addr, u32); 15] = [
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    (Ipv4Addr::new(192, 88, 99, 0), 24),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    (Ipv4Addr::new(203, 0, 113, 0), 24),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];

const REFUSED_V6: [(Ipv6Addr, u32); 11] = [
    (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
    (Ipv6Addr::new(0x5f00, 0, 0, 0, 0, 0, 0, 0), 16),
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
];

const GLOBAL_UNICAST: (Ipv6Addr, u32) = (Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3);

const EMBEDDINGS: [(Ipv6Addr, u32); 3] = [
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96),
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 96),
    (Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EgressMutation {
    None,
    WidenAllowlist,
    CorruptTunnelByte,
    RebindOnConnect,
    TrustPrivate,
}

impl EgressMutation {
    const ALL: [Self; 5] = [
        Self::None,
        Self::WidenAllowlist,
        Self::CorruptTunnelByte,
        Self::RebindOnConnect,
        Self::TrustPrivate,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::WidenAllowlist => "widen_allowlist",
            Self::CorruptTunnelByte => "corrupt_tunnel_byte",
            Self::RebindOnConnect => "rebind_on_connect",
            Self::TrustPrivate => "trust_private",
        }
    }

    pub fn names() -> String {
        Self::ALL
            .iter()
            .skip(1)
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }
}

fn within_v4(address: Ipv4Addr, (network, prefix): (Ipv4Addr, u32)) -> bool {
    let shift = u32::BITS - prefix;
    address.to_bits().checked_shr(shift).unwrap_or(0)
        == network.to_bits().checked_shr(shift).unwrap_or(0)
}

fn within_v6(address: Ipv6Addr, (network, prefix): (Ipv6Addr, u32)) -> bool {
    let shift = u128::BITS - prefix;
    address.to_bits().checked_shr(shift).unwrap_or(0)
        == network.to_bits().checked_shr(shift).unwrap_or(0)
}

fn low_v4(v6: Ipv6Addr) -> Ipv4Addr {
    Ipv4Addr::from_bits(
        u32::try_from(v6.to_bits() & u128::from(u32::MAX)).expect("the low 32 bits fit in u32"),
    )
}

fn canonical(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(v6) if EMBEDDINGS.iter().any(|prefix| within_v6(v6, *prefix)) => {
            IpAddr::V4(low_v4(v6))
        }
        _ => address,
    }
}

fn refused(address: IpAddr) -> bool {
    match canonical(address) {
        IpAddr::V4(v4) => REFUSED_V4.iter().any(|range| within_v4(v4, *range)),
        IpAddr::V6(v6) => {
            !within_v6(v6, GLOBAL_UNICAST) || REFUSED_V6.iter().any(|range| within_v6(v6, *range))
        }
    }
}

fn inside_v4(rng: &mut Rng, (network, prefix): (Ipv4Addr, u32)) -> Ipv4Addr {
    let random =
        u32::try_from(rng.draw() >> u32::BITS).expect("the high half of a u64 fits in u32");
    let mask = u32::MAX.checked_shr(prefix).unwrap_or(0);
    Ipv4Addr::from_bits(network.to_bits() | (random & mask))
}

fn inside_v6(rng: &mut Rng, (network, prefix): (Ipv6Addr, u32)) -> Ipv6Addr {
    let random = (u128::from(rng.draw()) << u64::BITS) | u128::from(rng.draw());
    let mask = u128::MAX.checked_shr(prefix).unwrap_or(0);
    Ipv6Addr::from_bits(network.to_bits() | (random & mask))
}

fn embed(rng: &mut Rng, v4: Ipv4Addr) -> IpAddr {
    let choice = rng.pick(EMBEDDINGS.len() + 1);
    EMBEDDINGS
        .get(choice)
        .map_or(IpAddr::V4(v4), |(prefix, _)| {
            IpAddr::V6(Ipv6Addr::from_bits(
                prefix.to_bits() | u128::from(v4.to_bits()),
            ))
        })
}

fn refused_address(rng: &mut Rng) -> IpAddr {
    let address = match rng.pick(3) {
        0 => {
            let range = REFUSED_V4[rng.pick(REFUSED_V4.len())];
            let v4 = inside_v4(rng, range);
            embed(rng, v4)
        }
        1 => {
            let range = REFUSED_V6[rng.pick(REFUSED_V6.len())];
            IpAddr::V6(inside_v6(rng, range))
        }
        _ => loop {
            let v6 =
                Ipv6Addr::from_bits((u128::from(rng.draw()) << u64::BITS) | u128::from(rng.draw()));
            if !within_v6(v6, GLOBAL_UNICAST)
                && !EMBEDDINGS.iter().any(|prefix| within_v6(v6, *prefix))
            {
                break IpAddr::V6(v6);
            }
        },
    };
    assert!(refused(address), "a refused sample is refused: {address}");
    address
}

fn public_address(rng: &mut Rng) -> IpAddr {
    if rng.chance(PUBLIC_POLICY_PPM) {
        loop {
            let v4 = Ipv4Addr::from_bits(
                u32::try_from(rng.draw() >> u32::BITS).expect("the high half of a u64 fits in u32"),
            );
            if !refused(IpAddr::V4(v4)) && !within_v4(v4, SIM_BLOCK) {
                break embed(rng, v4);
            }
        }
    } else {
        loop {
            let v6 = IpAddr::V6(inside_v6(rng, GLOBAL_UNICAST));
            if !refused(v6) {
                break v6;
            }
        }
    }
}

fn spell_part(rng: &mut Rng, value: u32) -> String {
    match rng.pick(3) {
        0 => format!("{value}"),
        1 => format!("0{value:o}"),
        _ => format!("0x{value:x}"),
    }
}

fn spell_v4(rng: &mut Rng, v4: Ipv4Addr) -> String {
    let bits = v4.to_bits();
    let leading = u32::try_from(rng.pick(4)).expect("at most three leading parts");
    let width = u32::BITS - u8::BITS * leading;
    let mut parts = (1..=leading)
        .map(|index| {
            let value = (bits >> (u32::BITS - u8::BITS * index)) & u32::from(u8::MAX);
            spell_part(rng, value)
        })
        .collect::<Vec<_>>();
    let last = bits & u32::MAX.checked_shr(u32::BITS - width).unwrap_or(0);
    parts.push(spell_part(rng, last));
    let mut spelled = parts.join(".");
    if rng.chance(TRAILING_DOT_PPM) {
        spelled.push('.');
    }
    spelled
}

fn spell(rng: &mut Rng, address: IpAddr) -> (String, String) {
    match address {
        IpAddr::V4(v4) => {
            let spelled = spell_v4(rng, v4);
            (spelled.clone(), spelled)
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            let spelled = match rng.pick(3) {
                0 => v6.to_string(),
                1 => segments
                    .iter()
                    .map(|segment| format!("{segment:x}"))
                    .collect::<Vec<_>>()
                    .join(":"),
                _ => format!(
                    "{}:{}",
                    segments[..6]
                        .iter()
                        .map(|segment| format!("{segment:x}"))
                        .collect::<Vec<_>>()
                        .join(":"),
                    low_v4(v6)
                ),
            };
            (format!("[{spelled}]"), spelled)
        }
    }
}

fn malformed(rng: &mut Rng) -> String {
    let byte = |rng: &mut Rng| rng.range(0, u64::from(u8::MAX));
    match rng.pick(5) {
        0 => format!(
            "{}.{}.{}.{}",
            rng.range(u64::from(u8::MAX) + 1, u64::from(u16::MAX)),
            byte(rng),
            byte(rng),
            byte(rng)
        ),
        1 => format!(
            "{}.{}",
            byte(rng),
            rng.range(1 << (u32::BITS - u8::BITS), u64::from(u32::MAX))
        ),
        2 => format!(
            "0{}.{}.{}.{}",
            rng.range(8, 9),
            byte(rng),
            byte(rng),
            byte(rng)
        ),
        3 => format!(
            "{}.{}.{}.{}.{}",
            byte(rng),
            byte(rng),
            byte(rng),
            byte(rng),
            byte(rng)
        ),
        _ => format!("{}", rng.range(u64::from(u32::MAX) + 1, u64::MAX)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Policy {
    Listed,
    Public,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Public,
    PrivateListed,
    PrivateRefused,
    Denied,
    Unresolvable,
    Unreachable,
    Special,
    Mixed,
    Rebinding,
}

struct HostEntry {
    name: &'static str,
    class: Class,
    address: Option<Ipv4Addr>,
}

const HOSTS: [HostEntry; 10] = [
    HostEntry {
        name: "api.sim",
        class: Class::Public,
        address: Some(Ipv4Addr::new(93, 184, 0, 1)),
    },
    HostEntry {
        name: "files.sim",
        class: Class::Public,
        address: Some(Ipv4Addr::new(93, 184, 0, 2)),
    },
    HostEntry {
        name: "db.sim",
        class: Class::PrivateListed,
        address: Some(Ipv4Addr::new(10, 0, 0, 9)),
    },
    HostEntry {
        name: "intra.sim",
        class: Class::PrivateRefused,
        address: Some(Ipv4Addr::new(10, 0, 0, 5)),
    },
    HostEntry {
        name: "evil.sim",
        class: Class::Denied,
        address: Some(EVIL),
    },
    HostEntry {
        name: "ghost.sim",
        class: Class::Unresolvable,
        address: None,
    },
    HostEntry {
        name: "down.sim",
        class: Class::Unreachable,
        address: Some(Ipv4Addr::new(93, 184, 0, 99)),
    },
    HostEntry {
        name: "special.sim",
        class: Class::Special,
        address: None,
    },
    HostEntry {
        name: "mixed.sim",
        class: Class::Mixed,
        address: Some(MIXED_PUBLIC),
    },
    HostEntry {
        name: "rebind.sim",
        class: Class::Rebinding,
        address: Some(REBIND_PUBLIC),
    },
];

fn entry_at(address: IpAddr) -> Option<&'static HostEntry> {
    let address = canonical(address);
    HOSTS
        .iter()
        .find(|host| host.address.map(IpAddr::V4) == Some(address))
}

fn key_of(address: IpAddr) -> String {
    entry_at(address).map_or_else(
        || canonical(address).to_string(),
        |host| host.name.to_owned(),
    )
}

fn listed_private(key: &str) -> bool {
    HOSTS
        .iter()
        .any(|host| host.class == Class::PrivateListed && host.name == key)
}

#[derive(Clone, Debug)]
enum Kind {
    Tunnel {
        payload: Vec<u8>,
    },
    Plain {
        post: bool,
        path: String,
        body: Vec<u8>,
        status: u16,
        answer: Vec<u8>,
    },
}

enum Target {
    Named(&'static HostEntry),
    Literal {
        authority: String,
        bare: String,
        address: Option<IpAddr>,
    },
}

struct Request {
    target: Target,
    kind: Kind,
    latency_ms: u64,
}

impl Request {
    fn authority(&self) -> &str {
        match &self.target {
            Target::Named(host) => host.name,
            Target::Literal { authority, .. } => authority,
        }
    }

    fn bare(&self) -> &str {
        match &self.target {
            Target::Named(host) => host.name,
            Target::Literal { bare, .. } => bare,
        }
    }

    fn key(&self) -> Option<String> {
        match &self.target {
            Target::Named(host) => Some(host.name.to_owned()),
            Target::Literal { address, .. } => address.map(key_of),
        }
    }

    const fn port(&self) -> u16 {
        match self.kind {
            Kind::Tunnel { .. } => TUNNEL_PORT,
            Kind::Plain { .. } => PLAIN_PORT,
        }
    }
}

#[derive(Clone)]
struct Answers {
    special: Vec<IpAddr>,
    mixed: Vec<IpAddr>,
    rebound: IpAddr,
    evil: IpAddr,
}

impl Answers {
    fn of(seed: u64) -> Self {
        let mut rng = Rng::derive(seed, "egress-answers");
        let special = (0..rng.range(1, SPECIAL_ANSWERS_MAX))
            .map(|_| refused_address(&mut rng))
            .collect();
        let mut mixed = (0..rng.range(1, MIXED_REFUSED_MAX))
            .map(|_| refused_address(&mut rng))
            .collect::<Vec<_>>();
        let at = rng.pick(mixed.len() + 1);
        mixed.insert(at, IpAddr::V4(MIXED_PUBLIC));
        let rebound = refused_address(&mut rng);
        let evil = embed(&mut rng, EVIL);
        Self {
            special,
            mixed,
            rebound,
            evil,
        }
    }
}

struct Script {
    policy: Policy,
    deny: String,
    requests: Vec<Request>,
    answers: Answers,
    read_piece_max: usize,
    write_piece_max: usize,
}

fn random_bytes(rng: &mut Rng, count: u64) -> Vec<u8> {
    (0..count)
        .map(|_| u8::try_from(rng.below(BYTE_VALUES)).expect("a byte value fits in u8"))
        .collect()
}

fn word(rng: &mut Rng) -> String {
    (0..rng.range(1, 8))
        .map(|_| char::from(LETTERS[rng.pick(LETTERS.len())]))
        .collect()
}

fn size(value: u64) -> usize {
    usize::try_from(value).expect("a simulated length fits in usize")
}

fn count(value: usize) -> u64 {
    u64::try_from(value).expect("a length fits in u64")
}

fn literal(rng: &mut Rng) -> Target {
    if rng.chance(MALFORMED_PPM) {
        let spelled = malformed(rng);
        return Target::Literal {
            authority: spelled.clone(),
            bare: spelled,
            address: None,
        };
    }
    let address = if rng.chance(REFUSED_LITERAL_PPM) {
        refused_address(rng)
    } else {
        public_address(rng)
    };
    let (authority, bare) = spell(rng, address);
    Target::Literal {
        authority,
        bare,
        address: Some(address),
    }
}

impl Script {
    fn of(seed: u64) -> Self {
        let mut rng = Rng::derive(seed, "egress-script");
        let policy = if rng.chance(PUBLIC_POLICY_PPM) {
            Policy::Public
        } else {
            Policy::Listed
        };
        let deny = if rng.chance(PUBLIC_POLICY_PPM) {
            format!("{EVIL}/{}", u32::BITS)
        } else {
            format!("{}/{}", EVIL.to_ipv6_mapped(), u128::BITS)
        };
        let requests = (0..rng.range(1, REQUESTS_MAX))
            .map(|index| {
                let target = if rng.chance(LITERAL_PPM) {
                    literal(&mut rng)
                } else {
                    Target::Named(&HOSTS[rng.pick(HOSTS.len())])
                };
                let kind = if rng.chance(CONNECT_PPM) {
                    let length = rng.range(1, PAYLOAD_BYTES_MAX);
                    Kind::Tunnel {
                        payload: random_bytes(&mut rng, length),
                    }
                } else {
                    let post = rng.chance(POST_PPM);
                    let path = format!(
                        "/q{index}{}",
                        (0..rng.range(1, PATH_SEGMENTS_MAX))
                            .map(|_| format!("/{}", word(&mut rng)))
                            .collect::<String>()
                    );
                    let body = if post {
                        let length = rng.range(0, PAYLOAD_BYTES_MAX);
                        random_bytes(&mut rng, length)
                    } else {
                        Vec::new()
                    };
                    let status = ORIGIN_STATUSES[rng.pick(ORIGIN_STATUSES.len())];
                    let length = rng.range(0, PAYLOAD_BYTES_MAX);
                    Kind::Plain {
                        post,
                        path,
                        body,
                        status,
                        answer: random_bytes(&mut rng, length),
                    }
                };
                Request {
                    target,
                    kind,
                    latency_ms: rng.range(0, LATENCY_MS_MAX),
                }
            })
            .collect();
        let tiny = rng.chance(TINY_PIECES_PPM);
        let piece_max = if tiny { TINY_PIECE_MAX } else { PIECE_MAX };
        Self {
            policy,
            deny,
            requests,
            answers: Answers::of(seed),
            read_piece_max: size(rng.range(1, piece_max)),
            write_piece_max: size(rng.range(1, piece_max)),
        }
    }

    fn literals(&self) -> Vec<String> {
        self.requests
            .iter()
            .filter(|request| matches!(request.target, Target::Literal { .. }))
            .map(|request| request.bare().trim_end_matches('.').to_owned())
            .collect()
    }

    fn describe(&self) -> String {
        let requests = self
            .requests
            .iter()
            .map(|request| match &request.kind {
                Kind::Tunnel { payload } => {
                    format!("CONNECT {} {}B", request.authority(), payload.len())
                }
                Kind::Plain {
                    post,
                    path,
                    body,
                    status,
                    answer,
                } => format!(
                    "{} {}{path} {}B -> {status} {}B",
                    if *post { "POST" } else { "GET" },
                    request.authority(),
                    body.len(),
                    answer.len()
                ),
            })
            .collect::<Vec<_>>()
            .join(" | ");
        let answers = &self.answers;
        format!(
            "scenario=egress policy={:?} deny={} special={:?} mixed={:?} rebound={} evil={} read_piece_max={} write_piece_max={} requests=[{requests}]",
            self.policy,
            self.deny,
            answers.special,
            answers.mixed,
            answers.rebound,
            answers.evil,
            self.read_piece_max,
            self.write_piece_max
        )
    }
}

fn allowed_hosts(script: &Script, mutation: EgressMutation) -> Vec<String> {
    HOSTS
        .iter()
        .filter(|host| host.class != Class::Denied || mutation == EgressMutation::WidenAllowlist)
        .map(|host| host.name.to_owned())
        .chain(script.literals())
        .collect()
}

fn private_hosts(script: &Script, mutation: EgressMutation) -> Vec<String> {
    let trusted = |class: Class| {
        class == Class::PrivateListed
            || (mutation == EgressMutation::TrustPrivate
                && matches!(class, Class::Special | Class::Mixed | Class::Rebinding))
    };
    let literals = if mutation == EgressMutation::TrustPrivate {
        script.literals()
    } else {
        Vec::new()
    };
    HOSTS
        .iter()
        .filter(|host| trusted(host.class))
        .map(|host| host.name.to_owned())
        .chain(literals)
        .collect()
}

#[derive(Clone, Debug)]
struct Dialed {
    key: String,
    address: IpAddr,
    received: Vec<u8>,
}

struct SimDial {
    seed: u64,
    connections: AtomicU64,
    rebinds: AtomicU64,
    answers: Answers,
    dialed: Arc<Atom<Vec<Dialed>>>,
    strays: Arc<Atom<Vec<String>>>,
    plain: Arc<Vec<(String, u16, Vec<u8>)>>,
    clock: Arc<Clock>,
    latency_ms: u64,
    read_piece_max: usize,
    write_piece_max: usize,
    mutation: EgressMutation,
}

impl SimDial {
    fn rebind(&self) -> IpAddr {
        if self
            .rebinds
            .fetch_add(1, Ordering::SeqCst)
            .is_multiple_of(2)
        {
            IpAddr::V4(REBIND_PUBLIC)
        } else {
            self.answers.rebound
        }
    }
}

impl Dial for SimDial {
    fn resolve(&self, host: &str, port: u16) -> Resolving {
        let answer = match HOSTS.iter().find(|entry| entry.name == host) {
            None => {
                self.strays.update(|strays| strays.push(host.to_owned()));
                None
            }
            Some(entry) => match entry.class {
                Class::Special => Some(self.answers.special.clone()),
                Class::Mixed => Some(self.answers.mixed.clone()),
                Class::Rebinding => Some(vec![self.rebind()]),
                Class::Denied => Some(vec![self.answers.evil]),
                _ => entry.address.map(|address| vec![IpAddr::V4(address)]),
            },
        };
        let found = answer.map(|addresses| {
            addresses
                .into_iter()
                .map(|address| SocketAddr::new(address, port))
                .collect::<Vec<_>>()
        });
        Box::pin(async move {
            found.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "sim: no such host"))
        })
    }

    fn connect(&self, address: SocketAddr) -> Dialing {
        let rebinds = entry_at(address.ip()).is_some_and(|entry| entry.class == Class::Rebinding);
        let address = if self.mutation == EgressMutation::RebindOnConnect && rebinds {
            SocketAddr::new(self.rebind(), address.port())
        } else {
            address
        };
        if entry_at(address.ip()).is_some_and(|entry| entry.class == Class::Unreachable) {
            return Box::pin(async {
                Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "sim: connection refused",
                ))
            });
        }
        let key = key_of(address.ip());
        let number = self.connections.fetch_add(1, Ordering::SeqCst);
        let (client, server) = socket::pair(
            Rng::derive(self.seed ^ number, "egress-upstream"),
            self.read_piece_max,
            self.write_piece_max,
        );
        let index = self.dialed.update(|dialed| {
            dialed.push(Dialed {
                key: key.clone(),
                address: address.ip(),
                received: Vec::new(),
            });
            dialed.len() - 1
        });
        let upstream = UpstreamTask {
            server,
            index,
            port: address.port(),
            host: key,
            dialed: Arc::clone(&self.dialed),
            plain: Arc::clone(&self.plain),
            clock: Arc::clone(&self.clock),
            latency_ms: self.latency_ms,
            corrupt: self.mutation == EgressMutation::CorruptTunnelByte,
        };
        nanocode_platform::spawn(upstream.run());
        Box::pin(async move { Ok(Box::new(client) as Upstream) })
    }
}

struct UpstreamTask {
    server: ServerSide,
    index: usize,
    port: u16,
    host: String,
    dialed: Arc<Atom<Vec<Dialed>>>,
    plain: Arc<Vec<(String, u16, Vec<u8>)>>,
    clock: Arc<Clock>,
    latency_ms: u64,
    corrupt: bool,
}

impl UpstreamTask {
    fn record(&self, bytes: &[u8]) {
        let index = self.index;
        self.dialed
            .update(|dialed| dialed[index].received.extend_from_slice(bytes));
    }

    async fn run(self) {
        if self.port == TUNNEL_PORT {
            self.echo().await;
        } else {
            self.origin().await;
        }
    }

    async fn echo(self) {
        let mut chunk = [0_u8; READ_BYTES];
        let mut corrupted = !self.corrupt;
        loop {
            match self.server.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    self.record(&chunk[..read]);
                    if !corrupted {
                        chunk[0] ^= CORRUPT_BITS;
                        corrupted = true;
                    }
                    if self.server.write(&chunk[..read]).is_err() {
                        break;
                    }
                }
            }
        }
        self.server.close();
    }

    async fn origin(self) {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; READ_BYTES];
        let complete = loop {
            if let Some(length) = request_length(&buffer)
                && buffer.len() >= length
            {
                break true;
            }
            match self.server.read(&mut chunk).await {
                Ok(0) | Err(_) => break false,
                Ok(read) => {
                    self.record(&chunk[..read]);
                    buffer.extend_from_slice(&chunk[..read]);
                }
            }
        };
        if complete {
            self.clock
                .sleep(Duration::from_millis(self.latency_ms))
                .await;
            let target = request_target(&buffer).unwrap_or_default();
            let answer = self
                .plain
                .iter()
                .find(|(host, _, _)| *host == format!("{}{target}", self.host))
                .map_or_else(
                    || (404, Vec::new()),
                    |(_, status, answer)| (*status, answer.clone()),
                );
            let mut response = format!(
                "HTTP/1.1 {} Sim\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                answer.0,
                answer.1.len()
            )
            .into_bytes();
            response.extend_from_slice(&answer.1);
            drop(self.server.write(&response));
        }
        self.server.close();
    }
}

fn head_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| at + 4)
}

fn request_length(bytes: &[u8]) -> Option<usize> {
    let end = head_end(bytes)?;
    let head = std::str::from_utf8(&bytes[..end]).ok()?;
    let body = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    Some(end + body)
}

fn request_target(bytes: &[u8]) -> Option<String> {
    let end = head_end(bytes)?;
    let head = std::str::from_utf8(&bytes[..end]).ok()?;
    head.lines().next()?.split(' ').nth(1).map(str::to_owned)
}

#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Status(u16),
    Tunnel(Vec<u8>),
    Body(Vec<u8>),
    Failed(String),
}

struct Reader<'a> {
    side: &'a ServerSide,
    buffer: Vec<u8>,
    eof: bool,
}

impl Reader<'_> {
    async fn more(&mut self) -> bool {
        if self.eof {
            return false;
        }
        let mut chunk = [0_u8; READ_BYTES];
        match self.side.read(&mut chunk).await {
            Ok(0) | Err(_) => {
                self.eof = true;
                false
            }
            Ok(read) => {
                self.buffer.extend_from_slice(&chunk[..read]);
                true
            }
        }
    }

    async fn head(&mut self) -> Result<(u16, Option<usize>), String> {
        loop {
            if let Some((head, length)) =
                wire::parse_response(&self.buffer).map_err(|error| error.to_string())?
            {
                self.buffer.drain(..length);
                let declared = head
                    .headers
                    .get(header::CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<usize>().ok());
                return Ok((head.status.as_u16(), declared));
            }
            if !self.more().await {
                return Err(format!(
                    "the connection closed inside a response head ({} bytes)",
                    self.buffer.len()
                ));
            }
        }
    }

    async fn exactly(&mut self, length: usize) -> Vec<u8> {
        while self.buffer.len() < length && self.more().await {}
        let take = length.min(self.buffer.len());
        self.buffer.drain(..take).collect()
    }

    async fn rest(&mut self) -> Vec<u8> {
        while self.more().await {}
        std::mem::take(&mut self.buffer)
    }
}

async fn client(request: &Request, side: ServerSide, log: &RefCell<Vec<Seen>>, trace: &Trace) {
    let host = request.authority();
    let mut reader = Reader {
        side: &side,
        buffer: Vec::new(),
        eof: false,
    };
    let push = |seen: Seen| log.borrow_mut().push(seen);
    match &request.kind {
        Kind::Tunnel { payload } => {
            let head = format!(
                "CONNECT {host}:{TUNNEL_PORT} HTTP/1.1\r\nhost: {host}:{TUNNEL_PORT}\r\n\r\n"
            );
            if side.write(head.as_bytes()).is_err() {
                return push(Seen::Failed(
                    "the proxy closed before the request".to_owned(),
                ));
            }
            match reader.head().await {
                Err(error) => push(Seen::Failed(error)),
                Ok((status, declared)) => {
                    push(Seen::Status(status));
                    if status == STATUS_OK {
                        if side.write(payload).is_ok() {
                            let echoed = reader.exactly(payload.len()).await;
                            push(Seen::Tunnel(echoed));
                        }
                    } else {
                        let body = match declared {
                            Some(length) => reader.exactly(length).await,
                            None => reader.rest().await,
                        };
                        push(Seen::Body(body));
                    }
                }
            }
        }
        Kind::Plain {
            post, path, body, ..
        } => {
            let method = if *post { "POST" } else { "GET" };
            let mut bytes = format!(
                "{method} http://{host}{path} HTTP/1.1\r\nhost: {host}\r\n{HOP_HEADER}: keep-alive\r\ncontent-length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes();
            bytes.extend_from_slice(body);
            if side.write(&bytes).is_err() {
                return push(Seen::Failed(
                    "the proxy closed before the request".to_owned(),
                ));
            }
            match reader.head().await {
                Err(error) => push(Seen::Failed(error)),
                Ok((status, declared)) => {
                    push(Seen::Status(status));
                    let body = match declared {
                        Some(length) => reader.exactly(length).await,
                        None => reader.rest().await,
                    };
                    push(Seen::Body(body));
                }
            }
        }
    }
    trace.line(&format!("client for {host} saw {:?}", log.borrow().first()));
    side.close();
}

struct Trace {
    clock: Arc<Clock>,
    digest: Cell<u64>,
    verbose: bool,
}

impl Trace {
    fn line(&self, text: &str) {
        let now_ns = self.clock.now_ns();
        let digest = fnv(
            fnv(self.digest.get(), &now_ns.to_le_bytes()),
            text.as_bytes(),
        );
        self.digest.set(digest);
        if self.verbose {
            println!("  {now_ns:>14} {text}");
        }
    }
}

fn private_text(host: &str) -> String {
    format!("{host} resolves only to private addresses.")
}

fn refused_text(host: &str) -> String {
    format!("{host} is not on the allowlist.")
}

fn proxy(
    script: &Script,
    mutation: EgressMutation,
    dial: Arc<SimDial>,
) -> Result<Arc<Proxy<Fixed>>, String> {
    let tls = Tls::with_roots("nanocode-sim", RootCertStore::empty())
        .map_err(|error| format!("egress CA: {error}"))?;
    let reach = match script.policy {
        Policy::Listed => Reach::Listed {
            hosts: allowed_hosts(script, mutation),
            private_hosts: private_hosts(script, mutation),
            tailnet: None,
        },
        Policy::Public => Reach::Public {
            deny: vec![
                script
                    .deny
                    .parse::<Cidr>()
                    .map_err(|error| format!("deny: {error}"))?,
            ],
        },
    };
    let rules = Rules {
        caching: Caching {
            granted: Duration::ZERO,
            refused: Duration::ZERO,
            recheck: Duration::ZERO,
            gather: Duration::ZERO,
            most: 0,
            flight: Duration::ZERO,
        },
        reporting: None,
        identity: Identity::Fixed(Credential {
            name: "sim".into(),
            token: String::new(),
        }),
        reach,
        passthrough: Vec::new(),
        injecting: Injecting::Set,
        intercept_port: None,
        texts: Texts {
            client: "sim",
            needs_credential: "",
            realm: "",
            unreachable: "",
            stale: "",
            private: private_text,
            refused: refused_text,
            compressed: "compressed",
        },
        limits: Limits {
            connections: CONNECTIONS_MAX,
            per_source: CONNECTIONS_MAX,
            connect: CONNECT_TIMEOUT,
            idle_headers: IDLE_HEADERS,
            idle_tunnel: IDLE_TUNNEL,
        },
    };
    let fixed = Arc::new(Fixed(Grant {
        secrets: Vec::new(),
    }));
    Ok(Arc::new(
        Proxy::new(fixed, Arc::new(tls), rules).dialing(dial),
    ))
}

enum Outcome {
    Reach(String),
    Refuse(u16, String),
    Rebound(String, String),
}

fn outcome(request: &Request, policy: Policy) -> Outcome {
    let bare = request.bare();
    let private = || Outcome::Refuse(STATUS_FORBIDDEN, private_text(bare));
    let unresolved = || Outcome::Refuse(STATUS_BAD_GATEWAY, format!("{bare} does not resolve."));
    match &request.target {
        Target::Literal { address: None, .. } => unresolved(),
        Target::Literal {
            address: Some(address),
            ..
        } if refused(*address) => private(),
        Target::Literal {
            address: Some(address),
            ..
        } => Outcome::Reach(key_of(*address)),
        Target::Named(host) => match (host.class, policy) {
            (Class::Public, _) | (Class::PrivateListed | Class::Mixed, Policy::Listed) => {
                Outcome::Reach(host.name.to_owned())
            }
            (Class::Denied, Policy::Listed) => {
                Outcome::Refuse(STATUS_FORBIDDEN, refused_text(bare))
            }
            (Class::PrivateListed | Class::Mixed | Class::Denied, Policy::Public)
            | (Class::PrivateRefused | Class::Special, _) => private(),
            (Class::Rebinding, _) => Outcome::Rebound(host.name.to_owned(), private_text(bare)),
            (Class::Unresolvable, _) => unresolved(),
            (Class::Unreachable, _) => Outcome::Refuse(
                STATUS_BAD_GATEWAY,
                format!("{bare}:{} can't be reached.", request.port()),
            ),
        },
    }
}

fn reached(request: &Request) -> Vec<Seen> {
    match &request.kind {
        Kind::Tunnel { payload } => vec![Seen::Status(STATUS_OK), Seen::Tunnel(payload.clone())],
        Kind::Plain { status, answer, .. } => {
            vec![Seen::Status(*status), Seen::Body(answer.clone())]
        }
    }
}

fn refusal(status: u16, text: &str) -> Vec<Seen> {
    vec![
        Seen::Status(status),
        Seen::Body(format!("{text}\n").into_bytes()),
    ]
}

fn alternatives(request: &Request, policy: Policy) -> Vec<(Vec<Seen>, Option<String>)> {
    match outcome(request, policy) {
        Outcome::Reach(key) => vec![(reached(request), Some(key))],
        Outcome::Refuse(status, text) => vec![(refusal(status, &text), None)],
        Outcome::Rebound(key, text) => vec![
            (reached(request), Some(key)),
            (refusal(STATUS_FORBIDDEN, &text), None),
        ],
    }
}

fn check_upstream(request: &Request, received: &[u8]) -> Result<(), String> {
    let name = request.authority();
    match &request.kind {
        Kind::Tunnel { payload } => {
            if received != payload.as_slice() {
                return Err(format!(
                    "the upstream of {name} received {} tunnelled bytes where the client sent {}",
                    received.len(),
                    payload.len()
                ));
            }
        }
        Kind::Plain {
            post, path, body, ..
        } => {
            let end = head_end(received)
                .ok_or_else(|| format!("the origin {name} got no request head"))?;
            let head = String::from_utf8_lossy(&received[..end]).to_ascii_lowercase();
            let method = if *post { "post" } else { "get" };
            let line = format!("{method} {path} http/1.1\r\n");
            if !head.starts_with(&line) {
                return Err(format!(
                    "the origin {name} got {:?} where the proxy owes {line:?}",
                    head.lines().next().unwrap_or_default()
                ));
            }
            if !head.contains(&format!("\r\nhost: {name}\r\n")) {
                return Err(format!("the origin {name} got no host header naming it"));
            }
            if head.contains(&format!("\r\n{HOP_HEADER}:")) {
                return Err(format!(
                    "the origin {name} got the hop-by-hop {HOP_HEADER} header"
                ));
            }
            if &received[end..] != body.as_slice() {
                return Err(format!(
                    "the origin {name} got a body of {} bytes where the client sent {}",
                    received.len() - end,
                    body.len()
                ));
            }
        }
    }
    Ok(())
}

fn show(list: &[Seen]) -> String {
    list.iter()
        .map(|seen| match seen {
            Seen::Status(status) => format!("status {status}"),
            Seen::Tunnel(bytes) => format!("tunnel {}B", bytes.len()),
            Seen::Body(bytes) => format!(
                "body {:?}",
                String::from_utf8_lossy(&bytes[..bytes.len().min(80)])
            ),
            Seen::Failed(error) => format!("failed {error}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn judge(
    script: &Script,
    seen: &[Vec<Seen>],
    dialed: &[Dialed],
    strays: &[String],
    ended: Ended,
    now_ns: u64,
) -> Result<(), String> {
    for dialed in dialed {
        let trusted = script.policy == Policy::Listed && listed_private(&dialed.key);
        if refused(dialed.address) && !trusted {
            return Err(format!(
                "the proxy connected to {} for {}, an address the policy refuses",
                dialed.address, dialed.key
            ));
        }
    }
    if let Some(stray) = strays.first() {
        return Err(format!(
            "the proxy asked the resolver for {stray}, an address literal it must judge as written"
        ));
    }
    if ended == Ended::Stuck {
        return Err(format!(
            "every task waits and no timer is set: a hang (client saw {seen:?})"
        ));
    }
    let run_ms = now_ns / 1_000_000;
    if run_ms > RUN_MS_MAX {
        return Err(format!(
            "the run took {run_ms} ms of virtual time, over {RUN_MS_MAX} ms"
        ));
    }
    let mut owed: Vec<(String, &Request)> = Vec::new();
    for (index, request) in script.requests.iter().enumerate() {
        let wanted = alternatives(request, script.policy);
        let Some((_, key)) = wanted.iter().find(|(sequence, _)| *sequence == seen[index]) else {
            return Err(format!(
                "request {index} to {}: the client saw [{}] where the policy says [{}]",
                request.authority(),
                show(&seen[index]),
                wanted
                    .iter()
                    .map(|(sequence, _)| show(sequence))
                    .collect::<Vec<_>>()
                    .join("] or [")
            ));
        };
        if let Some(key) = key {
            owed.push((key.clone(), request));
        }
    }
    let mut keys = owed
        .iter()
        .map(|(key, _)| key.clone())
        .chain(dialed.iter().map(|dialed| dialed.key.clone()))
        .collect::<Vec<_>>();
    keys.sort();
    keys.dedup();
    for key in keys {
        let requests = owed
            .iter()
            .filter(|(owed, _)| *owed == key)
            .map(|(_, request)| *request)
            .collect::<Vec<_>>();
        let mut unmatched = dialed
            .iter()
            .filter(|dialed| dialed.key == key)
            .collect::<Vec<_>>();
        if requests.len() != unmatched.len() {
            return Err(format!(
                "the proxy opened {} upstream connections to {key} where the policy allows {}",
                unmatched.len(),
                requests.len()
            ));
        }
        for request in requests {
            let at = unmatched
                .iter()
                .position(|dialed| check_upstream(request, &dialed.received).is_ok())
                .ok_or_else(|| {
                    unmatched.first().map_or_else(String::new, |dialed| {
                        check_upstream(request, &dialed.received)
                            .err()
                            .unwrap_or_default()
                    })
                })?;
            unmatched.remove(at);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EgressStats {
    pub seeds: u64,
    pub tunnels: u64,
    pub plain: u64,
    pub literals: u64,
    pub refused: u64,
    pub upstream: u64,
}

impl EgressStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.tunnels += other.tunnels;
        self.plain += other.plain;
        self.literals += other.literals;
        self.refused += other.refused;
        self.upstream += other.upstream;
    }
}

pub struct EgressOutcome {
    pub digest: u64,
    pub wrong: Option<String>,
    pub stats: EgressStats,
    pub shape: String,
}

fn simulate(seed: u64, mutation: EgressMutation, verbose: bool) -> EgressOutcome {
    let script = Script::of(seed);
    let shape = script.describe();
    let clock = Clock::new();
    crate::host::enter(seed, &clock);
    let trace = Trace {
        clock: Arc::clone(&clock),
        digest: Cell::new(FNV_OFFSET),
        verbose,
    };
    trace.line(&format!("seed {seed} {shape}"));
    let plain = script
        .requests
        .iter()
        .filter_map(|request| match &request.kind {
            Kind::Plain {
                path,
                status,
                answer,
                ..
            } => Some((format!("{}{path}", request.key()?), *status, answer.clone())),
            Kind::Tunnel { .. } => None,
        })
        .collect::<Vec<_>>();
    let dialed = Arc::new(Atom::new(Vec::new()));
    let strays = Arc::new(Atom::new(Vec::new()));
    let dial = Arc::new(SimDial {
        seed,
        connections: AtomicU64::new(0),
        rebinds: AtomicU64::new(0),
        answers: script.answers.clone(),
        dialed: Arc::clone(&dialed),
        strays: Arc::clone(&strays),
        plain: Arc::new(plain),
        clock: Arc::clone(&clock),
        latency_ms: script
            .requests
            .first()
            .map_or(0, |request| request.latency_ms),
        read_piece_max: script.read_piece_max,
        write_piece_max: script.write_piece_max,
        mutation,
    });
    let proxy = match proxy(&script, mutation, dial) {
        Ok(proxy) => proxy,
        Err(error) => {
            return EgressOutcome {
                digest: 0,
                wrong: Some(error),
                stats: EgressStats::default(),
                shape,
            };
        }
    };
    let logs = script
        .requests
        .iter()
        .map(|_| RefCell::new(Vec::new()))
        .collect::<Vec<_>>();
    let mut tasks: Vec<Task<'_>> = Vec::new();
    for (index, request) in script.requests.iter().enumerate() {
        let (inbound, side) = socket::pair(
            Rng::derive(seed ^ count(index), "egress-inbound"),
            script.read_piece_max,
            script.write_piece_max,
        );
        let slot: nanocode_sandbox::egress::Slot = Arc::new(());
        tasks.push(Box::pin(Arc::clone(&proxy).serve(inbound, slot, SOURCE)));
        tasks.push(Box::pin(client(request, side, &logs[index], &trace)));
    }
    let ended = Executor::new(Arc::clone(&clock), Rng::derive(seed, "scheduler")).run(tasks, None);
    drop(proxy);
    let seen = logs
        .into_iter()
        .map(RefCell::into_inner)
        .collect::<Vec<_>>();
    let dialed = dialed.get().as_ref().clone();
    let strays = strays.get().as_ref().clone();
    let wrong = judge(&script, &seen, &dialed, &strays, ended, clock.now_ns()).err();
    trace.line(&format!("verdict {wrong:?}"));
    let tally = |keep: &dyn Fn(&Request) -> bool| {
        count(
            script
                .requests
                .iter()
                .filter(|request| keep(request))
                .count(),
        )
    };
    let stats = EgressStats {
        seeds: 1,
        tunnels: tally(&|request| matches!(request.kind, Kind::Tunnel { .. })),
        plain: tally(&|request| matches!(request.kind, Kind::Plain { .. })),
        literals: tally(&|request| matches!(request.target, Target::Literal { .. })),
        refused: tally(&|request| matches!(outcome(request, script.policy), Outcome::Refuse(..))),
        upstream: count(dialed.len()),
    };
    EgressOutcome {
        digest: trace.digest.get(),
        wrong,
        stats,
        shape,
    }
}

pub fn run_seed(seed: u64, mutation: EgressMutation, verbose: bool) -> EgressOutcome {
    std::panic::catch_unwind(|| simulate(seed, mutation, verbose)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default();
        EgressOutcome {
            digest: 0,
            wrong: Some(format!("panic: {message}")),
            stats: EgressStats::default(),
            shape: String::new(),
        }
    })
}

pub fn judge_seed(
    seed: u64,
    mutation: EgressMutation,
    twice: bool,
    verbose: bool,
    totals: &mut EgressStats,
) -> Option<String> {
    let first = run_seed(seed, mutation, verbose);
    totals.add(&first.stats);
    if verbose {
        println!("seed {seed} digest {:016x} {}", first.digest, first.shape);
    }
    if first.wrong.is_some() || !twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let second = crate::host::second_pass(|| run_seed(seed, mutation, false));
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: trace digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: EgressMutation) -> String {
    let mut command = format!("nanocode-sim --scenario egress --seed {seed}");
    if mutation != EgressMutation::None {
        command.push_str(" --mutation ");
        command.push_str(mutation.name());
    }
    command
}

pub fn report(
    seeds: &Range<u64>,
    passed: u64,
    failed: u64,
    twice: bool,
    mutation: EgressMutation,
    totals: &EgressStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=egress mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        EgressMutation::None if failed == 0 => ExitCode::SUCCESS,
        EgressMutation::None => ExitCode::FAILURE,
        _ if failed > 0 => {
            println!("MUTATION {} caught by {failed} seeds", mutation.name());
            ExitCode::SUCCESS
        }
        _ => {
            println!("MUTATION {} NOT caught", mutation.name());
            ExitCode::FAILURE
        }
    }
}
