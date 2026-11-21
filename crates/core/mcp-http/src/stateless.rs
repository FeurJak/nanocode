use std::{fmt, sync::Arc, time::Duration};

use nanocode_http::layer::Sender;
use nanocode_http::types::{
    HeaderMap, HeaderName, HeaderValue, Method, StatusCode,
    header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE},
};
use nanocode_mcp::{
    Implementation, Message, RequestId, RpcError,
    protocol::{
        CLIENT_CAPABILITIES_META_KEY, CLIENT_INFO_META_KEY, INITIALIZE, InitializeParams,
        JsonObject, PROMPTS_GET, PROTOCOL_VERSION_META_KEY, RESOURCES_READ, SERVER_DISCOVER,
        STATELESS_PROTOCOL_VERSION, STATELESS_SUPPORTED_PROTOCOL_VERSIONS, TOOLS_CALL,
        UNSUPPORTED_PROTOCOL_VERSION,
    },
    sse,
};
use nanocode_platform::timeout;
use nanocode_sync::AtomMap;
use serde_json::Value;

use crate::streamable_http::{
    Client, EVENT_STREAM_MIME_TYPE, HEADER_PROTOCOL_VERSION, HEADER_SESSION_ID, JSON_MIME_TYPE,
    Reply, content_type, is_mime,
};

const HEADER_METHOD: HeaderName = HeaderName::from_static("mcp-method");
const HEADER_NAME: HeaderName = HeaderName::from_static("mcp-name");
const ACCEPT_STATELESS: &str = "application/json, text/event-stream";
const REQUEST_ID: i64 = 1;
const CLOUDFLARE_TIMEOUT: u16 = 524;
const HEADER_NAME_BYTES: std::ops::Range<u8> = 0x20..0x7f;
const SSE_EVENT_END: &[u8] = b"\n\n";

pub struct StatelessOptions {
    pub client_info: Implementation,
    pub client_capabilities: JsonObject,
    pub max_response_bytes: usize,
    pub request_timeout: Duration,
    pub versions_kept_max: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StatelessError {
    Unsent(String),
    Unanswered(String),
    Status {
        endpoint: String,
        status: u16,
    },
    Rpc(RpcError),
    UnsupportedVersions {
        endpoint: String,
        versions: Vec<String>,
    },
    Sessions {
        endpoint: String,
    },
}

impl fmt::Display for StatelessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsent(message) | Self::Unanswered(message) => f.write_str(message),
            Self::Status { endpoint, status } => write!(f, "{endpoint} answered HTTP {status}"),
            Self::Rpc(error) => write!(f, "{error}"),
            Self::UnsupportedVersions { endpoint, versions } => write!(
                f,
                "{endpoint} supports MCP {}, none of which this client speaks",
                versions.join(", ")
            ),
            Self::Sessions { endpoint } => {
                write!(f, "{endpoint} uses MCP sessions; this client is stateless")
            }
        }
    }
}

impl std::error::Error for StatelessError {}

pub struct Stateless {
    client: Client,
    options: StatelessOptions,
    client_info: Value,
    versions: AtomMap<String, &'static str>,
}

struct Answer {
    status: StatusCode,
    session: bool,
    event_stream: bool,
    body: Vec<u8>,
}

impl Stateless {
    #[cfg(not(target_family = "wasm"))]
    #[must_use]
    pub fn new(client: nanocode_http::Client, options: StatelessOptions) -> Self {
        Self::with_client(Client::Reqwest(client), options)
    }

    #[must_use]
    pub fn with_sender(
        sender: Arc<dyn Sender>,
        headers: HeaderMap,
        options: StatelessOptions,
    ) -> Self {
        Self::with_client(Client::Sender { sender, headers }, options)
    }

    fn with_client(client: Client, options: StatelessOptions) -> Self {
        assert!(options.versions_kept_max > 0);
        assert!(options.max_response_bytes > 0);
        let client_info = serde_json::to_value(&options.client_info)
            .expect("an Implementation serializes to JSON");
        Self {
            client,
            options,
            client_info,
            versions: AtomMap::new(),
        }
    }

