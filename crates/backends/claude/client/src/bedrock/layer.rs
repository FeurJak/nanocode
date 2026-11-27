use super::{BedrockConfig, DEFAULT_REGION, Lookup, is_mantle_model, runtime};
use nanocode_aws::{Authorized, AwsAuth, Scope};
use nanocode_http::Method;
use nanocode_http::layer::{BoxFuture, Error, Layer, Next, Request, Response, Sender};
use nanocode_http::types::header::AUTHORIZATION;
use nanocode_sync::{Atom, SingleFlight, StreamExt};
use std::sync::Arc;

const MANTLE_SERVICE: &str = "bedrock-mantle";
const EXPIRED_MARKER: &str = "expired";
const FORBIDDEN: u16 = 403;
const UNAUTHORIZED: u16 = 401;
const BAD_REQUEST: u16 = 400;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    Mantle,
    Runtime,
}

#[derive(Clone)]
struct Resolved {
    auth: Result<AwsAuth, String>,
    region: String,
    mantle: String,
    runtime: String,
}

struct Shared {
    config: BedrockConfig,
    lookup: Lookup,
    sender: Arc<dyn Sender>,
    resolved: Atom<Option<Resolved>>,
    flight: SingleFlight<Result<Resolved, String>>,
}

#[derive(Clone)]
pub struct Bedrock {
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

impl Bedrock {
    #[must_use]
    pub fn new(config: BedrockConfig, lookup: Lookup, sender: Arc<dyn Sender>) -> Self {
        Self {
            shared: Arc::new(Shared {
                config,
                lookup,
                sender,
                resolved: Atom::new(None),
                flight: SingleFlight::default(),
            }),
        }
    }

    #[must_use]
    pub fn config(&self) -> &BedrockConfig {
        &self.shared.config
    }

    pub(super) fn lookup(&self) -> Lookup {
        Arc::clone(&self.shared.lookup)
    }

    async fn resolve(&self) -> Result<Resolved, String> {
        if let Some(resolved) = self.shared.resolved.read(Clone::clone) {
            return Ok(resolved);
        }
        let shared = Arc::clone(&self.shared);
        self.shared
            .flight
            .run(move || async move {
                let lookup = |name: &str| (shared.lookup)(name);
                let region = nanocode_aws::region(&lookup)
                    .await
                    .unwrap_or_else(|| DEFAULT_REGION.to_owned());
                let auth = AwsAuth::discover(&lookup, Arc::clone(&shared.sender))
                    .await
                    .map_err(|error| format!("Claude on Amazon Bedrock: {error}"));
                let resolved = Resolved {
                    auth,
                    mantle: shared.config.mantle_base(&region),
                    runtime: shared.config.runtime_base(&region),
                    region,
                };
                shared.resolved.set(Some(resolved.clone()));
                Ok(resolved)
            })
            .await
    }

    fn rewrite(&self, request: &mut Request, resolved: &Resolved) -> Result<Route, Error> {
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
        let model = self.shared.config.model_id(requested);
        if !is_mantle_model(&model) {
            runtime::rewrite(request, body, &resolved.runtime, &model)?;
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
        &self,
        resolved: &Resolved,
        route: Route,
        request: &mut Request,
    ) -> Result<Option<Authorized>, Error> {
        let (skip, service) = match route {
            Route::Mantle => (self.shared.config.skip_mantle_auth, MANTLE_SERVICE),
            Route::Runtime => (self.shared.config.skip_runtime_auth, runtime::SERVICE),
        };
        if skip {
            return Ok(None);
        }
        let region = host_region(&request.url).unwrap_or_else(|| resolved.region.clone());
        let scope = Scope {
            region: &region,
            service,
        };
        let auth = resolved.auth.as_ref().map_err(Error::new)?;
        auth.authorize(request, scope)
            .await
            .map(Some)
            .map_err(|error| Error::new(format!("Claude on Amazon Bedrock: {error}")))
    }
}

impl Layer for Bedrock {
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
            let route = self.rewrite(&mut request, &resolved)?;
            let translate = |response: Response| match route {
                Route::Mantle => response,
                Route::Runtime => runtime::response(response),
            };
            let retry = request.clone();
            let authorized = self.authorize(&resolved, route, &mut request).await?;
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
            self.authorize(&resolved, route, &mut retry).await?;
            next.send(retry).await.map(translate)
        })
    }
}
