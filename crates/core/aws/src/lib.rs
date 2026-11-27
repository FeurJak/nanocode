mod credentials;
mod discover;
mod encode;
pub mod event_stream;
mod sign;
mod source;
mod time;

use nanocode_http::layer::{Request, Sender};
use nanocode_http::types::header::{AUTHORIZATION, HeaderValue};
use nanocode_sync::{Atom, SingleFlight};
use std::sync::Arc;

pub use credentials::{Credentials, REFRESH_MARGIN};
pub use discover::{BEARER_TOKEN, region};
pub use encode::{UtcTime, hex, uri_encode, utc};
pub use sign::{
    ALGORITHM, Canonical, SHA256_BYTES, Scope, hmac_sha256, sign_request, signature,
    signed_header_names,
};
pub use time::parse_rfc3339;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    pub(crate) fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }

    pub fn detail(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

struct Signed {
    source: source::Source,
    sender: Arc<dyn Sender>,
    cached: Atom<Option<Credentials>>,
    flight: SingleFlight<Result<Credentials, Error>>,
}

enum Inner {
    Bearer(String),
    Signed(Signed),
}

#[derive(Clone)]
pub struct AwsAuth {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for AwsAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AwsAuth")
            .field("origin", &self.origin())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Authorized {
    Bearer,
    Signed { access_key_id: String },
}

impl AwsAuth {
    pub async fn discover(
        lookup: &(dyn Fn(&str) -> Option<String> + Sync),
        sender: Arc<dyn Sender>,
    ) -> Result<Self, Error> {
        let inner = match discover::find(lookup).await? {
            discover::Found::Bearer(token) => Inner::Bearer(token),
            discover::Found::Signed(source) => Inner::Signed(Signed {
                source,
                sender,
                cached: Atom::new(None),
                flight: SingleFlight::default(),
            }),
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    pub fn origin(&self) -> String {
        match self.inner.as_ref() {
            Inner::Bearer(_) => BEARER_TOKEN.to_owned(),
            Inner::Signed(signed) => signed.source.origin(),
        }
    }

    pub fn is_bearer(&self) -> bool {
        matches!(self.inner.as_ref(), Inner::Bearer(_))
    }

    pub async fn credentials(&self) -> Result<Option<Credentials>, Error> {
        let Inner::Signed(signed) = self.inner.as_ref() else {
            return Ok(None);
        };
        let fresh = || {
            let now = nanocode_platform::system_time();
            signed
                .cached
                .read(|cached| cached.as_ref().filter(|c| c.is_fresh(now)).cloned())
        };
        if let Some(credentials) = fresh() {
            return Ok(Some(credentials));
        }
        signed
            .flight
            .run(|| async {
                if let Some(credentials) = fresh() {
                    return Ok(credentials);
                }
                let credentials =
                    signed
                        .source
                        .fetch(signed.sender.as_ref())
                        .await
                        .map_err(|error| {
                            Error::new(format!(
                                "AWS credentials from {} failed: {error}",
                                signed.source.origin()
                            ))
                        })?;
                signed.cached.set(Some(credentials.clone()));
                Ok(credentials)
            })
            .await
            .map(Some)
    }

    pub async fn authorize(
        &self,
        request: &mut Request,
        scope: Scope<'_>,
    ) -> Result<Authorized, Error> {
        let Some(credentials) = self.credentials().await? else {
            let Inner::Bearer(token) = self.inner.as_ref() else {
                return Err(Error::new("AWS credentials resolved to nothing"));
            };
            let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|error| Error::new(format!("{BEARER_TOKEN}: {error}")))?;
            value.set_sensitive(true);
            request.headers.remove("x-api-key");
            request.headers.insert(AUTHORIZATION, value);
            return Ok(Authorized::Bearer);
        };
        let unix_seconds = nanocode_platform::system_time()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| Error::new(format!("the clock is before 1970: {error}")))?
            .as_secs();
        sign_request(request, &credentials, scope, unix_seconds)?;
        Ok(Authorized::Signed {
            access_key_id: credentials.access_key_id().to_owned(),
        })
    }

    pub fn invalidate(&self, authorized: &Authorized) {
        let (Inner::Signed(signed), Authorized::Signed { access_key_id }) =
            (self.inner.as_ref(), authorized)
        else {
            return;
        };
        signed.cached.update(|cached| {
            if cached
                .as_ref()
                .is_some_and(|credentials| credentials.access_key_id() == access_key_id)
            {
                *cached = None;
            }
        });
    }
}
