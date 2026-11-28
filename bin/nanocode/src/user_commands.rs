use std::path::{Path, PathBuf};

const DIRECTORY: &str = "commands";
const PROJECT_DIRECTORY: &str = ".nanocode";
const EXTENSION: &str = "md";
const ARGUMENTS: &str = "$ARGUMENTS";

pub struct UserCommand {
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub subagent: Option<bool>,
    body: String,
}

pub fn directories(home: &Path, workspace: &Path) -> [PathBuf; 2] {
    [
        workspace.join(PROJECT_DIRECTORY).join(DIRECTORY),
        home.join(DIRECTORY),
    ]
}

fn valid(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | ':')
        })
}

pub fn find(home: &Path, workspace: &Path, name: &str) -> std::io::Result<Option<UserCommand>> {
    if !valid(name) {
        return Ok(None);
    }
    for directory in directories(home, workspace) {
        let path = directory.join(format!("{name}.{EXTENSION}"));
        match std::fs::read_to_string(&path) {
            Ok(text) => return Ok(Some(parse(name, path, &text))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("{}: {error}", path.display()),
                ));
            }
        }
    }
    Ok(None)
}

pub fn list(home: &Path, workspace: &Path) -> std::io::Result<Vec<UserCommand>> {
    let mut found: Vec<UserCommand> = Vec::new();
    for directory in directories(home, workspace) {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("{}: {error}", directory.display()),
                ));
            }
        };
        let mut names = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == EXTENSION)
            })
            .filter_map(|path| {
                let name = path.file_stem()?.to_str()?.to_owned();
                valid(&name).then_some(name)
            })
            .collect::<Vec<_>>();
        names.sort();
        for name in names {
            if found.iter().any(|command| command.name == name) {
                continue;
            }
            if let Some(command) = find(home, workspace, &name)? {
                found.push(command);
            }
        }
    }
    found.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(found)
}

fn field(front: &str, key: &str) -> Option<String> {
    front.lines().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix(':'))
            .map(|value| value.trim().trim_matches('"').trim_matches('\'').to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn parse(name: &str, path: PathBuf, text: &str) -> UserCommand {
    let (front, body) = match text
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
    {
        Some((front, body)) => (front, body),
        None => ("", text),
    };
    let flag = |key| field(front, key).map(|value| value == "true");
    UserCommand {
        name: name.to_owned(),
        path,
        description: field(front, "description"),
        model: field(front, "model"),
        agent: field(front, "agent"),
        subagent: flag("subagent").or_else(|| flag("subtask")),
        body: body.trim().to_owned(),
    }
}

pub struct Invocation {
    pub name: String,
    pub arguments: String,
    pub prompt: String,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub subagent: Option<bool>,
}

fn words(arguments: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    for character in arguments.chars() {
        match (quote, character) {
            (Some(open), character) if character == open => quote = None,
            (Some(_), character) => current.push(character),
            (None, '"' | '\'') => {
                quote = Some(character);
                started = true;
            }
            (None, character) if character.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, character) => {
                current.push(character);
                started = true;
            }
        }
    }
    if started {
        words.push(current);
    }
    words
}

fn positions(body: &str) -> usize {
    let bytes = body.as_bytes();
    let mut highest = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'$' {
            let digits = bytes[index + 1..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if let Ok(number) = body[index + 1..index + 1 + digits].parse::<usize>() {
                highest = highest.max(number);
            }
        }
    }
    highest
}

fn substituted(body: &str, arguments: &str) -> String {
    let highest = positions(body);
    let words = words(arguments);
    let mut text = body.replace(ARGUMENTS, arguments);
    for number in (1..=highest).rev() {
        let value = match number == highest {
            true => words
                .get(number - 1..)
                .map(|rest| rest.join(" "))
                .unwrap_or_default(),
            false => words.get(number - 1).cloned().unwrap_or_default(),
        };
        text = text.replace(&format!("${number}"), &value);
    }
    match (
        body.contains(ARGUMENTS) || highest > 0,
        arguments.is_empty(),
    ) {
        (false, false) => format!("{text}\n\n{arguments}"),
        _ => text,
    }
}

