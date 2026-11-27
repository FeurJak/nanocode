use crate::{Credentials, Error};
use nanocode_http::Method;
use nanocode_http::header::{HeaderMap, HeaderName, HeaderValue};
use nanocode_http::layer::{Request, Sender};
use nanocode_sync::StreamExt;
use serde_json::Value;
use std::time::Duration;

pub const CREDENTIAL_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(not(target_family = "wasm"))]
pub const UNEXPIRING_LIFETIME: Duration = Duration::from_secs(60 * 60);
const IMDS_TOKEN_TTL_SECONDS: &str = "21600";
const IMDS_TOKEN_TTL: &str = "x-aws-ec2-metadata-token-ttl-seconds";
const IMDS_TOKEN: &str = "x-aws-ec2-metadata-token";
const IMDS_ROLES: &str = "/latest/meta-data/iam/security-credentials/";

#[derive(Clone, Debug)]
pub enum Source {
    Static(Credentials),
    Process { profile: Option<String> },
    Container { url: String, token: Option<Token> },
    Instance { base: String },
}

#[derive(Clone, Debug)]
pub enum Token {
    Value(String),
    File(String),
}

struct Reply {
    status: u16,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).trim().to_owned()
    }

    fn ok(self, url: &str) -> Result<Self, Error> {
        match (200..300).contains(&self.status) {
            true => Ok(self),
            false => Err(Error::new(format!(
                "{url} returned {}: {}",
                self.status,
                self.text()
            ))),
        }
    }
}

async fn call(
    sender: &dyn Sender,
    method: Method,
    url: &str,
    headers: &[(&'static str, String)],
) -> Result<Reply, Error> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let mut value = HeaderValue::from_str(value)
            .map_err(|error| Error::new(format!("invalid {name} header for {url}: {error}")))?;
        value.set_sensitive(true);
        map.insert(HeaderName::from_static(name), value);
    }
    let request = Request {
        method,
        url: url.to_owned(),
        headers: map,
        body: nanocode_bytes::Bytes::new(),
    };
    let response = nanocode_platform::timeout(CREDENTIAL_TIMEOUT, sender.send(request))
        .await
        .map_err(|_| {
            Error::new(format!(
                "{url} did not answer within {CREDENTIAL_TIMEOUT:?}"
            ))
        })?
        .map_err(|error| Error::new(format!("{url}: {}", error.detail())))?;
    let status = response.status.as_u16();
    let mut stream = response.body;
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(
            &chunk.map_err(|error| Error::new(format!("{url}: {}", error.detail())))?,
        );
    }
    Ok(Reply { status, body })
}

fn text(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

pub fn parse(raw: &[u8], origin: &str) -> Result<Credentials, Error> {
    let value: Value = serde_json::from_slice(raw)
        .map_err(|error| Error::new(format!("{origin} returned invalid JSON: {error}")))?;
    let field = |name: &str| {
        text(&value, name)
            .ok_or_else(|| Error::new(format!("{origin} returned no {name}: {value}")))
    };
    let expires = match text(&value, "Expiration") {
        Some(expiration) => Some(crate::time::parse_rfc3339(&expiration).ok_or_else(|| {
            Error::new(format!(
                "{origin} returned an unreadable Expiration {expiration}"
            ))
        })?),
        None => None,
    };
    Ok(Credentials::new(
        field("AccessKeyId")?,
        field("SecretAccessKey")?,
        text(&value, "SessionToken").or_else(|| text(&value, "Token")),
        expires,
    ))
}

impl Source {
    pub fn origin(&self) -> String {
        match self {
            Self::Static(_) => "AWS_ACCESS_KEY_ID".to_owned(),
            Self::Process {
                profile: Some(profile),
            } => format!("the AWS CLI profile {profile}"),
            Self::Process { profile: None } => "the AWS CLI default profile".to_owned(),
            Self::Container { url, .. } => format!("the container credentials at {url}"),
            Self::Instance { base } => format!("the EC2 instance metadata at {base}"),
        }
    }

    pub async fn fetch(&self, sender: &dyn Sender) -> Result<Credentials, Error> {
        match self {
            Self::Static(credentials) => Ok(credentials.clone()),
            Self::Process { profile } => process(profile.as_deref()).await,
            Self::Container { url, token } => container(sender, url, token.as_ref()).await,
            Self::Instance { base } => instance(sender, base).await,
        }
    }
}

async fn container(
    sender: &dyn Sender,
    url: &str,
    token: Option<&Token>,
) -> Result<Credentials, Error> {
    let token = match token {
        Some(Token::Value(token)) => Some(token.clone()),
        Some(Token::File(path)) => Some(
            String::from_utf8_lossy(&crate::discover::read_file(path.clone()).await.map_err(
                |error| Error::new(format!("cannot read the container token {path}: {error}")),
            )?)
            .trim()
            .to_owned(),
        ),
        None => None,
    };
    let headers = token
        .map(|token| vec![("authorization", token)])
        .unwrap_or_default();
    let reply = call(sender, Method::GET, url, &headers).await?.ok(url)?;
    parse(&reply.body, url)
}

async fn instance(sender: &dyn Sender, base: &str) -> Result<Credentials, Error> {
    let token_url = format!("{base}/latest/api/token");
    let token = call(
        sender,
        Method::PUT,
        &token_url,
        &[(IMDS_TOKEN_TTL, IMDS_TOKEN_TTL_SECONDS.to_owned())],
    )
    .await?
    .ok(&token_url)?
    .text();
    let roles_url = format!("{base}{IMDS_ROLES}");
    let headers = [(IMDS_TOKEN, token)];
    let roles = call(sender, Method::GET, &roles_url, &headers)
        .await?
        .ok(&roles_url)?
        .text();
    let role = roles
        .lines()
        .map(str::trim)
        .find(|role| !role.is_empty())
        .ok_or_else(|| Error::new(format!("{roles_url} names no instance role")))?;
    let url = format!("{roles_url}{role}");
    let reply = call(sender, Method::GET, &url, &headers).await?.ok(&url)?;
    parse(&reply.body, &url)
}

#[cfg(not(target_family = "wasm"))]
async fn process(profile: Option<&str>) -> Result<Credentials, Error> {
    let mut args = vec!["configure", "export-credentials", "--format", "process"];
    if let Some(profile) = profile {
        args.extend(["--profile", profile]);
    }
    let command = format!("aws {}", args.join(" "));
    let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
    let run = nanocode_platform::spawn_blocking(move || {
        std::process::Command::new("aws")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
    });
    let output = nanocode_platform::timeout(CREDENTIAL_TIMEOUT, run)
        .await
        .map_err(|_| Error::new(format!("{command} took over {CREDENTIAL_TIMEOUT:?}")))?
        .map_err(|error| Error::new(format!("cannot run {command}: {error}")))?
        .map_err(|error| Error::new(format!("cannot run {command}: {error}")))?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "{command} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let credentials = parse(&output.stdout, &command)?;
    Ok(match credentials.expires() {
        Some(_) => credentials,
        None => Credentials::new(
            credentials.access_key_id(),
            credentials.secret_access_key(),
            credentials.session_token().map(str::to_owned),
            Some(nanocode_platform::system_time() + UNEXPIRING_LIFETIME),
        ),
    })
}

#[cfg(target_family = "wasm")]
async fn process(_profile: Option<&str>) -> Result<Credentials, Error> {
    Err(Error::new("the AWS CLI is not available in WASM"))
}
