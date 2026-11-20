use std::{
    error::Error,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

const fn v4(a: u8, b: u8, c: u8, d: u8, bits: u8) -> Cidr {
    Cidr {
        network: IpAddr::V4(Ipv4Addr::new(a, b, c, d)),
        bits,
    }
}

const fn v6(first: u16, second: u16, third: u16, bits: u8) -> Cidr {
    Cidr {
        network: IpAddr::V6(Ipv6Addr::new(first, second, third, 0, 0, 0, 0, 0)),
        bits,
    }
}

pub const SPECIAL: [Cidr; 26] = [
    v4(0, 0, 0, 0, 8),
    v4(10, 0, 0, 0, 8),
    v4(100, 64, 0, 0, 10),
    v4(127, 0, 0, 0, 8),
    v4(169, 254, 0, 0, 16),
    v4(172, 16, 0, 0, 12),
    v4(192, 0, 0, 0, 24),
    v4(192, 0, 2, 0, 24),
    v4(192, 88, 99, 0, 24),
    v4(192, 168, 0, 0, 16),
    v4(198, 18, 0, 0, 15),
    v4(198, 51, 100, 0, 24),
    v4(203, 0, 113, 0, 24),
    v4(224, 0, 0, 0, 4),
    v4(240, 0, 0, 0, 4),
    v6(0x64, 0xff9b, 1, 48),
    v6(0x100, 0, 0, 64),
    v6(0x2001, 0, 0, 23),
    v6(0x2001, 0xdb8, 0, 32),
    v6(0x2002, 0, 0, 16),
    v6(0x3fff, 0, 0, 20),
    v6(0x5f00, 0, 0, 16),
    v6(0xfc00, 0, 0, 7),
    v6(0xfe80, 0, 0, 10),
    v6(0xfec0, 0, 0, 10),
    v6(0xff00, 0, 0, 8),
];

const GLOBAL_UNICAST: Cidr = v6(0x2000, 0, 0, 3);

#[must_use]
pub const fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => !special(IpAddr::V4(v4)),
        IpAddr::V6(v6) => match embedded(v6) {
            Some(v4) => !special(IpAddr::V4(v4)),
            None => GLOBAL_UNICAST.covers(address) && !special(address),
        },
    }
}

const fn special(address: IpAddr) -> bool {
    let mut index = 0;
    while index < SPECIAL.len() {
        if SPECIAL[index].covers(address) {
            return true;
        }
        index += 1;
    }
    false
}

const fn embedded(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    match v6.segments() {
        [0, 0, 0, 0, 0, 0 | 0xffff, high, low] | [0x64, 0xff9b, 0, 0, 0, 0, high, low] => Some(
            Ipv4Addr::from_bits(((high as u32) << u16::BITS) | low as u32),
        ),
        _ => None,
    }
}

#[must_use]
pub fn reachable(address: IpAddr, deny: &[Cidr]) -> bool {
    is_public(address) && !deny.iter().any(|cidr| cidr.contains(address))
}

pub(super) enum Literal {
    Address(IpAddr),
    Name,
    Malformed,
}

pub(super) fn literal(host: &str) -> Literal {
    let bare = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let unzoned = bare.split_once('%').map_or(bare, |(address, _)| address);
    if let Ok(v6) = unzoned.parse::<Ipv6Addr>() {
        return Literal::Address(IpAddr::V6(v6));
    }
    let dotted = bare.strip_suffix('.').unwrap_or(bare);
    let parts: Vec<&str> = dotted.split('.').collect();
    if !parts.iter().all(|part| numeric(part)) {
        return Literal::Name;
    }
    dotted_v4(&parts).map_or(Literal::Malformed, |v4| Literal::Address(IpAddr::V4(v4)))
}

fn numeric(part: &str) -> bool {
    let hex = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X"));
    hex.map_or_else(
        || !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()),
        |digits| digits.bytes().all(|b| b.is_ascii_hexdigit()),
    )
}

fn part_value(part: &str) -> Option<u32> {
    let (digits, radix) = match part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
        Some(hex) => (hex, 16),
        None => match part.strip_prefix('0') {
            Some(octal) if !octal.is_empty() => (octal, 8),
            _ => (part, 10),
        },
    };
    if digits.is_empty() {
        return None;
    }
    u32::from_str_radix(digits, radix).ok()
}

