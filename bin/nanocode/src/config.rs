use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use nanocode::tools::mcp::{McpServer, McpToolExposure};
use serde::Deserialize;

use crate::Result;

const FILE: &str = "config.json";
const PROJECT_DIRECTORY: &str = ".nanocode";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(rename = "$schema")]
    _schema: Option<String>,
    #[serde(default)]
    mcp: Mcp,
    herdr: Option<bool>,
    forge_usage: Option<bool>,
    #[serde(default)]
    hooks: Hooks,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Hooks {
    pre_tool: Option<Vec<crate::hook::Entry>>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mcp {
    #[serde(default)]
    servers: BTreeMap<String, Entry>,
    #[serde(default)]
    timeout: Timeout,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Timeout {
    startup: Option<u64>,
    catalog: Option<u64>,
    execution: Option<u64>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Entry {
    Local {
        command: Vec<String>,
        cwd: Option<PathBuf>,
        #[serde(default)]
        environment: BTreeMap<String, String>,
        #[serde(default)]
        disabled: bool,
        codemode: Option<bool>,
        #[serde(default)]
        timeout: Timeout,
        protocol: Option<String>,
        pool: Option<bool>,
    },
    Remote {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        oauth: Option<serde_json::Value>,
        #[serde(default)]
        disabled: bool,
        codemode: Option<bool>,
        #[serde(default)]
        timeout: Timeout,
        protocol: Option<String>,
        pool: Option<bool>,
    },
}

struct Common<'a> {
    disabled: bool,
    codemode: Option<bool>,
    timeout: Timeout,
    protocol: Option<&'a str>,
    pool: Option<bool>,
}

impl Entry {
    fn common(&self) -> Common<'_> {
        match self {
            Self::Local {
                disabled,
                codemode,
                timeout,
                protocol,
                pool,
                ..
            }
            | Self::Remote {
                disabled,
                codemode,
                timeout,
                protocol,
                pool,
                ..
            } => Common {
                disabled: *disabled,
                codemode: *codemode,
                timeout: *timeout,
                protocol: protocol.as_deref(),
                pool: *pool,
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct Configured {
    pub name: String,
    pub source: PathBuf,
    pub transport: String,
    pub enabled: bool,
}

pub struct Config {
    pub servers: Vec<(String, McpServer)>,
    pub configured: Vec<Configured>,
    pub herdr: Option<bool>,
    pub forge_usage: Option<bool>,
    pub pre_tool: Vec<crate::hook::Entry>,
}

pub fn paths(home: &Path, workspace: &Path) -> [PathBuf; 2] {
    [
        home.join(FILE),
        workspace.join(PROJECT_DIRECTORY).join(FILE),
    ]
}

pub async fn load(home: &Path, workspace: &Path) -> Result<Config> {
    let paths = paths(home, workspace);
    let workspace = workspace.to_path_buf();
    nanocode_platform::spawn_blocking(move || read(&paths, &workspace)).await?
}

fn read(paths: &[PathBuf], workspace: &Path) -> Result<Config> {
    let mut entries = BTreeMap::new();
    let mut defaults = Timeout::default();
    let mut herdr = None;
    let mut forge_usage = None;
    let mut pre_tool = Vec::new();
    for path in paths {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{}: {error}", path.display()).into()),
        };
        let file: File = serde_json::from_slice(&bytes)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        for (name, entry) in file.mcp.servers {
            entries.insert(name, (path.clone(), entry));
        }
        defaults = Timeout {
            startup: file.mcp.timeout.startup.or(defaults.startup),
            catalog: file.mcp.timeout.catalog.or(defaults.catalog),
            execution: file.mcp.timeout.execution.or(defaults.execution),
        };
        herdr = file.herdr.or(herdr);
        forge_usage = file.forge_usage.or(forge_usage);
        if let Some(entries) = file.hooks.pre_tool {
            pre_tool = entries;
        }
    }
    let mut loaded = Config {
        servers: Vec::new(),
        configured: Vec::new(),
        herdr,
        forge_usage,
        pre_tool,
    };
    for (name, (source, entry)) in entries {
        let place = |error: String| format!("{} mcp.servers.{name}: {error}", source.display());
        let enabled = !entry.common().disabled;
        let (server, transport) = server(entry, defaults, workspace).map_err(place)?;
        loaded.configured.push(Configured {
            name: name.clone(),
            source,
            transport,
            enabled,
        });
        if enabled {
            loaded.servers.push((name, server));
        }
    }
    Ok(loaded)
}

fn server(
    entry: Entry,
    defaults: Timeout,
    workspace: &Path,
) -> std::result::Result<(McpServer, String), String> {
    let Common {
        codemode,
        timeout,
        protocol,
        pool,
        ..
    } = entry.common();
    if protocol.is_some_and(|protocol| protocol != "legacy") {
        return Err("nanocode speaks the classic initialize handshake only; set protocol to \"legacy\" or leave it out".to_owned());
    }
    if pool == Some(true) {
        return Err("nanocode does not share MCP connections across directories; set pool to false or leave it out".to_owned());
    }
    let (server, transport) = match entry {
        Entry::Local {
            command,
            cwd,
            environment,
            ..
        } => {
            let mut words = command
                .iter()
                .map(|word| substitute(word))
                .collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter();
            let program = words
                .next()
                .filter(|program| !program.trim().is_empty())
                .ok_or("command is empty: give the executable and its arguments")?;
            let transport = format!("stdio: {program}");
            let mut server = McpServer::stdio(program)
                .args(words)
                .cwd(workspace.join(cwd.unwrap_or_default()));
            for (name, value) in environment {
                server = server.env(name, substitute(&value)?);
            }
            (server, transport)
        }
        Entry::Remote {
            url,
            headers,
            oauth,
            ..
        } => {
            if oauth.is_some_and(|oauth| oauth != serde_json::Value::Bool(false)) {
                return Err("nanocode does not run an MCP OAuth sign-in from config yet; set oauth to false and pass the credential in headers".to_owned());
            }
            let url = substitute(&url)?;
            let transport = format!("http: {url}");
            let mut server = McpServer::http(url);
            for (name, value) in headers {
                server = server.header(name, substitute(&value)?);
            }
            (server, transport)
        }
    };
    let server = match codemode {
        Some(false) => server.tool_exposure(McpToolExposure::DeferredOnly),
        Some(true) | None => server,
    };
    Ok((timed(server, timeout, defaults)?, transport))
}

fn timed(
    server: McpServer,
    timeout: Timeout,
    defaults: Timeout,
) -> std::result::Result<McpServer, String> {
    let millis = |value: Option<u64>, name: &str| match value {
        Some(0) => Err(format!("timeout.{name} must be at least 1 ms")),
        value => Ok(value.map(Duration::from_millis)),
    };
    if millis(timeout.catalog.or(defaults.catalog), "catalog")?.is_some() {
        return Err("nanocode lists a server's tools within its startup timeout; set timeout.startup instead of timeout.catalog".to_owned());
    }
    let server = match millis(timeout.startup.or(defaults.startup), "startup")? {
        Some(startup) => server.startup_timeout(startup),
        None => server,
    };
    Ok(
        match millis(timeout.execution.or(defaults.execution), "execution")? {
            Some(execution) => server.tool_timeout(execution),
            None => server,
        },
    )
}

fn substitute(text: &str) -> std::result::Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{env:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "{env:".len()..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("{text:?} opens {{env: without a closing }}"))?;
        let variable = &after[..end];
        let value = std::env::var(variable).map_err(|_| {
            format!("{text:?} names the environment variable {variable}, which is not set")
        })?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

impl crate::app::App {
    pub(crate) fn mcp(&mut self) {
        let handle = self.host.mcp.clone();
        let configured = self.host.config.mcp_configured.clone();
        let paths = paths(&self.home, &self.host.config.workspace);
        let tx = self.tx.clone();
        nanocode_platform::spawn_task(async move {
            let tools = match &handle {
                Some(handle) => handle.tools().await,
                None => BTreeMap::new(),
            };
            let text = describe(&configured, &tools, &paths);
            drop(tx.send(crate::app::AppEvent::Notice(
                nanocode_tui::transcript::NoticeLevel::Info,
                text,
            )));
        });
    }
}

fn describe(
    configured: &[Configured],
    tools: &BTreeMap<
        String,
        std::result::Result<Vec<String>, nanocode::tools::mcp::McpServerError>,
    >,
    paths: &[PathBuf],
) -> String {
    let files = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(" and ");
    if configured.is_empty() && tools.is_empty() {
        return format!("no MCP servers; list them under \"mcp\": {{\"servers\": ...}} in {files}");
    }
    let mut lines = vec![format!("MCP servers (configured in {files}):")];
    for server in configured {
        let state = match (server.enabled, tools.get(&server.name)) {
            (false, _) => "disabled".to_owned(),
            (true, Some(Ok(names))) => ready(names),
            (true, Some(Err(error))) => format!("failed: {error}"),
            (true, None) => "not started".to_owned(),
        };
        lines.push(format!(
            "  {} · {} · {} · {state}",
            server.name,
            server.transport,
            server.source.display()
        ));
    }
    for (name, listed) in tools {
        if configured.iter().any(|server| &server.name == name) {
            continue;
        }
        let state = match listed {
            Ok(names) => ready(names),
            Err(error) => format!("failed: {error}"),
        };
        lines.push(format!("  {name} · built in · {state}"));
    }
    lines.join("\n")
}

fn ready(names: &[String]) -> String {
    match names.len() {
        0 => "ready, no tools".to_owned(),
        1 => format!("ready, 1 tool: {}", names[0]),
        count => format!("ready, {count} tools: {}", names.join(", ")),
    }
}
