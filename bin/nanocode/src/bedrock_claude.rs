use nanocode_aws::{Authorized, AwsAuth, Scope};
use nanocode_http::Method;
use nanocode_http::layer::{BoxFuture, Error, Layer, Next, Request, Response, Sender};
use nanocode_http::types::header::AUTHORIZATION;
use nanocode_sync::{Atom, SingleFlight, StreamExt};
use std::sync::Arc;

pub const USE_BEDROCK: &str = "CLAUDE_CODE_USE_BEDROCK";
pub const USE_MANTLE: &str = "CLAUDE_CODE_USE_MANTLE";
const MANTLE_BASE_URL: &str = "ANTHROPIC_BEDROCK_MANTLE_BASE_URL";
const SKIP_MANTLE_AUTH: &str = "CLAUDE_CODE_SKIP_MANTLE_AUTH";
const SKIP_BEDROCK_AUTH: &str = "CLAUDE_CODE_SKIP_BEDROCK_AUTH";
const DEFAULT_REGION: &str = "us-east-1";
const MANTLE_SERVICE: &str = "bedrock-mantle";
const MODEL_PREFIX: &str = "anthropic.";
const REGION_PREFIX: &str = "ANTHROPIC_BEDROCK_REGION_PREFIX";
const REGION_PREFIXES: [&str; 6] = ["global", "us", "eu", "apac", "jp", "au"];
const FAMILY_MODELS: [(&str, &[&str]); 3] = [
    ("opus", &["ANTHROPIC_DEFAULT_OPUS_MODEL"]),
    ("sonnet", &["ANTHROPIC_DEFAULT_SONNET_MODEL"]),
    (
        "haiku",
        &[
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_SMALL_FAST_MODEL",
        ],
    ),
];
const EXPIRED_MARKER: &str = "expired";
const FORBIDDEN: u16 = 403;
const UNAUTHORIZED: u16 = 401;
const BAD_REQUEST: u16 = 400;

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn flag(name: &str) -> bool {
    env(name)
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

pub fn enabled() -> bool {
    flag(USE_BEDROCK) || flag(USE_MANTLE)
}

fn default_model(model: &str) -> Result<String, Error> {
    if flag(USE_MANTLE) {
        return Ok(format!("{MODEL_PREFIX}{model}"));
    }
    let prefix = match env(REGION_PREFIX) {
        Some(prefix) if REGION_PREFIXES.contains(&prefix.as_str()) => prefix,
        Some(prefix) => {
            return Err(Error::new(format!(
                "Claude on Amazon Bedrock: {REGION_PREFIX}={prefix} is not one of {}",
                REGION_PREFIXES.join(", ")
            )));
        }
        None => REGION_PREFIXES[0].to_owned(),
    };
    Ok(format!("{prefix}.{MODEL_PREFIX}{model}"))
}

fn bedrock_model(model: &str) -> Result<String, Error> {
    if model.starts_with("arn:") || model.contains(MODEL_PREFIX) {
        return Ok(model.to_owned());
    }
    let lower = model.to_ascii_lowercase();
    match FAMILY_MODELS
        .iter()
        .filter(|(family, _)| lower.contains(family))
        .flat_map(|(_, names)| names.iter())
        .find_map(|name| env(name))
    {
        Some(configured) => Ok(configured),
        None => default_model(model),
    }
}

fn is_mantle_model(model: &str) -> bool {
    model.starts_with(MODEL_PREFIX) && !model.contains(':')
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    Mantle,
    Runtime,
}

impl Route {
    const fn service(self) -> &'static str {
        match self {
            Self::Mantle => MANTLE_SERVICE,
            Self::Runtime => crate::bedrock_runtime::SERVICE,
        }
    }

    fn skips_auth(self) -> bool {
        match self {
            Self::Mantle => flag(SKIP_MANTLE_AUTH),
            Self::Runtime => flag(SKIP_BEDROCK_AUTH),
        }
    }
}

#[derive(Clone)]
struct Resolved {
    auth: Result<AwsAuth, String>,
    region: String,
    mantle: String,
    runtime: String,
}

struct Shared {
    sender: Arc<dyn Sender>,
    resolved: Atom<Option<Resolved>>,
    flight: SingleFlight<Result<Resolved, String>>,
}

#[derive(Clone)]
pub struct BedrockClaude {
    shared: Arc<Shared>,
}

fn host_region(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let host = rest.split(['/', ':', '?']).next()?;
    let mut labels = host.split('.');
    let service = labels.next()?;
    let region = labels.next()?;
    let domain = labels.collect::<Vec<_>>().join(".");
    let known = matches!(service, "bedrock-mantle" | "bedrock-runtime")
        && matches!(domain.as_str(), "api.aws" | "amazonaws.com");
    known.then(|| region.to_owned())
}

fn mantle_base(region: &str) -> String {
    env(MANTLE_BASE_URL)
        .unwrap_or_else(|| format!("https://bedrock-mantle.{region}.api.aws/anthropic"))
        .trim_end_matches('/')
        .to_owned()
}