fn dotted_v4(parts: &[&str]) -> Option<Ipv4Addr> {
    let values = parts
        .iter()
        .map(|part| part_value(part))
        .collect::<Option<Vec<u32>>>()?;
    let (last, leading) = values.split_last()?;
    let leading_bits = u8::BITS.checked_mul(u32::try_from(leading.len()).ok()?)?;
    let width = u32::BITS
        .checked_sub(leading_bits)
        .filter(|width| *width >= u8::BITS)?;
    if leading.iter().any(|value| *value > u32::from(u8::MAX))
        || last.checked_shr(width).unwrap_or(0) != 0
    {
        return None;
    }
    let high = leading
        .iter()
        .fold(0_u32, |high, value| (high << u8::BITS) | value)
        .checked_shl(width)
        .unwrap_or(0);
    Some(Ipv4Addr::from_bits(high | last))
}

#[must_use]
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    pattern.strip_prefix("*.").map_or_else(
        || host == pattern,
        |domain| {
            host.strip_suffix(domain)
                .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.'))
        },
    )
}

pub(super) const TAILNET_SUFFIX: &str = ".ts.net";

#[must_use]
pub fn tailnet_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let bare = host.strip_prefix("*.").unwrap_or(&host);
    bare.ends_with(TAILNET_SUFFIX) || TAILNET_SUFFIX.strip_prefix('.') == Some(bare)
}

#[must_use]
pub fn valid_pattern(host: &str) -> bool {
    let bare = host.strip_prefix("*.").unwrap_or(host);
    !bare.is_empty()
        && bare.len() <= 253
        && bare
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    bits: u8,
}

impl Cidr {
    #[must_use]
    pub const fn contains(&self, address: IpAddr) -> bool {
        if self.covers(address) {
            return true;
        }
        match address {
            IpAddr::V6(v6) => match embedded(v6) {
                Some(v4) => self.covers(IpAddr::V4(v4)),
                None => false,
            },
            IpAddr::V4(_) => false,
        }
    }

    const fn covers(&self, address: IpAddr) -> bool {
        let prefix = self.bits as u32;
        match (address, self.network) {
            (IpAddr::V4(address), IpAddr::V4(network)) => {
                let mask = match u32::MAX.checked_shl(u32::BITS - prefix) {
                    Some(mask) => mask,
                    None => 0,
                };
                address.to_bits() & mask == network.to_bits() & mask
            }
            (IpAddr::V6(address), IpAddr::V6(network)) => {
                let mask = match u128::MAX.checked_shl(u128::BITS - prefix) {
                    Some(mask) => mask,
                    None => 0,
                };
                address.to_bits() & mask == network.to_bits() & mask
            }
            _ => false,
        }
    }
}

impl FromStr for Cidr {
    type Err = NotCidr;

    fn from_str(text: &str) -> Result<Self, NotCidr> {
        let not = |cause: Option<Box<dyn Error + Send + Sync>>| NotCidr {
            text: text.to_owned(),
            cause,
        };
        let (network, bits) = text.split_once('/').unwrap_or((text, ""));
        let network: IpAddr = network
            .parse()
            .map_err(|error| not(Some(Box::new(error))))?;
        let most: u8 = if network.is_ipv4() { 32 } else { 128 };
        let bits = if bits.is_empty() {
            most
        } else {
            bits.parse().map_err(|error| not(Some(Box::new(error))))?
        };
        if bits > most {
            return Err(not(None));
        }
        match network {
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => {
                    let mapped = u8::try_from(u128::BITS - u32::BITS)
                        .map_err(|error| not(Some(Box::new(error))))?;
                    let bits = bits.checked_sub(mapped).ok_or_else(|| not(None))?;
                    Ok(Self {
                        network: IpAddr::V4(v4),
                        bits,
                    })
                }
                None => Ok(Self { network, bits }),
            },
            IpAddr::V4(_) => Ok(Self { network, bits }),
        }
    }
}

#[derive(Debug)]
pub struct NotCidr {
    text: String,
    cause: Option<Box<dyn Error + Send + Sync>>,
}

impl fmt::Display for NotCidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is not a CIDR", self.text)
    }
}

impl Error for NotCidr {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.cause
            .as_deref()
            .map(|cause| cause as &(dyn Error + 'static))
    }
}