    pub async fn negotiate(
        &self,
        endpoint: &str,
        headers: &HeaderMap,
    ) -> Result<(&'static str, Value), StatelessError> {
        let negotiated = match self.discover(endpoint, headers).await {
            Ok(Some(found)) => Ok(found),
            Ok(None) => self.initialize(endpoint, headers).await,
            Err(error) => Err(error),
        }
        .map_err(|error| match error {
            StatelessError::Unanswered(message) => StatelessError::Unsent(message),
            error => error,
        })?;
        self.keep(endpoint, negotiated.0);
        Ok(negotiated)
    }

    pub async fn request(
        &self,
        endpoint: &str,
        method: &str,
        params: JsonObject,
        headers: &HeaderMap,
    ) -> Result<Value, StatelessError> {
        let mut params = self.with_meta(params);
        let version = match self.versions.get(endpoint) {
            Some(version) => version,
            None => self.negotiate(endpoint, headers).await?.0,
        };
        let mut sent = version;
        let mut answer = self
            .post(endpoint, method, &mut params, Some(version), headers)
            .await?;
        if let Some(spoken) = refusal(&answer).and_then(|supported| newer(&supported, version)) {
            nanocode_trace::info!(
                endpoint = endpoint,
                version = version,
                spoken = spoken,
                "MCP server refused the kept protocol version"
            );
            self.keep(endpoint, spoken);
            sent = spoken;
            answer = self
                .post(endpoint, method, &mut params, Some(spoken), headers)
                .await?;
        }
        if answer.status == StatusCode::BAD_REQUEST {
            self.versions.remove_if(endpoint, |kept| *kept == sent);
        }
        read(endpoint, answer)
    }

    async fn discover(
        &self,
        endpoint: &str,
        headers: &HeaderMap,
    ) -> Result<Option<(&'static str, Value)>, StatelessError> {
        let mut params = self.with_meta(JsonObject::new());
        let mut version = STATELESS_PROTOCOL_VERSION;
        let mut answer = self
            .post(
                endpoint,
                SERVER_DISCOVER,
                &mut params,
                Some(version),
                headers,
            )
            .await?;
        if let Some(supported) = refusal(&answer) {
            version =
                newer(&supported, version).ok_or_else(|| StatelessError::UnsupportedVersions {
                    endpoint: endpoint.to_owned(),
                    versions: supported,
                })?;
            answer = self
                .post(
                    endpoint,
                    SERVER_DISCOVER,
                    &mut params,
                    Some(version),
                    headers,
                )
                .await?;
        }
        let Ok(result) = read(endpoint, answer) else {
            return Ok(None);
        };
        let spoken = result
            .get("supportedVersions")
            .and_then(Value::as_array)
            .map_or(Some(version), |listed| {
                newest(listed.iter().filter_map(Value::as_str)).filter(|spoken| stateless(spoken))
            });
        Ok(spoken.map(|spoken| (spoken, result)))
    }

    async fn initialize(
        &self,
        endpoint: &str,
        headers: &HeaderMap,
    ) -> Result<(&'static str, Value), StatelessError> {
        let legacy = STATELESS_SUPPORTED_PROTOCOL_VERSIONS
            .into_iter()
            .find(|version| !stateless(version))
            .expect("a legacy protocol version is supported");
        let initialize = InitializeParams {
            protocol_version: legacy.to_owned(),
            capabilities: self.options.client_capabilities.clone(),
            client_info: self.options.client_info.clone(),
        };
        let Ok(Value::Object(params)) = serde_json::to_value(initialize) else {
            unreachable!("initialize params serialize to a JSON object");
        };
        let mut params = self.with_meta(params);
        let answer = self
            .post(endpoint, INITIALIZE, &mut params, None, headers)
            .await?;
        let session = answer.session;
        let result = read(endpoint, answer)?;
        if session {
            return Err(StatelessError::Sessions {
                endpoint: endpoint.to_owned(),
            });
        }
        let answered = result.get("protocolVersion").and_then(Value::as_str);
        let spoken = newest(answered)
            .filter(|spoken| !stateless(spoken))
            .ok_or_else(|| StatelessError::UnsupportedVersions {
                endpoint: endpoint.to_owned(),
                versions: answered.map(str::to_owned).into_iter().collect(),
            })?;
        Ok((spoken, result))
    }

