mod layer;
mod runtime;

use crate::{ClaudeClient, HourCache, InferenceRoute, transport::HttpClient};
use std::sync::Arc;

pub use layer::Bedrock;

pub const USE_BEDROCK: &str = "CLAUDE_CODE_USE_BEDROCK";
pub const USE_MANTLE: &str = "CLAUDE_CODE_USE_MANTLE";
const MANTLE_BASE_URL: &str = "ANTHROPIC_BEDROCK_MANTLE_BASE_URL";
const RUNTIME_BASE_URL: &str = "ANTHROPIC_BEDROCK_BASE_URL";
const SKIP_MANTLE_AUTH: &str = "CLAUDE_CODE_SKIP_MANTLE_AUTH";
const SKIP_RUNTIME_AUTH: &str = "CLAUDE_CODE_SKIP_BEDROCK_AUTH";
const HOUR_CACHE: &str = "ENABLE_PROMPT_CACHING_1H";
const REGION_PREFIX: &str = "ANTHROPIC_BEDROCK_REGION_PREFIX";
const REGION_VARIABLES: [&str; 2] = ["AWS_REGION", "AWS_DEFAULT_REGION"];
pub const DEFAULT_REGION: &str = "us-east-1";
pub const REGION_PREFIXES: [&str; 6] = ["global", "us", "eu", "apac", "jp", "au"];
const MODEL_PREFIX: &str = "anthropic.";
const RUNTIME_FALLBACKS: [&str; 3] = ["claude-opus-5", "claude-opus-4-8", "claude-sonnet-5"];
const FAMILIES: [&str; 3] = ["opus", "sonnet", "haiku"];
const FAMILY_PINS: [&[&str]; 3] = [
    &["ANTHROPIC_DEFAULT_OPUS_MODEL"],
    &["ANTHROPIC_DEFAULT_SONNET_MODEL"],
    &[
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_SMALL_FAST_MODEL",
    ],
];

pub type Lookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BedrockError(String);

