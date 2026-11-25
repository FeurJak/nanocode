use crate::{
    EDGE_ORIGIN, Error, HOST_NAME, ORIGIN, browsers::browser_dirs, chatgpt_installed,
    host::manifest_path,
};
use nanocode_digest::Sha256;
use nanocode_platform::{Committed, Revisioned};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Registration {
    pub root: PathBuf,
    pub original: Option<String>,
    #[serde(default, rename = "auto")]
    pub automatic: bool,
}

#[derive(Serialize, Deserialize)]
struct Released {
    released: Registration,
}

enum Stored {
    Live(Registration),
    Released(Registration),
}

#[derive(Clone, Debug)]
pub struct Upstream {
    pub executable: Option<PathBuf>,
    pub origins: Value,
}

#[derive(Clone, Debug)]
pub struct Launch {
    pub root: PathBuf,
    pub manifest: PathBuf,
    pub registered: SystemTime,
    pub started: Option<(SystemTime, String)>,
}

#[derive(Clone, Debug)]
pub struct Launcher {
    environment: Vec<(String, PathBuf)>,
    program: PathBuf,
    arguments: Vec<String>,
}

impl Launcher {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            environment: Vec::new(),
            program: program.into(),
            arguments: Vec::new(),
        }
    }

    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<PathBuf>) -> Self {
        self.environment.push((name.into(), value.into()));
        self
    }

    #[must_use]
    pub fn arg(mut self, argument: impl Into<String>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    pub fn install(&self, path: &Path) -> Result<(), Error> {
        write_atomic(path, self.script(&[])?.as_bytes(), EXECUTABLE)
    }

    fn script(&self, tail: &[&Path]) -> Result<String, Error> {
        let quote = |text: &str| -> Result<String, Error> {
            if text.chars().any(char::is_control) {
                return Err("host paths must be UTF-8 without control characters".into());
            }
            Ok(format!("'{}'", text.replace('\'', "'\\''")))
        };
        let path = |path: &Path| -> Result<String, Error> {
            quote(
                path.to_str()
                    .ok_or("host paths must be UTF-8 without control characters")?,
            )
        };
        let mut script = String::from("#!/bin/sh\n");
        for (name, value) in &self.environment {
            let mut characters = name.chars();
            if !characters
                .next()
                .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
                || !characters.all(|next| next == '_' || next.is_ascii_alphanumeric())
            {
                return Err(format!("invalid launcher environment variable {name:?}").into());
            }
            script.push_str(&format!("{name}={} ", path(value)?));
        }
        script.push_str(&format!("exec {}", path(&self.program)?));
        for argument in &self.arguments {
            let bare = !argument.is_empty()
                && argument
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c));
            script.push(' ');
            script.push_str(&if bare {
                argument.clone()
            } else {
                quote(argument)?
            });
        }
        for argument in tail {
            script.push(' ');
            script.push_str(&path(argument)?);
        }
        script.push_str(" \"$@\"\n");
        Ok(script)
    }
}

#[derive(Clone, Debug)]
pub struct Bridge {
    base: PathBuf,
    state: PathBuf,
    launcher: Option<Launcher>,
    description: String,
    owner: String,
    namespace: String,
    alternative: Option<String>,
}

const EXECUTABLE: u32 = 0o700;
const PRIVATE: u32 = 0o600;
const RECORD_PREFIX: &str = "chatgpt-";
const RECORD_SUFFIX: &str = ".json";

pub fn host_path(root: &Path) -> PathBuf {
    manifest_path(root, HOST_NAME)
}

fn random_hex() -> Result<String, Error> {
    let mut random = [0u8; 16];
    nanocode_platform::fill_random(&mut random).map_err(|_| "cannot generate random bytes")?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

pub(crate) fn started_path(record: &Path) -> PathBuf {
    record.with_extension("started")
}

pub(crate) fn random_prefix(namespace: &str) -> Result<String, Error> {
    Ok(format!("{namespace}:{}:", random_hex()?))
}

fn ours(path: &Path, launcher: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
        && fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|value| {
                value["name"] == HOST_NAME
                    && value["path"] == launcher.to_string_lossy().as_ref()
                    && value["allowed_origins"].as_array().is_some_and(|origins| {
                        origins
                            .iter()
                            .any(|origin| origin == ORIGIN || origin == EDGE_ORIGIN)
                    })
            })
}