    fn keep(&self, endpoint: &str, version: &'static str) {
        let kept_max = self.options.versions_kept_max;
        self.versions.update(|versions| {
            if versions.len() >= kept_max && !versions.contains_key(endpoint) {
                versions.clear();
            }
            versions.insert(endpoint.to_owned(), version);
        });
    }

    fn with_meta(&self, mut params: JsonObject) -> JsonObject {
        let meta = params
            .entry("_meta")
            .or_insert_with(|| Value::Object(JsonObject::new()));
        if let Value::Object(meta) = meta {
            meta.insert(CLIENT_INFO_META_KEY.to_owned(), self.client_info.clone());
            meta.insert(
                CLIENT_CAPABILITIES_META_KEY.to_owned(),
                Value::Object(self.options.client_capabilities.clone()),
            );
        }
        params
    }

    async fn post(
        &self,
        endpoint: &str,
        method: &str,
        params: &mut JsonObject,
        version: Option<&'static str>,
        headers: &HeaderMap,
    ) -> Result<Answer, StatelessError> {
        if let Some(Value::Object(meta)) = params.get_mut("_meta") {
            match version.filter(|version| stateless(version)) {
                Some(version) => {
                    meta.insert(PROTOCOL_VERSION_META_KEY.to_owned(), version.into());
                }
                None => {
                    meta.remove(PROTOCOL_VERSION_META_KEY);
                }
            }
        }
        let headers = request_headers(endpoint, method, params, version, headers)?;
        let body = Message::Request {
            id: RequestId::Number(REQUEST_ID),
            method: method.to_owned(),
            params: Some(Value::Object(params.clone())),
        }
        .to_vec()
        .map_err(|error| StatelessError::Unsent(format!("encoding {method}: {error}")))?;
        let exchange = async {
            let reply = self
                .client
                .send(&Method::POST, endpoint, headers, &body)
                .await
                .map_err(|error| {
                    if error.is_unsent() {
                        StatelessError::Unsent(format!("reaching {endpoint}: {error}"))
                    } else {
                        StatelessError::Unanswered(format!("{endpoint} didn't answer: {error}"))
                    }
                })?;
            self.answer(endpoint, reply).await
        };
        timeout(self.options.request_timeout, exchange)
            .await
            .map_err(|_| {
                StatelessError::Unanswered(format!(
                    "{endpoint} didn't answer within {:?}",
                    self.options.request_timeout
                ))
            })?
    }

    async fn answer(&self, endpoint: &str, reply: Reply) -> Result<Answer, StatelessError> {
        let max_bytes = self.options.max_response_bytes;
        let status = reply.status;
        let session = reply.headers.contains_key(&HEADER_SESSION_ID);
        let event_stream = is_mime(content_type(&reply).as_deref(), EVENT_STREAM_MIME_TYPE);
        let declared_bytes = reply
            .headers
            .get(CONTENT_LENGTH)
            .and_then(|length| length.to_str().ok())
            .and_then(|length| length.parse::<usize>().ok());
        if declared_bytes.is_some_and(|bytes| bytes > max_bytes) {
            return Err(StatelessError::Unanswered(format!(
                "{endpoint}'s answer exceeds max_response_bytes of {max_bytes} bytes; its outcome is unknown"
            )));
        }
        let body = reply.bytes(Some(max_bytes)).await.map_err(|error| {
            StatelessError::Unanswered(format!("{endpoint}'s answer broke off: {error}"))
        })?;
        Ok(Answer {
            status,
            session,
            event_stream,
            body,
        })
    }
}

fn stateless(version: &str) -> bool {
    version >= STATELESS_PROTOCOL_VERSION
}

fn newest<'a>(supported: impl IntoIterator<Item = &'a str>) -> Option<&'static str> {
    let supported: Vec<&str> = supported.into_iter().collect();
    STATELESS_SUPPORTED_PROTOCOL_VERSIONS
        .into_iter()
        .find(|version| supported.contains(version))
}