fn shell_blocks(text: &str, cwd: &Path) -> std::io::Result<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_owned());
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("!`") {
        let Some(length) = rest[start + 2..].find('`') else {
            break;
        };
        out.push_str(&rest[..start]);
        let command = &rest[start + 2..start + 2 + length];
        let output = std::process::Command::new(&shell)
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .output()?;
        out.push_str(String::from_utf8_lossy(&output.stdout).trim_end());
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            out.push('\n');
            out.push_str(stderr.trim_end());
        }
        rest = &rest[start + 3 + length..];
    }
    out.push_str(rest);
    Ok(out)
}

impl UserCommand {
    pub fn invoke(&self, arguments: &str, cwd: &Path) -> std::io::Result<Invocation> {
        let arguments = arguments.trim();
        Ok(Invocation {
            name: self.name.clone(),
            arguments: arguments.to_owned(),
            prompt: shell_blocks(&substituted(&self.body, arguments), cwd)?,
            model: self.model.clone(),
            agent: self.agent.clone(),
            subagent: self.subagent,
        })
    }
}

impl crate::app::App {
    pub(crate) fn user_command(&mut self, name: &str, arguments: &str) {
        let home = self.home.clone();
        let workspace = self.host.config.workspace.clone();
        let tx = self.tx.clone();
        let name = name.to_owned();
        let arguments = arguments.to_owned();
        nanocode_platform::spawn_task(async move {
            let lookup = {
                let name = name.clone();
                nanocode_platform::spawn_blocking(move || {
                    find(&home, &workspace, &name)
                        .and_then(|command| {
                            command
                                .map(|command| command.invoke(&arguments, &workspace))
                                .transpose()
                        })
                        .map_err(|error| error.to_string())
                })
            };
            let found = lookup.await.unwrap_or_else(|error| Err(error.to_string()));
            drop(tx.send(crate::app::AppEvent::UserCommand { name, found }));
        });
    }

    pub(crate) fn list_user_commands(&mut self) {
        let home = self.home.clone();
        let workspace = self.host.config.workspace.clone();
        let tx = self.tx.clone();
        nanocode_platform::spawn_task(async move {
            let paths = directories(&home, &workspace);
            let listed = nanocode_platform::spawn_blocking(move || list(&home, &workspace)).await;
            let text = match listed {
                Ok(Ok(commands)) if commands.is_empty() => format!(
                    "no commands of your own; add <name>.md to {} or {}",
                    paths[0].display(),
                    paths[1].display()
                ),
                Ok(Ok(commands)) => std::iter::once("your commands:".to_owned())
                    .chain(commands.iter().map(|command| {
                        format!(
                            "  /{} · {} · {}",
                            command.name,
                            command.description.as_deref().unwrap_or("no description"),
                            command.path.display()
                        )
                    }))
                    .collect::<Vec<_>>()
                    .join("\n"),
                Ok(Err(error)) => format!("commands: {error}"),
                Err(error) => format!("commands: {error}"),
            };
            drop(tx.send(crate::app::AppEvent::Notice(
                nanocode_tui::transcript::NoticeLevel::Info,
                text,
            )));
        });
    }
}

fn model_name(model: &str) -> (String, Option<String>) {
    let (model, variant) = model
        .split_once('#')
        .map_or((model, None), |(model, variant)| {
            (model, Some(variant.to_owned()))
        });
    let bare = model.rsplit_once('/').map_or(model, |(_, model)| model);
    let mut name = String::new();
    let characters = bare.chars().collect::<Vec<_>>();
    for (index, character) in characters.iter().enumerate() {
        let digits = index > 0
            && characters[index - 1].is_ascii_digit()
            && characters.get(index + 1).is_some_and(char::is_ascii_digit);
        name.push(if *character == '.' && digits {
            '-'
        } else {
            *character
        });
    }
    (name, variant)
}

