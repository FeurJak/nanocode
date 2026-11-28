use std::{
    collections::HashMap,
    io::Write as _,
    path::{Path, PathBuf},
    sync::mpsc,
};

use nanocode::{
    agent::events::{AgentEvent, AgentEventKind},
    subagents::{AgentId, AgentUpdate, ScopedAgentUpdate},
};
use nanocode_sync::Atom;
use serde_json::{Value, json};

const DIRECTORY_ENV: &str = "FORGE_USAGE_DIR";
const SCHEMA: u64 = 1;
const NANO_USD: f64 = 1e9;
const NANOS_PER_MILLI: u64 = 1_000_000;

#[derive(Clone)]
struct Child {
    session: String,
    role: String,
    parent: Option<AgentId>,
}

enum Line {
    Record(u64, String),
    Flush(nanocode_sync::oneshot::Sender<()>),
}

pub struct Ledger {
    lines: mpsc::Sender<Line>,
    cwd: PathBuf,
    children: Atom<HashMap<(String, AgentId), Child>>,
    commands: Atom<HashMap<String, (String, String)>>,
    hour: Atom<HashMap<String, bool>>,
    calls: std::sync::atomic::AtomicU64,
}

pub fn forge_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".forge"))
}

impl Ledger {
    pub fn open(setting: Option<bool>, workspace: &Path) -> crate::Result<Option<Self>> {
        let forge = forge_home();
        let enabled = setting.unwrap_or_else(|| forge.as_deref().is_some_and(Path::is_dir));
        if !enabled {
            return Ok(None);
        }
        let directory = match std::env::var_os(DIRECTORY_ENV) {
            Some(directory) => PathBuf::from(directory),
            None => forge
                .ok_or("forge_usage is on, but HOME is unset, so ~/.forge/telemetry has no place")?
                .join("telemetry"),
        };
        let (lines, received) = mpsc::channel::<Line>();
        std::thread::Builder::new()
            .name("forge-usage".to_owned())
            .spawn(move || write(&directory, &received))?;
        Ok(Some(Self {
            lines,
            cwd: workspace.to_path_buf(),
            children: Atom::new(HashMap::new()),
            commands: Atom::new(HashMap::new()),
            hour: Atom::new(HashMap::new()),
            calls: std::sync::atomic::AtomicU64::new(0),
        }))
    }

    pub fn command(&self, root: &str, command: Option<(String, String)>) {
        self.commands.update(|commands| match &command {
            Some(command) => {
                commands.insert(root.to_owned(), command.clone());
            }
            None => {
                commands.remove(root);
            }
        });
    }

    pub fn root_event(&self, session: &str, event: &AgentEvent) {
        self.event(session, session, None, None, event);
    }

    pub fn update(&self, update: &ScopedAgentUpdate) {
        let root = update.root_session_id.as_str();
        match &update.update {
            AgentUpdate::Added(descriptor) => {
                let child = Child {
                    session: descriptor.session_id.clone(),
                    role: descriptor.role.clone(),
                    parent: descriptor.parent,
                };
                let key = (root.to_owned(), descriptor.id);
                self.children.update(|children| {
                    children.insert(key.clone(), child.clone());
                });
            }
            AgentUpdate::Event { id, parent, event } => {
                let lookup = |id: AgentId| {
                    self.children
                        .read(|children| children.get(&(root.to_owned(), id)).cloned())
                };
                let child = lookup(*id);
                let session = child
                    .as_ref()
                    .map_or_else(|| format!("{root}/{id}"), |child| child.session.clone());
                let parent = parent
                    .or(child.as_ref().and_then(|child| child.parent))
                    .map_or_else(
                        || root.to_owned(),
                        |parent| {
                            lookup(parent)
                                .map_or_else(|| format!("{root}/{parent}"), |parent| parent.session)
                        },
                    );
                let role = child.map(|child| child.role);
                self.event(&session, root, Some(&parent), role.as_deref(), event);
            }
            AgentUpdate::Status { .. }
            | AgentUpdate::Completion { .. }
            | AgentUpdate::Message(_) => {}
        }
    }