impl BedrockClaude {
    pub fn from_env() -> Result<Self, String> {
        let sender = nanocode::antigravity::Client::native_sender()
            .map_err(|error| format!("{USE_BEDROCK}: {error}"))?;
        Ok(Self {
            shared: Arc::new(Shared {
                sender,
                resolved: Atom::new(None),
                flight: SingleFlight::default(),
            }),
        })
    }

    pub fn messages_url() -> String {
        let region = ["AWS_REGION", "AWS_DEFAULT_REGION"]
            .iter()
            .find_map(|name| env(name))
            .unwrap_or_else(|| DEFAULT_REGION.to_owned());
        format!("{}/v1/messages", mantle_base(&region))
    }

    async fn resolve(&self) -> Result<Resolved, String> {
        if let Some(resolved) = self.shared.resolved.read(Clone::clone) {
            return Ok(resolved);
        }
        let shared = Arc::clone(&self.shared);
        self.shared
            .flight
            .run(move || async move {
                let lookup = |name: &str| std::env::var(name).ok();
                let region = nanocode_aws::region(&lookup)
                    .await
                    .unwrap_or_else(|| DEFAULT_REGION.to_owned());
                let auth = AwsAuth::discover(&lookup, Arc::clone(&shared.sender))
                    .await
                    .map_err(|error| format!("Claude on Amazon Bedrock ({USE_BEDROCK}): {error}"));
                let resolved = Resolved {
                    auth,
                    mantle: mantle_base(&region),
                    runtime: crate::bedrock_runtime::base(&region),
                    region,
                };
                shared.resolved.set(Some(resolved.clone()));
                Ok(resolved)
            })
            .await
    }

    fn rewrite(request: &mut Request, resolved: &Resolved) -> Result<Route, Error> {
        let mut body: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&request.body).map_err(|error| {
                Error::new(format!(
                    "Claude on Amazon Bedrock: the Messages request is not a JSON object: {error}"
                ))
            })?;
        let requested = body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                Error::new("Claude on Amazon Bedrock: the Messages request names no model")
            })?;
        let model = bedrock_model(requested)?;
        if !is_mantle_model(&model) {
            crate::bedrock_runtime::rewrite(request, body, &resolved.runtime, &model)?;
            return Ok(Route::Runtime);
        }
        if model != requested {
            body.insert("model".to_owned(), serde_json::Value::String(model));
            request.body = serde_json::to_vec(&body)
                .map_err(|error| Error::new(format!("Claude on Amazon Bedrock: {error}")))?
                .into();
        }
        request.url = format!("{}/v1/messages", resolved.mantle);
        request.headers.remove("x-api-key");
        request.headers.remove(AUTHORIZATION);
        Ok(Route::Mantle)
    }

    async fn authorize(
        resolved: &Resolved,
        route: Route,
        request: &mut Request,
    ) -> Result<Option<Authorized>, Error> {
        if route.skips_auth() {
            return Ok(None);
        }
        let region = host_region(&request.url).unwrap_or_else(|| resolved.region.clone());
        let scope = Scope {
            region: &region,
            service: route.service(),
        };
        let auth = resolved.auth.as_ref().map_err(Error::new)?;
        auth.authorize(request, scope)
            .await
            .map(Some)
            .map_err(|error| Error::new(format!("Claude on Amazon Bedrock: {error}")))
    }
}

async fn buffered(response: Response) -> Result<(Response, Vec<u8>), Error> {
    let (status, headers, mut stream, _) = response.into_parts();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
    }
    let replay = nanocode_sync::stream::iter([Ok(body.clone())]);
    Ok((Response::new(status, headers, Box::pin(replay)), body))
}

fn expired(status: u16, body: &[u8]) -> bool {
    matches!(status, FORBIDDEN | UNAUTHORIZED | BAD_REQUEST)
        && String::from_utf8_lossy(body)
            .to_ascii_lowercase()
            .contains(EXPIRED_MARKER)
}

impl Layer for BedrockClaude {
    fn send<'a>(
        &'a self,
        mut request: Request,
        next: Next<'a>,
    ) -> BoxFuture<'a, Result<Response, Error>> {
        Box::pin(async move {
            if request.method != Method::POST || !request.url.ends_with("/messages") {
                return next.send(request).await;
            }
            let resolved = self.resolve().await.map_err(Error::new)?;
            let route = Self::rewrite(&mut request, &resolved)?;
            let translate = |response: Response| match route {
                Route::Mantle => response,
                Route::Runtime => crate::bedrock_runtime::response(response),
            };
            let retry = request.clone();
            let authorized = Self::authorize(&resolved, route, &mut request).await?;
            let response = next.send(request).await?;
            let Some(authorized) = authorized else {
                return Ok(translate(response));
            };
            if response.status.is_success() || authorized == Authorized::Bearer {
                return Ok(translate(response));
            }
            let status = response.status.as_u16();
            let (response, body) = buffered(response).await?;
            if !expired(status, &body) {
                return Ok(response);
            }
            if let Ok(auth) = &resolved.auth {
                auth.invalidate(&authorized);
            }
            let mut retry = retry;
            Self::authorize(&resolved, route, &mut retry).await?;
            next.send(retry).await.map(translate)
        })
    }
}
