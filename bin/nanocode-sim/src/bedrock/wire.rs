use nanocode_digest::Sha256;
use nanocode_http::layer::Request;
use nanocode_sys::base64::STANDARD;
use serde_json::{Value, json};

const CRC_POLYNOMIAL: u32 = 0xEDB8_8320;
const HMAC_BLOCK: usize = 64;
const UNRESERVED: &[u8] = b"-._~";

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = match crc & 1 {
                1 => (crc >> 1) ^ CRC_POLYNOMIAL,
                _ => crc >> 1,
            };
        }
    }
    !crc
}

pub struct Frame {
    pub bytes: Vec<u8>,
    pub event_type_at: usize,
}

pub fn frame(headers: &[(&str, &str)], payload: &[u8]) -> Frame {
    let mut encoded = Vec::new();
    let mut event_type_at = 0;
    for (name, value) in headers {
        encoded.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
        encoded.extend_from_slice(name.as_bytes());
        encoded.push(7);
        encoded.extend_from_slice(&u16::try_from(value.len()).unwrap_or(u16::MAX).to_be_bytes());
        if *name == ":event-type" || *name == ":exception-type" {
            event_type_at = 12 + encoded.len();
        }
        encoded.extend_from_slice(value.as_bytes());
    }
    let total = 16 + encoded.len() + payload.len();
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(&u32::try_from(total).unwrap_or(u32::MAX).to_be_bytes());
    bytes.extend_from_slice(
        &u32::try_from(encoded.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    let prelude = crc32(&bytes);
    bytes.extend_from_slice(&prelude.to_be_bytes());
    bytes.extend_from_slice(&encoded);
    bytes.extend_from_slice(payload);
    let message = crc32(&bytes);
    bytes.extend_from_slice(&message.to_be_bytes());
    Frame {
        bytes,
        event_type_at,
    }
}

pub fn chunk(event: &Value) -> Frame {
    let payload = json!({"bytes": STANDARD.encode(event.to_string())}).to_string();
    frame(
        &[
            (":message-type", "event"),
            (":event-type", "chunk"),
            (":content-type", "application/json"),
        ],
        payload.as_bytes(),
    )
}

pub fn exception() -> Frame {
    frame(
        &[
            (":message-type", "exception"),
            (":exception-type", "throttlingException"),
            (":content-type", "application/json"),
        ],
        br#"{"message":"sim throttled"}"#,
    )
}

pub fn sse(text: &str) -> Result<Vec<(String, Value)>, String> {
    let mut events = Vec::new();
    for block in text.split("\n\n").filter(|block| !block.trim().is_empty()) {
        let mut name = None;
        let mut data = None;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("event: ") {
                name = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("data: ") {
                data = Some(value.to_owned());
            }
        }
        let (Some(name), Some(data)) = (name, data) else {
            return Err(format!(
                "an SSE block lacks its event or data line: {block:?}"
            ));
        };
        let data = serde_json::from_str(&data)
            .map_err(|error| format!("an SSE data line is not JSON ({error}): {data}"))?;
        events.push((name, data));
    }
    Ok(events)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut block = [0u8; HMAC_BLOCK];
    match key.len() > HMAC_BLOCK {
        true => block[..32].copy_from_slice(&Sha256::digest(key)),
        false => block[..key.len()].copy_from_slice(key),
    }
    let mut inner = Sha256::new();
    inner.update(&block.map(|byte| byte ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(&block.map(|byte| byte ^ 0x5c));
    outer.update(&inner.finalize());
    outer.finalize().to_vec()
}

pub fn encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || UNRESERVED.contains(&byte) || byte == b'/' {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn field<'a>(authorization: &'a str, name: &str) -> Result<&'a str, String> {
    authorization
        .split([' ', ','])
        .find_map(|part| part.strip_prefix(name))
        .ok_or_else(|| format!("the authorization header names no {name}: {authorization}"))
}

pub fn verify(request: &Request, secret: &str) -> Result<(), String> {
    let header = |name: &str| {
        request
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| format!("the signed request lacks its {name} header"))
    };
    let authorization = header("authorization")?;
    let credential = field(&authorization, "Credential=")?;
    let signed = field(&authorization, "SignedHeaders=")?;
    let signature = field(&authorization, "Signature=")?;
    let scope = credential.split_once('/').map_or("", |(_, scope)| scope);
    let mut parts = scope.split('/');
    let (Some(date), Some(region), Some(service)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(format!("the credential scope is malformed: {credential}"));
    };
    let rest = request.url.split_once("://").map_or("", |(_, rest)| rest);
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let payload = hex(&Sha256::digest(&request.body));
    let mut canonical = format!("POST\n{}\n\n", encode(path));
    for name in signed.split(';') {
        let value = match name {
            "host" => host.to_owned(),
            other => header(other)?,
        };
        canonical.push_str(&format!("{name}:{}\n", value.trim()));
    }
    canonical.push_str(&format!("\n{signed}\n{payload}"));
    let amz_date = header("x-amz-date")?;
    let text = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let mut key = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    for part in [region, service, "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    let expected = hex(&hmac(&key, text.as_bytes()));
    match expected == signature {
        true => Ok(()),
        false => Err(format!(
            "the SigV4 signature of {} does not verify: sent {signature}, a reference signer computes {expected}",
            request.url
        )),
    }
}
