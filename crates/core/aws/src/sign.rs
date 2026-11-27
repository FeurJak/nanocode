use nanocode_digest::Sha256;
use nanocode_http::layer::Request;
use nanocode_http::types::header::{AUTHORIZATION, HeaderName, HeaderValue};

use crate::encode::{UtcTime, hex, uri_encode, utc};
use crate::{Credentials, Error};

pub const SHA256_BYTES: usize = Sha256::OUTPUT_BYTES;
const SHA256_BLOCK_BYTES: usize = 64;
const HMAC_INNER_PAD: u8 = 0x36;
const HMAC_OUTER_PAD: u8 = 0x5c;
pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const SCOPE_TERMINATOR: &str = "aws4_request";
const SIGNATURE_HEX_CHARS: usize = SHA256_BYTES * 2;
const AMZ_DATE: &str = "x-amz-date";
const SECURITY_TOKEN: &str = "x-amz-security-token";
const CONTENT_SHA256: &str = "x-amz-content-sha256";

pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; SHA256_BYTES] {
    let mut block = [0u8; SHA256_BLOCK_BYTES];
    if key.len() > SHA256_BLOCK_BYTES {
        block[..SHA256_BYTES].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(&block.map(|byte| byte ^ HMAC_INNER_PAD));
    inner.update(message);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&block.map(|byte| byte ^ HMAC_OUTER_PAD));
    outer.update(&inner_hash);
    outer.finalize()
}

#[derive(Clone, Copy, Debug)]
pub struct Scope<'a> {
    pub region: &'a str,
    pub service: &'a str,
}

impl Scope<'_> {
    pub fn text(&self, time: &UtcTime) -> String {
        assert!(!self.region.is_empty());
        assert!(!self.service.is_empty());
        format!(
            "{}/{}/{}/{SCOPE_TERMINATOR}",
            time.date, self.region, self.service
        )
    }
}

pub fn signed_header_names(headers: &[(&'static str, String)]) -> String {
    let mut names = String::new();
    for (index, (name, _)) in headers.iter().enumerate() {
        if index > 0 {
            names.push(';');
        }
        names.push_str(name);
    }
    names
}

pub struct Canonical<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: &'a [(&'static str, String)],
    pub payload_sha256: &'a str,
}

fn canonical_request(canonical: &Canonical<'_>) -> String {
    assert!(canonical.path.starts_with('/'));
    assert!(!canonical.headers.is_empty());
    assert!(
        canonical
            .headers
            .windows(2)
            .all(|pair| pair[0].0 < pair[1].0)
    );
    let mut text = String::new();
    text.push_str(canonical.method);
    text.push('\n');
    text.push_str(canonical.path);
    text.push('\n');
    text.push_str(canonical.query);
    text.push('\n');
    for (name, value) in canonical.headers {
        assert!(name.bytes().all(|byte| !byte.is_ascii_uppercase()));
        text.push_str(name);
        text.push(':');
        text.push_str(value.trim());
        text.push('\n');
    }
    text.push('\n');
    text.push_str(&signed_header_names(canonical.headers));
    text.push('\n');
    text.push_str(canonical.payload_sha256);
    text
}

pub fn signature(
    secret_access_key: &str,
    scope: Scope<'_>,
    time: &UtcTime,
    canonical: &Canonical<'_>,
) -> String {
    let request = canonical_request(canonical);
    let string_to_sign = format!(
        "{ALGORITHM}\n{}\n{}\n{}",
        time.amz_date,
        scope.text(time),
        hex(&Sha256::digest(request.as_bytes()))
    );
    let secret = format!("AWS4{secret_access_key}");
    let date_key = hmac_sha256(secret.as_bytes(), time.date.as_bytes());
    let region_key = hmac_sha256(&date_key, scope.region.as_bytes());
    let service_key = hmac_sha256(&region_key, scope.service.as_bytes());
    let signing_key = hmac_sha256(&service_key, SCOPE_TERMINATOR.as_bytes());
    let signature = hex(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    assert_eq!(signature.len(), SIGNATURE_HEX_CHARS);
    signature
}

fn canonical_query(url: &nanocode_url::Url) -> String {
    let mut pairs = url
        .query_pairs()
        .map(|(name, value)| {
            let mut encoded = (String::new(), String::new());
            uri_encode(&mut encoded.0, &name, false);
            uri_encode(&mut encoded.1, &value, false);
            encoded
        })
        .collect::<Vec<_>>();
    pairs.sort();
    let mut query = String::new();
    for (index, (name, value)) in pairs.iter().enumerate() {
        if index > 0 {
            query.push('&');
        }
        query.push_str(name);
        query.push('=');
        query.push_str(value);
    }
    query
}

fn header(value: &str, sensitive: bool) -> Result<HeaderValue, Error> {
    let mut value = HeaderValue::from_str(value)
        .map_err(|error| Error::new(format!("AWS signing produced an invalid header: {error}")))?;
    value.set_sensitive(sensitive);
    Ok(value)
}

pub fn sign_request(
    request: &mut Request,
    credentials: &Credentials,
    scope: Scope<'_>,
    unix_seconds: u64,
) -> Result<(), Error> {
    let url = nanocode_url::Url::parse(&request.url)
        .map_err(|error| Error::new(format!("cannot sign {}: {error}", request.url)))?;
    let host = url
        .host_str()
        .ok_or_else(|| Error::new(format!("cannot sign {}: it names no host", request.url)))?;
    let host = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let mut path = String::new();
    match crate::sim::mutated(crate::sim::Mutation::SingleEncodedPath) {
        true => path.push_str(url.path()),
        false => uri_encode(&mut path, url.path(), true),
    }
    if path.is_empty() {
        path.push('/');
    }
    let time = utc(unix_seconds);
    let payload_sha256 = hex(&Sha256::digest(&request.body));
    let mut headers = vec![
        ("host", host),
        (CONTENT_SHA256, payload_sha256.clone()),
        (AMZ_DATE, time.amz_date.clone()),
    ];
    if let Some(token) = credentials.session_token() {
        headers.push((SECURITY_TOKEN, token.to_owned()));
    }
    let query = canonical_query(&url);
    let canonical = Canonical {
        method: request.method.as_str(),
        path: &path,
        query: &query,
        headers: &headers,
        payload_sha256: &payload_sha256,
    };
    let signature = signature(credentials.secret_access_key(), scope, &time, &canonical);
    let authorization = format!(
        "{ALGORITHM} Credential={}/{}, SignedHeaders={}, Signature={signature}",
        credentials.access_key_id(),
        scope.text(&time),
        signed_header_names(&headers)
    );
    request.headers.remove("x-api-key");
    request
        .headers
        .insert(AUTHORIZATION, header(&authorization, true)?);
    for (name, value) in &headers[1..] {
        request.headers.insert(
            HeaderName::from_static(name),
            header(value, *name == SECURITY_TOKEN)?,
        );
    }
    assert!(request.headers.contains_key(AMZ_DATE));
    assert!(request.headers.contains_key(AUTHORIZATION));
    Ok(())
}