fn temporary(path: &Path) -> Result<PathBuf, Error> {
    let name = path
        .file_name()
        .ok_or("a written path must name a file")?
        .to_string_lossy();
    Ok(path.with_file_name(format!(".{name}.{}.tmp", random_hex()?)))
}

fn written(path: &Path, bytes: &[u8], mode: u32) -> Result<PathBuf, Error> {
    let temporary = temporary(path)?;
    let result = (|| -> Result<(), Error> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(temporary),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), Error> {
    let temporary = written(path, bytes, mode)?;
    let renamed = fs::rename(&temporary, path);
    if renamed.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    Ok(renamed?)
}

fn create_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), Error> {
    let temporary = written(path, bytes, mode)?;
    let linked = fs::hard_link(&temporary, path);
    let _ = fs::remove_file(&temporary);
    Ok(linked?)
}

fn removed(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn official(bytes: &[u8]) -> Result<(PathBuf, Value), Error> {
    let value: Value = serde_json::from_slice(bytes)?;
    let executable = PathBuf::from(
        value["path"]
            .as_str()
            .ok_or("OpenAI host has no executable")?,
    );
    if value["name"] != HOST_NAME
        || value["type"] != "stdio"
        || !value["allowed_origins"].as_array().is_some_and(|origins| {
            origins
                .iter()
                .any(|entry| entry == ORIGIN || entry == EDGE_ORIGIN)
        })
        || !executable.is_absolute()
        || !fs::metadata(&executable)?.is_file()
        || fs::metadata(&executable)?.permissions().mode() & 0o111 == 0
    {
        return Err("OpenAI host is not a usable official-extension stdio host".into());
    }
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(&executable)
            .status()?;
        let details = std::process::Command::new("/usr/bin/codesign")
            .args(["-dv", "--verbose=4"])
            .arg(&executable)
            .output()?;
        if !status.success()
            || !details.status.success()
            || !String::from_utf8_lossy(&details.stderr)
                .lines()
                .any(|line| line == "TeamIdentifier=2DC432GLL2")
            || !String::from_utf8_lossy(&details.stderr)
                .lines()
                .any(|line| line == "Identifier=extension-host")
        {
            return Err("native host is not the OpenAI-signed extension host".into());
        }
    }
    Ok((executable, value["allowed_origins"].clone()))
}

