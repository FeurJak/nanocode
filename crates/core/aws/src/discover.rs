use crate::Error;
use crate::source::{Source, Token};

pub const BEARER_TOKEN: &str = "AWS_BEARER_TOKEN_BEDROCK";
const INSTANCE_METADATA: &str = "http://169.254.169.254";
const CONTAINER_METADATA: &str = "http://169.254.170.2";
const DEFAULT_PROFILE: &str = "default";

type Lookup<'a> = &'a (dyn Fn(&str) -> Option<String> + Sync);

#[derive(Clone, Debug)]
pub enum Found {
    Bearer(String),
    Signed(Source),
}

fn present(lookup: Lookup<'_>, name: &str) -> Option<String> {
    lookup(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn flag(lookup: Lookup<'_>, name: &str) -> bool {
    present(lookup, name)
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

#[cfg(not(target_family = "wasm"))]
pub async fn read_file(path: String) -> Result<Vec<u8>, String> {
    nanocode_platform::spawn_blocking(move || std::fs::read(path))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

#[cfg(target_family = "wasm")]
pub async fn read_file(_path: String) -> Result<Vec<u8>, String> {
    Err("AWS configuration files are not readable in WASM".to_owned())
}

fn ini_section<'a>(text: &'a str, section: &str) -> Option<Vec<(&'a str, &'a str)>> {
    let mut found = None;
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') || line.starts_with(';') || line.is_empty() {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            inside = name.split_whitespace().collect::<Vec<_>>().join(" ") == section;
            if inside {
                found.get_or_insert_with(Vec::new);
            }
            continue;
        }
        if inside
            && let Some((key, value)) = line.split_once('=')
            && let Some(pairs) = found.as_mut()
        {
            pairs.push((key.trim(), value.trim()));
        }
    }
    found
}

fn home_file(lookup: Lookup<'_>, override_name: &str, file: &str) -> Option<String> {
    present(lookup, override_name).or_else(|| {
        let home = present(lookup, "HOME").or_else(|| present(lookup, "USERPROFILE"))?;
        Some(format!("{home}/.aws/{file}"))
    })
}

struct Files {
    config: String,
    credentials: String,
}

async fn files(lookup: Lookup<'_>) -> Files {
    let read = |path: Option<String>| async move {
        match path {
            Some(path) => read_file(path)
                .await
                .map(|raw| String::from_utf8_lossy(&raw).into_owned())
                .unwrap_or_default(),
            None => String::new(),
        }
    };
    Files {
        config: read(home_file(lookup, "AWS_CONFIG_FILE", "config")).await,
        credentials: read(home_file(
            lookup,
            "AWS_SHARED_CREDENTIALS_FILE",
            "credentials",
        ))
        .await,
    }
}

fn config_section(profile: &str) -> String {
    match profile {
        DEFAULT_PROFILE => DEFAULT_PROFILE.to_owned(),
        profile => format!("profile {profile}"),
    }
}

fn is_region(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub async fn region(lookup: Lookup<'_>) -> Option<String> {
    let configured = ["AWS_REGION", "AWS_DEFAULT_REGION"]
        .iter()
        .filter_map(|name| present(lookup, name))
        .find(|value| is_region(value));
    if configured.is_some() {
        return configured;
    }
    let profile = present(lookup, "AWS_PROFILE").unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    let files = files(lookup).await;
    [
        ini_section(&files.credentials, &profile),
        ini_section(&files.config, &config_section(&profile)),
    ]
    .into_iter()
    .flatten()
    .flatten()
    .find(|(key, value)| *key == "region" && is_region(value))
    .map(|(_, value)| value.to_owned())
}

fn container(lookup: Lookup<'_>) -> Option<Source> {
    let url = present(lookup, "AWS_CONTAINER_CREDENTIALS_FULL_URI").or_else(|| {
        present(lookup, "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
            .map(|path| format!("{CONTAINER_METADATA}{path}"))
    })?;
    let token = present(lookup, "AWS_CONTAINER_AUTHORIZATION_TOKEN")
        .map(Token::Value)
        .or_else(|| present(lookup, "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE").map(Token::File));
    Some(Source::Container { url, token })
}

pub async fn find(lookup: Lookup<'_>) -> Result<Found, Error> {
    if let Some(token) = present(lookup, BEARER_TOKEN) {
        return Ok(Found::Bearer(token));
    }
    if let (Some(id), Some(secret)) = (
        present(lookup, "AWS_ACCESS_KEY_ID"),
        present(lookup, "AWS_SECRET_ACCESS_KEY"),
    ) {
        let token = present(lookup, "AWS_SESSION_TOKEN");
        return Ok(Found::Signed(Source::Static(crate::Credentials::new(
            id, secret, token, None,
        ))));
    }
    if let Some(profile) = present(lookup, "AWS_PROFILE") {
        return Ok(Found::Signed(Source::Process {
            profile: Some(profile),
        }));
    }
    if let Some(source) = container(lookup) {
        return Ok(Found::Signed(source));
    }
    let files = files(lookup).await;
    if ini_section(&files.credentials, DEFAULT_PROFILE).is_some()
        || ini_section(&files.config, DEFAULT_PROFILE).is_some()
    {
        return Ok(Found::Signed(Source::Process { profile: None }));
    }
    if flag(lookup, "AWS_EC2_METADATA_DISABLED") {
        return Err(Error::new(format!(
            "no AWS credentials found; tried {BEARER_TOKEN}, AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, AWS_PROFILE, the ECS container credentials, a default profile in the AWS config files, and the EC2 instance metadata (AWS_EC2_METADATA_DISABLED is set)"
        )));
    }
    let base = present(lookup, "AWS_EC2_METADATA_SERVICE_ENDPOINT")
        .unwrap_or_else(|| INSTANCE_METADATA.to_owned())
        .trim_end_matches('/')
        .to_owned();
    Ok(Found::Signed(Source::Instance { base }))
}