    fn event(
        &self,
        session: &str,
        root: &str,
        parent: Option<&str>,
        agent: Option<&str>,
        event: &AgentEvent,
    ) {
        if !matches!(
            event.kind,
            AgentEventKind::ModelCallStarted | AgentEventKind::ModelCallCompleted
        ) {
            return;
        }
        let Ok(payload) = serde_json::from_str::<Value>(event.payload.get()) else {
            return;
        };
        if event.kind == AgentEventKind::ModelCallStarted {
            let hour = payload["cache_ttl"].as_str() == Some("1h");
            self.hour.update(|sessions| {
                sessions.insert(session.to_owned(), hour);
            });
            return;
        }
        let at = nanocode_platform::unix_millis();
        let call = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let hour = self
            .hour
            .read(|sessions| sessions.get(session).copied().unwrap_or(false));
        let command = self.commands.read(|commands| commands.get(root).cloned());
        let record = record(
            &payload,
            &Context {
                at,
                session,
                root,
                parent,
                agent,
                cwd: &self.cwd,
                call,
                hour,
                command: command.as_ref(),
            },
        );
        drop(self.lines.send(Line::Record(at, record.to_string())));
    }

    pub async fn flush(&self) {
        let (done, flushed) = nanocode_sync::oneshot::channel();
        if self.lines.send(Line::Flush(done)).is_ok() {
            drop(flushed.await);
        }
    }
}

struct Context<'a> {
    at: u64,
    session: &'a str,
    root: &'a str,
    parent: Option<&'a str>,
    agent: Option<&'a str>,
    cwd: &'a Path,
    call: u64,
    hour: bool,
    command: Option<&'a (String, String)>,
}

fn record(payload: &Value, context: &Context<'_>) -> Value {
    let usage = &payload["usage"];
    let count = |value: &Value| value.as_u64().unwrap_or_default();
    let input = count(&usage["input_tokens"]);
    let cache_read = count(&usage["input_tokens_details"]["cached_tokens"]);
    let cache_write = count(&usage["input_tokens_details"]["cache_write_tokens"]);
    let output = count(&usage["output_tokens"]);
    let reasoning = count(&usage["output_tokens_details"]["reasoning_tokens"]);
    let model = payload["model"].as_str().unwrap_or_default();
    let message = payload["response_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map_or_else(
            || format!("{}#{}", context.session, context.call),
            str::to_owned,
        );
    let mut record = json!({
        "v": SCHEMA,
        "ts": timestamp(context.at),
        "harness": "nanocode",
        "command": context.command.map(|(name, _)| format!("/{name}")),
        "command_source": context.command.map(|_| "hook"),
        "args": context.command.map(|(_, arguments)| arguments).filter(|arguments| !arguments.is_empty()),
        "artifact": null,
        "domain": null,
        "session": context.session,
        "command_session": context.root,
        "parent": context.parent,
        "agent": context.agent,
        "provider": provider(model),
        "model": model,
        "repo": null,
        "branch": null,
        "cwd": context.cwd.display().to_string(),
        "message": message,
        "tokens": {
            "in": input.saturating_sub(cache_read).saturating_sub(cache_write),
            "out": output.saturating_sub(reasoning),
            "reasoning": reasoning,
            "cache_read": cache_read,
            "cache_write": cache_write,
        },
        "cost": usage["estimated_cost_nano_usd"].as_u64().map(|nano| nano as f64 / NANO_USD),
        "ms": payload["duration_ns"].as_u64().map(|nanos| nanos / NANOS_PER_MILLI),
        "finish": payload["status"],
    });
    if context.hour && cache_write > 0 {
        record["cache_write_1h"] = json!(cache_write);
    }
    record
}

fn provider(model: &str) -> Option<&'static str> {
    let set = |name: &str| {
        std::env::var(name).is_ok_and(|value| matches!(value.trim(), "1" | "true" | "yes"))
    };
    if model.starts_with("claude-") {
        return Some(match set("CLAUDE_CODE_USE_BEDROCK") {
            true => "amazon-bedrock",
            false => "anthropic",
        });
    }
    if model.starts_with("gemini-") {
        return Some("google");
    }
    if model.starts_with("gpt-") || model.starts_with("codex-") {
        return Some("openai");
    }
    None
}

fn timestamp(millis: u64) -> String {
    let seconds = millis / 1000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let (year, month, day) = nanocode_platform::civil_from_days(days);
    let rest = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        millis % 1000
    )
}

fn write(directory: &Path, lines: &mpsc::Receiver<Line>) {
    let mut failed = false;
    for line in lines {
        let (at, line) = match line {
            Line::Record(at, line) => (at, line),
            Line::Flush(done) => {
                let _ = done.send(());
                continue;
            }
        };
        let stamp = timestamp(at);
        let path = directory.join(format!("usage-{}.jsonl", &stamp[..7]));
        let written = std::fs::create_dir_all(directory).and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut file| file.write_all(format!("{line}\n").as_bytes()))
        });
        match written {
            Ok(()) => failed = false,
            Err(error) if !failed => {
                failed = true;
                nanocode_trace::warn!(path = %path.display(), %error, "a Forge usage record was not written");
            }
            Err(_) => {}
        }
    }
}