impl crate::app::App {
    pub(crate) fn run_invocation(&mut self, invocation: Invocation) {
        let resolved = match invocation.model.as_deref().map(model_name) {
            Some((name, variant)) => {
                let thinking = match variant.as_deref().map(crate::host::parse_thinking) {
                    Some(Ok(thinking)) => Some(thinking),
                    Some(Err(error)) => {
                        return self.notice(
                            nanocode_tui::transcript::NoticeLevel::Error,
                            format!("/{}: model {name}: {error}", invocation.name),
                        );
                    }
                    None => None,
                };
                match self.host.config.clients.resolve(&name) {
                    Ok((_, model)) => Some((model, thinking)),
                    Err(error) => {
                        return self.notice(
                            nanocode_tui::transcript::NoticeLevel::Error,
                            format!("/{}: model {name}: {error}", invocation.name),
                        );
                    }
                }
            }
            None => None,
        };
        let other_model = resolved.as_ref().is_some_and(|(model, thinking)| {
            *model != self.model || thinking.is_some_and(|thinking| thinking != self.thinking)
        });
        let child = invocation
            .subagent
            .unwrap_or(invocation.agent.is_some() || other_model);
        let session = self
            .agent
            .as_ref()
            .map(|agent| agent.session_id().to_owned());
        match (child, session) {
            (true, Some(session)) => self.start_command_child(invocation, resolved, session),
            (true, None) => {
                self.notice(
                    nanocode_tui::transcript::NoticeLevel::Warning,
                    format!(
                        "/{} runs in a subagent, which needs a session; send a prompt first",
                        invocation.name
                    ),
                );
            }
            (false, _) => {
                if invocation.agent.is_some() || other_model {
                    self.notice(
                        nanocode_tui::transcript::NoticeLevel::Info,
                        format!("/{} has subagent: false, so it runs here on this thread's model and without its agent", invocation.name),
                    );
                }
                self.command_span = Some((invocation.name, invocation.arguments));
                self.send(invocation.prompt, true);
            }
        }
    }

    fn start_command_child(
        &mut self,
        invocation: Invocation,
        resolved: Option<(nanocode::ModelSelection, Option<nanocode::Thinking>)>,
        session: String,
    ) {
        let runtime = self.host.runtime.clone();
        let usage = self.host.config.usage.clone();
        let tx = self.tx.clone();
        let role = invocation
            .agent
            .clone()
            .unwrap_or_else(|| "general".to_owned());
        nanocode_platform::spawn_task(async move {
            let mut options = nanocode::agent::SpawnOptions::new();
            if let Some((model, thinking)) = resolved {
                options = options.harness_model(model);
                if let Some(thinking) = thinking {
                    options = options.thinking(thinking);
                }
            }
            let prompt = invocation.prompt.clone();
            let task = nanocode::subagents::AgentTask {
                role: role.clone(),
                task: invocation.prompt,
                output_schema: serde_json::json!({"type": "string"}),
            };
            let text = match runtime.subagents() {
                Some(control) => match control.start(&session, task, options).await {
                    Ok(report) => {
                        if let Some(usage) = &usage {
                            usage.child_command(
                                &session,
                                report.agent_id,
                                (invocation.name.clone(), invocation.arguments.clone()),
                            );
                        }
                        let model = report
                            .model
                            .map(|model| format!(" on {model}"))
                            .unwrap_or_default();
                        drop(tx.send(crate::app::AppEvent::CommandChild(format!(
                            "I ran /{}{}. It runs as subagent {} ({role}{model}) with this task:\n\n{prompt}\n\nIts result comes back to you when it finishes. Until then do nothing for it; when it arrives, give me its result.",
                            invocation.name,
                            match invocation.arguments.is_empty() {
                                true => String::new(),
                                false => format!(" {}", invocation.arguments),
                            },
                            report.agent_id
                        ))));
                        (
                            nanocode_tui::transcript::NoticeLevel::Info,
                            format!(
                                "/{} runs as subagent {} ({role}{model}); its result comes back to this thread when it finishes",
                                invocation.name, report.agent_id
                            ),
                        )
                    }
                    Err(error) => (
                        nanocode_tui::transcript::NoticeLevel::Error,
                        format!("/{}: the subagent did not start: {error}", invocation.name),
                    ),
                },
                None => (
                    nanocode_tui::transcript::NoticeLevel::Error,
                    format!("/{}: subagents are off in this session", invocation.name),
                ),
            };
            drop(tx.send(crate::app::AppEvent::Notice(text.0, text.1)));
        });
    }
}