fn newer(supported: &[String], refused: &str) -> Option<&'static str> {
    newest(supported.iter().map(String::as_str))
        .filter(|spoken| *spoken != refused && stateless(spoken))
}

fn call_name<'a>(method: &str, params: &'a JsonObject) -> Option<&'a str> {
    let key = match method {
        TOOLS_CALL | PROMPTS_GET => "name",
        RESOURCES_READ => "uri",
        _ => return None,
    };
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|name| name.bytes().all(|byte| HEADER_NAME_BYTES.contains(&byte)))
}

fn request_headers(
    endpoint: &str,
    method: &str,
    params: &JsonObject,
    version: Option<&'static str>,
    headers: &HeaderMap,
) -> Result<HeaderMap, StatelessError> {
    let invalid = |error: nanocode_http::types::header::InvalidHeaderValue| {
        StatelessError::Unsent(format!("reaching {endpoint}: {error}"))
    };
    let mut headers = headers.clone();
    headers.insert(ACCEPT, HeaderValue::from_static(ACCEPT_STATELESS));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(JSON_MIME_TYPE));
    headers.insert(
        HEADER_METHOD,
        HeaderValue::from_str(method).map_err(invalid)?,
    );
    if let Some(version) = version {
        headers.insert(HEADER_PROTOCOL_VERSION, HeaderValue::from_static(version));
    }
    if let Some(name) = call_name(method, params) {
        headers.insert(HEADER_NAME, HeaderValue::from_str(name).map_err(invalid)?);
    }
    Ok(headers)
}

fn refusal(answer: &Answer) -> Option<Vec<String>> {
    if answer.status != StatusCode::BAD_REQUEST {
        return None;
    }
    let message: Value = serde_json::from_slice(&answer.body).ok()?;
    let error = message.get("error")?;
    if error.get("code").and_then(Value::as_i64) != Some(UNSUPPORTED_PROTOCOL_VERSION) {
        return None;
    }
    Some(
        error
            .get("data")?
            .get("supported")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
    )
}

fn read(endpoint: &str, answer: Answer) -> Result<Value, StatelessError> {
    let status = answer.status;
    if status == StatusCode::GATEWAY_TIMEOUT || status.as_u16() == CLOUDFLARE_TIMEOUT {
        return Err(StatelessError::Unanswered(format!(
            "{endpoint} answered HTTP {status}"
        )));
    }
    if !status.is_success() {
        return Err(StatelessError::Status {
            endpoint: endpoint.to_owned(),
            status: status.as_u16(),
        });
    }
    let unreadable = |error: String| {
        StatelessError::Unanswered(format!("{endpoint}'s answer is unreadable: {error}"))
    };
    let mut message = if answer.event_stream {
        from_events(&answer.body)
    } else {
        serde_json::from_slice::<Value>(&answer.body).map_err(|error| error.to_string())
    }
    .map_err(unreadable)?;
    if let Some(error) = message.get_mut("error") {
        let error = serde_json::from_value::<RpcError>(error.take())
            .map_err(|error| unreadable(error.to_string()))?;
        return Err(StatelessError::Rpc(error));
    }
    message
        .get_mut("result")
        .map(Value::take)
        .ok_or_else(|| unreadable("it holds neither a result nor an error".to_owned()))
}

fn from_events(body: &[u8]) -> Result<Value, String> {
    let mut parser = sse::Parser::new(body.len() + SSE_EVENT_END.len());
    let mut events = Vec::new();
    parser
        .feed(body, &mut events)
        .and_then(|()| parser.feed(SSE_EVENT_END, &mut events))
        .and_then(|()| parser.finish(&mut events))
        .map_err(|error| error.to_string())?;
    for event in events {
        let Some(data) = event.data.filter(|data| !data.is_empty()) else {
            continue;
        };
        let Ok(message) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        if message.get("id").and_then(Value::as_i64) == Some(REQUEST_ID)
            && (message.get("result").is_some() || message.get("error").is_some())
        {
            return Ok(message);
        }
    }
    Err("its event stream ended without an answer".to_owned())
}
