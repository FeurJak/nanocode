use nanocode_aws::event_stream::{Decoder, Message};
use nanocode_http::layer::{Body, Error, Request, Response};
use nanocode_http::types::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use nanocode_sync::StreamExt;
use nanocode_sys::base64::STANDARD;
use serde_json::{Map, Value};

pub const SERVICE: &str = "bedrock";
const VERSION: &str = "bedrock-2023-05-31";
const BASE_URL: &str = "ANTHROPIC_BEDROCK_BASE_URL";
const EVENT_STREAM: &str = "application/vnd.amazon.eventstream";
const SERVER_SENT_EVENTS: &str = "text/event-stream";
const BETA_HEADER: &str = "anthropic-beta";

pub fn base(region: &str) -> String {
    std::env::var(BASE_URL)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("https://bedrock-runtime.{region}.amazonaws.com"))
        .trim_end_matches('/')
        .to_owned()
}

pub fn rewrite(
    request: &mut Request,
    mut body: Map<String, Value>,
    base: &str,
    model: &str,
) -> Result<(), Error> {
    body.remove("model");
    let stream = body
        .remove("stream")
        .and_then(|stream| stream.as_bool())
        .unwrap_or(false);
    let betas = request
        .headers
        .get_all(BETA_HEADER)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|beta| !beta.is_empty())
        .map(|beta| Value::String(beta.to_owned()))
        .collect::<Vec<_>>();
    let mut ordered = Map::new();
    ordered.insert(
        "anthropic_version".to_owned(),
        Value::String(VERSION.to_owned()),
    );
    if !betas.is_empty() {
        ordered.insert("anthropic_beta".to_owned(), Value::Array(betas));
    }
    ordered.extend(body);
    request.body = serde_json::to_vec(&ordered)
        .map_err(|error| Error::new(format!("Claude on Amazon Bedrock: {error}")))?
        .into();
    let mut encoded = String::new();
    nanocode_aws::uri_encode(&mut encoded, model, false);
    let action = match stream {
        true => "invoke-with-response-stream",
        false => "invoke",
    };
    request.url = format!("{base}/model/{encoded}/{action}");
    for name in [BETA_HEADER, "anthropic-version", "x-api-key"] {
        request.headers.remove(name);
    }
    request.headers.remove(AUTHORIZATION);
    assert!(request.url.starts_with(base));
    assert!(!request.headers.contains_key(BETA_HEADER));
    Ok(())
}

fn error_type(exception: &str) -> &'static str {
    match exception {
        "throttlingException" => "rate_limit_error",
        "serviceUnavailableException" | "modelNotReadyException" => "overloaded_error",
        "validationException" => "invalid_request_error",
        "accessDeniedException" => "permission_error",
        _ => "api_error",
    }
}

fn sse(event: &str, data: &str) -> Vec<u8> {
    format!("event: {event}\ndata: {data}\n\n").into_bytes()
}

fn exception(message: &Message) -> Vec<u8> {
    let kind = message
        .text(":exception-type")
        .or_else(|| message.text(":error-code"))
        .unwrap_or("unknown");
    let payload = serde_json::from_slice::<Value>(message.payload()).ok();
    let detail = payload
        .as_ref()
        .and_then(|payload| payload.get("message").and_then(Value::as_str))
        .or_else(|| message.text(":error-message"))
        .unwrap_or("Amazon Bedrock ended the stream with an exception");
    let error = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type(kind),
            "message": format!("Amazon Bedrock {kind}: {detail}"),
        },
    });
    sse("error", &error.to_string())
}

fn chunk(message: &Message) -> Result<Vec<u8>, Error> {
    let wrapper: Value = serde_json::from_slice(message.payload()).map_err(|error| {
        Error::new(format!(
            "Amazon Bedrock sent a chunk that is not JSON: {error}"
        ))
    })?;
    let encoded = wrapper
        .get("bytes")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::new("Amazon Bedrock sent a chunk with no bytes"))?;
    let decoded = STANDARD.decode(encoded).map_err(|error| {
        Error::new(format!(
            "Amazon Bedrock sent a chunk that is not base64: {error:?}"
        ))
    })?;
    let event: Value = serde_json::from_slice(&decoded).map_err(|error| {
        Error::new(format!(
            "Amazon Bedrock sent an event that is not JSON: {error}"
        ))
    })?;
    let kind = event.get("type").and_then(Value::as_str).ok_or_else(|| {
        Error::new(format!(
            "Amazon Bedrock sent an event with no type: {event}"
        ))
    })?;
    let data = String::from_utf8(decoded).map_err(|error| {
        Error::new(format!(
            "Amazon Bedrock sent an event that is not UTF-8: {error}"
        ))
    })?;
    Ok(sse(kind, &data))
}

fn translate(message: &Message) -> Result<Option<Vec<u8>>, Error> {
    match (message.text(":message-type"), message.text(":event-type")) {
        (Some("event"), Some("chunk")) => chunk(message).map(Some),
        (Some("event"), _) => Ok(None),
        _ => Ok(Some(exception(message))),
    }
}

struct Frames {
    body: Body,
    decoder: Decoder,
    done: bool,
}

async fn step(mut frames: Frames) -> Option<(Result<Vec<u8>, Error>, Frames)> {
    while !frames.done {
        match frames.decoder.next_message() {
            Ok(Some(message)) => match translate(&message) {
                Ok(Some(bytes)) => return Some((Ok(bytes), frames)),
                Ok(None) => continue,
                Err(error) => {
                    frames.done = true;
                    return Some((Err(error), frames));
                }
            },
            Err(error) => {
                frames.done = true;
                return Some((Err(Error::new(format!("Amazon Bedrock: {error}"))), frames));
            }
            Ok(None) => {}
        }
        match frames.body.next().await {
            Some(Ok(bytes)) => frames.decoder.push(&bytes),
            Some(Err(error)) => {
                frames.done = true;
                return Some((Err(error), frames));
            }
            None => {
                frames.done = true;
                if let Err(error) = frames.decoder.finish() {
                    return Some((Err(Error::new(format!("Amazon Bedrock: {error}"))), frames));
                }
            }
        }
    }
    None
}

pub fn response(response: Response) -> Response {
    let streamed = response
        .headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with(EVENT_STREAM));
    if !streamed {
        return response;
    }
    let (status, mut headers, body, _) = response.into_parts();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(SERVER_SENT_EVENTS));
    let frames = Frames {
        body,
        decoder: Decoder::default(),
        done: false,
    };
    Response::new(
        status,
        headers,
        Box::pin(nanocode_sync::stream::unfold(frames, step)),
    )
}