impl BedrockError {
    pub(crate) fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BedrockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BedrockError {}

fn present(lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    lookup(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn flag(lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> bool {
    present(lookup, name)
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

pub fn enabled(lookup: &dyn Fn(&str) -> Option<String>) -> bool {
    flag(lookup, USE_BEDROCK) || flag(lookup, USE_MANTLE)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BedrockConfig {
    mantle: bool,
    region_prefix: &'static str,
    pins: [Option<String>; 3],
    mantle_base: Option<String>,
    runtime_base: Option<String>,
    skip_mantle_auth: bool,
    skip_runtime_auth: bool,
    hour_cache: HourCache,
}

impl Default for BedrockConfig {
    fn default() -> Self {
        Self {
            mantle: false,
            region_prefix: REGION_PREFIXES[0],
            pins: [None, None, None],
            mantle_base: None,
            runtime_base: None,
            skip_mantle_auth: false,
            skip_runtime_auth: false,
            hour_cache: HourCache::AfterIdle,
        }
    }
}

impl BedrockConfig {
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self, BedrockError> {
        let region_prefix = match present(lookup, REGION_PREFIX) {
            Some(prefix) => REGION_PREFIXES
                .into_iter()
                .find(|known| *known == prefix)
                .ok_or_else(|| {
                    BedrockError::new(format!(
                        "Claude on Amazon Bedrock: {REGION_PREFIX}={prefix} is not one of {}",
                        REGION_PREFIXES.join(", ")
                    ))
                })?,
            None => REGION_PREFIXES[0],
        };
        let pins = FAMILY_PINS.map(|names| names.iter().find_map(|name| present(lookup, name)));
        let base = |name| present(lookup, name).map(|url| url.trim_end_matches('/').to_owned());
        Ok(Self {
            mantle: flag(lookup, USE_MANTLE),
            region_prefix,
            pins,
            mantle_base: base(MANTLE_BASE_URL),
            runtime_base: base(RUNTIME_BASE_URL),
            skip_mantle_auth: flag(lookup, SKIP_MANTLE_AUTH),
            skip_runtime_auth: flag(lookup, SKIP_RUNTIME_AUTH),
            hour_cache: match flag(lookup, HOUR_CACHE) {
                true => HourCache::Always,
                false => HourCache::AfterIdle,
            },
        })
    }

    #[must_use]
    pub const fn with_mantle(mut self, mantle: bool) -> Self {
        self.mantle = mantle;
        self
    }

    pub fn with_region_prefix(mut self, prefix: &str) -> Result<Self, BedrockError> {
        self.region_prefix = REGION_PREFIXES
            .into_iter()
            .find(|known| *known == prefix)
            .ok_or_else(|| BedrockError::new(format!("unknown Bedrock region prefix {prefix}")))?;
        Ok(self)
    }

    #[must_use]
    pub const fn with_hour_cache(mut self, hour_cache: HourCache) -> Self {
        self.hour_cache = hour_cache;
        self
    }

    #[must_use]
    pub const fn hour_cache(&self) -> HourCache {
        self.hour_cache
    }

    fn pin(&self, model: &str) -> Option<&str> {
        let lower = model.to_ascii_lowercase();
        FAMILIES
            .iter()
            .zip(&self.pins)
            .find(|(family, _)| lower.contains(*family))
            .and_then(|(_, pin)| pin.as_deref())
    }

    fn default_model(&self, model: &str) -> String {
        match self.mantle {
            true => format!("{MODEL_PREFIX}{model}"),
            false => format!("{}.{MODEL_PREFIX}{model}", self.region_prefix),
        }
    }

    #[must_use]
    pub fn model_id(&self, model: &str) -> String {
        if model.starts_with("arn:") || model.contains(MODEL_PREFIX) {
            return model.to_owned();
        }
        self.pin(model)
            .map_or_else(|| self.default_model(model), str::to_owned)
    }

    #[must_use]
    pub fn inference_route(&self, model: &str) -> InferenceRoute {
        let id = self.model_id(model);
        let regional = REGION_PREFIXES[1..]
            .iter()
            .any(|prefix| id.starts_with(&format!("{prefix}.{MODEL_PREFIX}")));
        match regional {
            true => InferenceRoute::Regional,
            false => InferenceRoute::Global,
        }
    }

    #[must_use]
    pub fn refusal_fallbacks(&self) -> Vec<String> {
        RUNTIME_FALLBACKS
            .iter()
            .filter(|model| {
                crate::sim::mutated(crate::sim::Mutation::FallbackUnchecked)
                    || self.pin(model).is_none()
                    && self.model_id(model)
                        == format!("{}.{MODEL_PREFIX}{model}", REGION_PREFIXES[0])
            })
            .map(|model| (*model).to_owned())
            .collect()
    }

    fn mantle_base(&self, region: &str) -> String {
        self.mantle_base
            .clone()
            .unwrap_or_else(|| format!("https://bedrock-mantle.{region}.api.aws/anthropic"))
    }

    fn runtime_base(&self, region: &str) -> String {
        self.runtime_base
            .clone()
            .unwrap_or_else(|| format!("https://bedrock-runtime.{region}.amazonaws.com"))
    }
}

fn is_mantle_model(model: &str) -> bool {
    model.starts_with(MODEL_PREFIX) && !model.contains(':')
}

impl Bedrock {
    #[cfg(not(target_family = "wasm"))]
    pub fn from_env() -> Result<Self, BedrockError> {
        let lookup: Lookup = Arc::new(|name: &str| std::env::var(name).ok());
        let config = BedrockConfig::from_lookup(lookup.as_ref())?;
        Ok(Self::new(config, lookup, native_sender()))
    }

    #[must_use]
    pub fn messages_url(&self) -> String {
        let lookup = self.lookup();
        let region = REGION_VARIABLES
            .iter()
            .find_map(|name| present(lookup.as_ref(), name))
            .unwrap_or_else(|| DEFAULT_REGION.to_owned());
        format!("{}/v1/messages", self.config().mantle_base(&region))
    }
}

#[cfg(not(target_family = "wasm"))]
struct NativeSender(nanocode_http::Client);

#[cfg(not(target_family = "wasm"))]
impl nanocode_http::layer::Sender for NativeSender {
    fn send(
        &self,
        request: nanocode_http::layer::Request,
    ) -> nanocode_http::layer::BoxFuture<
        '_,
        Result<nanocode_http::layer::Response, nanocode_http::layer::Error>,
    > {
        Box::pin(async move {
            nanocode_http::ClientSender::new(&self.0)
                .send(request)
                .await
        })
    }
}

#[cfg(not(target_family = "wasm"))]
#[must_use]
pub fn native_sender() -> Arc<dyn nanocode_http::layer::Sender> {
    Arc::new(NativeSender(ClaudeClient::http_client()))
}

impl ClaudeClient {
    #[must_use]
    pub fn bedrock(http: HttpClient, bedrock: Bedrock) -> Self {
        let config = bedrock.config().clone();
        let route = config.clone();
        Self::with_auth_headers(
            http,
            bedrock.messages_url(),
            nanocode_http::types::header::HeaderMap::new(),
        )
        .with_hour_cache(config.hour_cache())
        .with_refusal_fallbacks(config.refusal_fallbacks())
        .with_inference_route(move |model| route.inference_route(model))
        .with_http_layer(bedrock)
    }
}
