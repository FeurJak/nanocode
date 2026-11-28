use std::path::{Path, PathBuf};

const DIRECTORY: &str = "commands";
const PROJECT_DIRECTORY: &str = ".nanocode";
const EXTENSION: &str = "md";
const ARGUMENTS: &str = "$ARGUMENTS";

pub struct UserCommand {
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
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

fn parse(name: &str, path: PathBuf, text: &str) -> UserCommand {
    let (front, body) = match text
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
    {
        Some((front, body)) => (front, body),
        None => ("", text),
    };
    let description = front.lines().find_map(|line| {
        line.strip_prefix("description:")
            .map(|value| value.trim().trim_matches('"').to_owned())
            .filter(|value| !value.is_empty())
    });
    UserCommand {
        name: name.to_owned(),
        path,
        description,
        body: body.trim_start_matches('\n').to_owned(),
    }
}

impl UserCommand {
    pub fn prompt(&self, arguments: &str) -> String {
        let arguments = arguments.trim();
        match (self.body.contains(ARGUMENTS), arguments.is_empty()) {
            (true, _) => self.body.replace(ARGUMENTS, arguments),
            (false, true) => self.body.clone(),
            (false, false) => format!("{}\n\nARGUMENTS: {arguments}", self.body.trim_end()),
        }
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
                let arguments = arguments.clone();
                nanocode_platform::spawn_blocking(move || {
                    find(&home, &workspace, &name)
                        .map(|command| command.map(|command| command.prompt(&arguments)))
                        .map_err(|error| error.to_string())
                })
            };
            let found = lookup.await.unwrap_or_else(|error| Err(error.to_string()));
            drop(tx.send(crate::app::AppEvent::UserCommand {
                name,
                arguments,
                found,
            }));
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
