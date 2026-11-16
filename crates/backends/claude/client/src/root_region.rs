use nanocode_http::types::{HeaderMap, HeaderName, HeaderValue};

const MAGIC: &[u8; 8] = b"NCROOT01";
const FIELDS: usize = MAGIC.len() + 3 * size_of::<u64>();

pub trait RootRegion: Send + Sync + 'static {
    fn adopted(&self, fresh: bool);

    fn store(&self, region: Option<Vec<u8>>);
}

pub(crate) struct Region {
    pub(crate) body: String,
    pub(crate) endpoint: String,
    pub(crate) headers: HeaderMap,
}

#[derive(Debug)]
pub enum RegionError {
    Checksum,
    Header,
    Magic,
    Torn,
    Utf8,
}

impl std::fmt::Display for RegionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Checksum => "mapped root region checksum mismatch",
            Self::Header => "mapped root region header is invalid",
            Self::Magic => "mapped root region magic mismatch",
            Self::Torn => "mapped root region length mismatch",
            Self::Utf8 => "mapped root region is not UTF-8",
        })
    }
}

impl std::error::Error for RegionError {}

pub(crate) fn encode(endpoint: &str, headers: &HeaderMap, body: &str) -> Vec<u8> {
    let mut meta = Vec::new();
    push(&mut meta, endpoint.as_bytes());
    for (name, value) in headers {
        push(&mut meta, name.as_str().as_bytes());
        push(&mut meta, value.as_bytes());
    }

    let mut region = Vec::with_capacity(FIELDS + meta.len() + body.len());
    region.extend_from_slice(MAGIC);
    region.extend_from_slice(&(meta.len() as u64).to_le_bytes());
    region.extend_from_slice(&(body.len() as u64).to_le_bytes());
    region.extend_from_slice(&[0; size_of::<u64>()]);
    region.extend_from_slice(&meta);
    region.extend_from_slice(body.as_bytes());
    let checksum = nanocode_digest::xxh64(&region[FIELDS..], 0);
    region[FIELDS - size_of::<u64>()..FIELDS].copy_from_slice(&checksum.to_le_bytes());
    region
}

pub(crate) fn decode(region: &[u8]) -> Result<Region, RegionError> {
    let word = |at: usize| {
        region
            .get(at..at + size_of::<u64>())
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_le_bytes)
    };
    if region.get(..MAGIC.len()) != Some(MAGIC) {
        return Err(RegionError::Magic);
    }

    let (Some(meta_len), Some(body_len), Some(checksum)) = (
        word(MAGIC.len()),
        word(MAGIC.len() + size_of::<u64>()),
        word(FIELDS - size_of::<u64>()),
    ) else {
        return Err(RegionError::Torn);
    };
    let meta_len = usize::try_from(meta_len).map_err(|_| RegionError::Torn)?;
    let body_len = usize::try_from(body_len).map_err(|_| RegionError::Torn)?;
    if meta_len
        .checked_add(body_len)
        .and_then(|len| len.checked_add(FIELDS))
        != Some(region.len())
    {
        return Err(RegionError::Torn);
    }
    if nanocode_digest::xxh64(&region[FIELDS..], 0) != checksum {
        return Err(RegionError::Checksum);
    }

    let mut meta = &region[FIELDS..FIELDS + meta_len];
    let endpoint = std::str::from_utf8(take(&mut meta)?)
        .map_err(|_| RegionError::Utf8)?
        .to_owned();
    let mut headers = HeaderMap::new();
    while !meta.is_empty() {
        let name = HeaderName::from_bytes(take(&mut meta)?).map_err(|_| RegionError::Header)?;
        let value = HeaderValue::from_bytes(take(&mut meta)?).map_err(|_| RegionError::Header)?;
        headers.append(name, value);
    }

    let body = std::str::from_utf8(&region[FIELDS + meta_len..])
        .map_err(|_| RegionError::Utf8)?
        .to_owned();
    Ok(Region {
        body,
        endpoint,
        headers,
    })
}

fn push(meta: &mut Vec<u8>, bytes: &[u8]) {
    meta.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    meta.extend_from_slice(bytes);
}

fn take<'a>(meta: &mut &'a [u8]) -> Result<&'a [u8], RegionError> {
    let (len, rest) = meta
        .split_first_chunk::<{ size_of::<u64>() }>()
        .ok_or(RegionError::Header)?;
    let len = usize::try_from(u64::from_le_bytes(*len)).map_err(|_| RegionError::Header)?;
    if len > rest.len() {
        return Err(RegionError::Header);
    }

    let (bytes, rest) = rest.split_at(len);
    *meta = rest;
    Ok(bytes)
}