impl Bridge {
    pub fn new(base: impl Into<PathBuf>, state: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            state: state.into(),
            launcher: None,
            description: "nanocode proxy for the original ChatGPT host".into(),
            owner: "nanocode".into(),
            namespace: "nanocode".into(),
            alternative: None,
        }
    }

    #[must_use]
    pub fn launcher(mut self, launcher: Launcher) -> Self {
        self.launcher = Some(launcher);
        self
    }

    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    #[must_use]
    pub fn owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = owner.into();
        self
    }

    #[must_use]
    pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = namespace.into();
        self
    }

    #[must_use]
    pub fn alternative(mut self, alternative: impl Into<String>) -> Self {
        self.alternative = Some(alternative.into());
        self
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn state(&self) -> &Path {
        &self.state
    }

    pub fn manifest_description(&self) -> &str {
        &self.description
    }

    pub(crate) fn owner_name(&self) -> &str {
        &self.owner
    }

    pub(crate) fn namespace_name(&self) -> &str {
        &self.namespace
    }

    pub(crate) fn alternative_text(&self) -> Option<&str> {
        self.alternative.as_deref()
    }

    pub fn browsers(&self) -> Vec<(&'static str, PathBuf)> {
        browser_dirs(&self.base)
    }

    pub fn browser_root(&self, browser: &str) -> Result<PathBuf, Error> {
        let root = if Path::new(browser).is_absolute() {
            PathBuf::from(browser)
        } else {
            self.browsers()
                .into_iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(browser))
                .map(|(_, path)| path)
                .ok_or_else(|| format!("unknown browser {browser:?}; use a browser name or an absolute Chromium data directory"))?
        };
        fs::canonicalize(&root).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => format!(
                "{browser} has no data directory at {}: it is not installed for this user, or it has not been opened yet",
                root.display()
            )
            .into(),
            _ => format!("{browser}'s data directory {}: {error}", root.display()).into(),
        })
    }

    fn paths(&self, root: &Path) -> Result<(PathBuf, PathBuf), Error> {
        let bytes = root.to_str().ok_or("browser path must be UTF-8")?;
        let key = Sha256::digest(bytes.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok((
            self.state.join(format!("chatgpt-{key}.json")),
            self.state.join(format!("chatgpt-{key}.sh")),
        ))
    }

    pub fn record_path(&self, root: &Path) -> Result<PathBuf, Error> {
        Ok(self.paths(root)?.0)
    }

    pub fn launcher_path(&self, root: &Path) -> Result<PathBuf, Error> {
        Ok(self.paths(root)?.1)
    }

    pub fn registration(&self, root: &Path) -> Result<Registration, Error> {
        self.read_record(&self.paths(root)?.0)
    }

    pub fn save_registration(&self, registration: &Registration) -> Result<(), Error> {
        let path = self.paths(&registration.root)?.0;
        let bytes = serde_json::to_vec(registration)?;
        loop {
            let (revision, _) = self.record_at(&path)?;
            if commit_record(&path, revision, &bytes)? {
                return Ok(());
            }
        }
    }

    pub fn release(&self, root: &Path) -> Result<bool, Error> {
        self.uninstall_one(&self.paths(root)?.0)
    }

    pub fn write_private(&self, path: &Path, bytes: &[u8]) -> Result<(), Error> {
        write_atomic(path, bytes, PRIVATE)
    }

    fn records(&self) -> Result<Vec<PathBuf>, Error> {
        let mut result = BTreeSet::new();
        for entry in fs::read_dir(&self.state)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            let record = name
                .rsplit_once('.')
                .filter(|(_, revision)| {
                    !revision.is_empty() && revision.bytes().all(|byte| byte.is_ascii_digit())
                })
                .map_or(&*name, |(record, _)| record);
            if record.starts_with(RECORD_PREFIX) && record.ends_with(RECORD_SUFFIX) {
                result.insert(self.state.join(record));
            }
        }
        Ok(result.into_iter().collect())
    }

    fn stored_at(&self, path: &Path) -> Result<(u64, Option<Stored>), Error> {
        let revision = Revisioned::new(path).read()?;
        let stored = match revision.bytes {
            None => None,
            Some(bytes) => Some(match serde_json::from_slice::<Released>(&bytes) {
                Ok(Released { released }) => Stored::Released(self.checked(path, released)?),
                Err(_) => Stored::Live(self.checked(path, serde_json::from_slice(&bytes)?)?),
            }),
        };
        Ok((revision.number, stored))
    }

    fn record_at(&self, path: &Path) -> Result<(u64, Option<Registration>), Error> {
        let (revision, stored) = self.stored_at(path)?;
        let live = match stored {
            Some(Stored::Live(registration)) => Some(registration),
            Some(Stored::Released(_)) | None => None,
        };
        Ok((revision, live))
    }

    fn checked(&self, path: &Path, registration: Registration) -> Result<Registration, Error> {
        if !registration.root.is_absolute()
            || fs::canonicalize(&registration.root)? != registration.root
            || self.paths(&registration.root)?.0 != path
        {
            return Err("ChatGPT registration does not match its browser directory".into());
        }
        Ok(registration)
    }

    fn read_record(&self, path: &Path) -> Result<Registration, Error> {
        self.record_at(path)?
            .1
            .ok_or_else(|| format!("no ChatGPT registration at {}", path.display()).into())
    }

    pub(crate) fn owns(&self, manifest: &Path, launcher: &Path) -> bool {
        ours(manifest, launcher) && self.proxy_manifest(manifest)
    }

    fn proxy_manifest(&self, path: &Path) -> bool {
        fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|value| value["description"] == self.description.as_str())
    }

    fn write_launcher(&self, launcher: &Path, manifest: &Path, root: &Path) -> Result<(), Error> {
        let script = self
            .launcher
            .as_ref()
            .ok_or("no launcher is configured for the ChatGPT host")?
            .script(&[manifest, root])?;
        if fs::symlink_metadata(launcher)
            .is_ok_and(|meta| meta.file_type().is_file() && meta.permissions().mode() & 0o111 != 0)
            && fs::read(launcher).is_ok_and(|bytes| bytes == script.as_bytes())
        {
            return Ok(());
        }
        write_atomic(launcher, script.as_bytes(), EXECUTABLE)
    }

    fn write_manifest(
        &self,
        path: &Path,
        launcher: &Path,
        origins: Value,
        replace: bool,
    ) -> Result<(), Error> {
        let manifest = json!({
            "name": HOST_NAME,
            "description": self.description,
            "path": launcher,
            "type": "stdio",
            "allowed_origins": origins,
        });
        let mut bytes = serde_json::to_string_pretty(&manifest)?.into_bytes();
        bytes.push(b'\n');
        if replace {
            write_atomic(path, &bytes, PRIVATE)
        } else {
            create_atomic(path, &bytes, PRIVATE)
        }
    }

    pub fn upstream(&self, root: &Path) -> Result<Upstream, Error> {
        self.upstream_of(&self.registration(root)?)
    }

    pub(crate) fn upstream_of(&self, record: &Registration) -> Result<Upstream, Error> {
        let original = match record.original {
            Some(ref original) => Some(original.clone()),
            None => self.chrome_original()?,
        };
        match original {
            Some(bytes) => {
                let (executable, origins) = official(bytes.as_bytes())?;
                Ok(Upstream {
                    executable: Some(executable),
                    origins,
                })
            }
            None => Ok(Upstream {
                executable: None,
                origins: json!([ORIGIN, EDGE_ORIGIN]),
            }),
        }
    }

    fn chrome_original(&self) -> Result<Option<String>, Error> {
        let chrome = self
            .browsers()
            .into_iter()
            .find(|(name, _)| *name == "Chrome")
            .ok_or("Chrome's data directory is unknown")?
            .1;
        let chrome = match fs::canonicalize(chrome) {
            Ok(chrome) => chrome,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let (backup, launcher) = self.paths(&chrome)?;
        let bytes = if backup.exists() {
            match self.read_record(&backup)?.original {
                Some(bytes) => bytes,
                None => return Ok(None),
            }
        } else {
            let path = host_path(&chrome);
            if ours(&path, &launcher) {
                return Err("Chrome's original host registration was not saved".into());
            }
            match fs::read_to_string(path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        };
        official(bytes.as_bytes())?;
        Ok(Some(bytes))
    }

    fn reconcile_one(&self, record_path: &Path) -> Result<bool, Error> {
        let record = loop {
            let (revision, stored) = self.stored_at(record_path)?;
            let mut record = match stored {
                None => return Ok(false),
                Some(Stored::Released(record)) => {
                    self.withdraw(&record)?;
                    return Ok(false);
                }
                Some(Stored::Live(record)) => record,
            };
            let (path, launcher) = (host_path(&record.root), self.paths(&record.root)?.1);
            let current = match fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_file() => Some(fs::read_to_string(&path)?),
                Ok(_) => return Err("ChatGPT native-host manifest is not a regular file".into()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if current.is_some() && self.owns(&path, &launcher) {
                self.write_launcher(&launcher, &path, &record.root)?;
                return Ok(false);
            }
            let replaced = current.is_some();
            if let Some(ref bytes) = current {
                official(bytes.as_bytes())?;
                if record.original.as_deref() != Some(bytes) {
                    record.original = Some(bytes.clone());
                    if !commit_record(record_path, revision, &serde_json::to_vec(&record)?)? {
                        continue;
                    }
                }
            }
            let upstream = self.upstream_of(&record)?;
            self.write_launcher(&launcher, &path, &record.root)?;
            self.write_manifest(&path, &launcher, upstream.origins, replaced)?;
            break record;
        };
        if self.record_at(record_path)?.1.is_none() {
            self.withdraw(&record)?;
            return Ok(false);
        }
        Ok(true)
    }

    fn withdraw(&self, record: &Registration) -> Result<(), Error> {
        let (path, launcher) = (host_path(&record.root), self.paths(&record.root)?.1);
        if ours(&path, &launcher) && self.proxy_manifest(&path) {
            let original = record
                .original
                .clone()
                .or_else(|| self.chrome_original().ok().flatten());
            match original {
                Some(original) => write_atomic(&path, original.as_bytes(), PRIVATE)?,
                None => removed(&path)?,
            }
        }
        removed(&launcher)
    }

    pub fn install(&self, root: &Path, browser: &str, automatic: bool) -> Result<PathBuf, Error> {
        if !cfg!(target_os = "macos") {
            return Err("experimental ChatGPT proxying requires macOS".into());
        }
        let root = root.to_path_buf();
        if !chatgpt_installed(&root)? {
            return Err(format!(
                "the official ChatGPT extension ({} or {}) is not installed in {browser}",
                crate::EXTENSION_ID,
                crate::EDGE_EXTENSION_ID
            )
            .into());
        }
        let path = host_path(&root);
        let hosts = path.parent().ok_or("invalid native-host path")?;
        fs::create_dir_all(hosts)?;
        if fs::canonicalize(hosts)?.parent() != Some(fs::canonicalize(&root)?.as_path()) {
            return Err("browser native-host directory must not redirect elsewhere".into());
        }
        let (record_path, launcher) = self.paths(&root)?;
        loop {
            let (revision, record) = self.record_at(&record_path)?;
            let record = match record {
                None => self.registered(&root, &path, automatic)?,
                Some(mut record) => {
                    if !ours(&path, &launcher) && path.exists() {
                        official(&fs::read(&path)?)?;
                    }
                    if automatic || !record.automatic {
                        break;
                    }
                    record.automatic = false;
                    record
                }
            };
            if commit_record(&record_path, revision, &serde_json::to_vec(&record)?)? {
                break;
            }
        }
        self.reconcile_one(&record_path)?;
        Ok(path)
    }

    fn registered(&self, root: &Path, path: &Path, automatic: bool) -> Result<Registration, Error> {
        let original = match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_file() => {
                let bytes = fs::read_to_string(path)?;
                official(bytes.as_bytes())?;
                Some(bytes)
            }
            Ok(_) => return Err("native-host manifest is not a regular file".into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let registration = Registration {
            root: root.to_path_buf(),
            original,
            automatic,
        };
        if registration.original.is_none() {
            self.upstream_of(&registration)?;
        }
        Ok(registration)
    }

    fn uninstall_one(&self, record_path: &Path) -> Result<bool, Error> {
        let record = loop {
            let (revision, stored) = self.stored_at(record_path)?;
            let record = match stored {
                None => return Ok(false),
                Some(Stored::Released(record)) => {
                    self.withdraw(&record)?;
                    return Ok(false);
                }
                Some(Stored::Live(record)) => record,
            };
            let (path, launcher) = (host_path(&record.root), self.paths(&record.root)?.1);
            if ours(&path, &launcher) && !self.proxy_manifest(&path) {
                return Err(format!(
                    "{}'s registration changed; refusing to restore over it",
                    self.owner
                )
                .into());
            }
            let released = Released { released: record };
            if commit_record(record_path, revision, &serde_json::to_vec(&released)?)? {
                break released.released;
            }
        };
        self.withdraw(&record)?;
        removed(&started_path(record_path))?;
        Ok(true)
    }

    pub fn uninstall_all(&self) -> Result<(), Error> {
        if !self.state.is_dir() {
            return Ok(());
        }
        for record in self.records()? {
            self.uninstall_one(&record)?;
        }
        Ok(())
    }

    pub fn launches(&self) -> Vec<Launch> {
        let mut result = Vec::new();
        for record_path in self.records().unwrap_or_default() {
            let Ok(record) = self.read_record(&record_path) else {
                continue;
            };
            let manifest = host_path(&record.root);
            let Ok(registered) = fs::metadata(&manifest).and_then(|meta| meta.modified()) else {
                continue;
            };
            let marker = started_path(&record_path);
            let started = fs::metadata(&marker)
                .and_then(|meta| meta.modified())
                .ok()
                .filter(|started| *started >= registered)
                .map(|started| (started, fs::read_to_string(&marker).unwrap_or_default()));
            result.push(Launch {
                root: record.root,
                manifest,
                registered,
                started,
            });
        }
        result
    }

    pub fn reconcile_all(&self) -> Result<usize, Error> {
        if !self.state.is_dir() {
            return Ok(0);
        }
        let mut repaired = 0;
        let mut failure = None;
        for record in self.records()? {
            match self.reconcile_one(&record) {
                Ok(true) => repaired += 1,
                Ok(false) => {}
                Err(error) if failure.is_none() => failure = Some(error),
                Err(_) => {}
            }
        }
        if let Some(error) = failure {
            Err(error)
        } else {
            Ok(repaired)
        }
    }
}

pub(crate) fn live_record(path: &Path) -> bool {
    Revisioned::new(path).read().is_ok_and(|revision| {
        revision
            .bytes
            .is_some_and(|bytes| serde_json::from_slice::<Registration>(&bytes).is_ok())
    })
}

fn commit_record(path: &Path, expected: u64, bytes: &[u8]) -> Result<bool, Error> {
    Ok(matches!(
        Revisioned::new(path).commit(expected, bytes)?,
        Committed::Landed(_)
    ))
}
