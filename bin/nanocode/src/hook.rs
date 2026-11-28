use std::{path::PathBuf, sync::Arc, time::Duration};

use nanocode::{
    HostFuture,
    tools::hold::{
        Answer, HoldCall, HoldLedger, HoldPolicy, Holds, Judgment, LedgerError, Resumption, Running,
    },
};
use nanocode_io::AsyncWriteExt as _;
use nanocode_sync::{Atom, mpsc::UnboundedSender, oneshot};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};

const CLAUDE_CODE_TIMEOUT: Duration = Duration::from_mins(1);
const REFUSED: u8 = 2;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    #[serde(default)]
    pub matcher: String,
    pub command: Vec<String>,
    pub timeout: Option<u64>,
}

pub struct Ask {
    pub tool: String,
    pub input: String,
    pub reason: String,
    pub answer: oneshot::Sender<bool>,
}

pub enum Report {
    Ask(Ask),
    Notice(String),
}

pub struct Hook {
    entries: Vec<Entry>,
    workspace: PathBuf,
    sink: Atom<Option<UnboundedSender<Report>>>,
}

enum Decision {
    Allow,
    Deny(String),
    Ask(String),
}

impl Hook {
    pub fn new(entries: Vec<Entry>, workspace: PathBuf) -> Self {
        Self {
            entries,
            workspace,
            sink: Atom::new(None),
        }
    }

    pub fn report_to(&self, sink: UnboundedSender<Report>) {
        self.sink.set(Some(sink));
    }

    pub fn holds(self: &Arc<Self>) -> Holds {
        Holds::new(Arc::clone(self) as Arc<dyn HoldPolicy>, Arc::new(Unheld))
    }

    fn notice(&self, text: String) {
        match self.sink.read(Option::clone) {
            Some(sink) => drop(sink.send(Report::Notice(text))),
            None => eprintln!("nanocode: {text}"),
        }
    }

    async fn decide(&self, call: &HoldCall<'_>) -> Decision {
        let cwd = call
            .directory
            .map_or_else(|| self.workspace.clone(), PathBuf::from);
        let input = serde_json::from_str::<Value>(call.input.get())
            .unwrap_or_else(|_| Value::String(call.input.get().to_owned()));
        let payload = json!({
            "session_id": call.session_id,
            "cwd": cwd.display().to_string(),
            "permission_mode": "default",
            "hook_event_name": "PreToolUse",
            "tool_name": call.tool,
            "tool_input": input,
            "tool_use_id": call.call_id,
        })
        .to_string();
        let mut asked = None;
        for entry in self
            .entries
            .iter()
            .filter(|entry| matches(&entry.matcher, call.tool))
        {
            match run(entry, &payload, &cwd).await {
                Ok(Decision::Allow) => {}
                Ok(Decision::Deny(reason)) => return Decision::Deny(reason),
                Ok(Decision::Ask(reason)) => asked = asked.or(Some(reason)),
                Err(error) => self.notice(format!(
                    "the pre-tool hook {:?} failed, so {} ran unchecked: {error}",
                    entry.command.join(" "),
                    call.tool
                )),
            }
        }
        asked.map_or(Decision::Allow, Decision::Ask)
    }
}

pub fn matches(matcher: &str, tool: &str) -> bool {
    let matcher = matcher.trim();
    matcher.is_empty()
        || matcher
            .split('|')
            .map(str::trim)
            .any(|name| match name.strip_suffix('*') {
                Some(prefix) => tool.starts_with(prefix),
                None => name == tool,
            })
}

async fn run(entry: &Entry, payload: &str, cwd: &std::path::Path) -> Result<Decision, String> {
    let (program, args) = entry.command.split_first().ok_or("its command is empty")?;
    let limit = entry
        .timeout
        .map_or(CLAUDE_CODE_TIMEOUT, Duration::from_millis);
    let mut command = nanocode_executor::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env("CLAUDE_PROJECT_DIR", cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let mut stdin = child.stdin.take().ok_or("its stdin was not opened")?;
    let finished = nanocode_platform::timeout(limit, async move {
        stdin
            .write_all(payload.as_bytes())
            .await
            .map_err(|error| error.to_string())?;
        drop(stdin);
        child
            .wait_with_output()
            .await
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|_| format!("it ran past its {} ms timeout", limit.as_millis()))??;
    let stderr = String::from_utf8_lossy(&finished.stderr).trim().to_owned();
    match finished.status.code() {
        Some(0) => Ok(parsed(&String::from_utf8_lossy(&finished.stdout))),
        Some(code) if code == i32::from(REFUSED) => Ok(Decision::Deny(match stderr.is_empty() {
            true => "the pre-tool hook refused this call".to_owned(),
            false => stderr,
        })),
        Some(code) => Err(format!("it exited with {code}: {stderr}")),
        None => Err(format!("it was killed by a signal: {stderr}")),
    }
}

fn parsed(stdout: &str) -> Decision {
    let Ok(reply) = serde_json::from_str::<Value>(stdout.trim()) else {
        return Decision::Allow;
    };
    let specific = &reply["hookSpecificOutput"];
    let reason = specific["permissionDecisionReason"]
        .as_str()
        .or_else(|| reply["reason"].as_str())
        .or_else(|| reply["stopReason"].as_str())
        .unwrap_or_default()
        .to_owned();
    let or = |fallback: &str| match reason.is_empty() {
        true => fallback.to_owned(),
        false => reason.clone(),
    };
    if reply["continue"] == Value::Bool(false) {
        return Decision::Deny(or("the pre-tool hook stopped this call"));
    }
    match (
        specific["permissionDecision"].as_str(),
        reply["decision"].as_str(),
    ) {
        (Some("deny"), _) | (None, Some("block")) => {
            Decision::Deny(or("the pre-tool hook refused this call"))
        }
        (Some("ask"), _) => Decision::Ask(reason),
        _ => Decision::Allow,
    }
}

impl HoldPolicy for Hook {
    fn judge<'a>(&'a self, call: HoldCall<'a>) -> HostFuture<'a, Judgment> {
        Box::pin(async move {
            match self.decide(&call).await {
                Decision::Allow => Judgment::Run,
                Decision::Deny(message) => Judgment::Refuse { message },
                Decision::Ask(reason) => {
                    let Some(sink) = self.sink.read(Option::clone) else {
                        return Judgment::Refuse {
                            message: format!(
                                "the pre-tool hook asked for a person to approve {}, and nobody can answer here: {reason}",
                                call.tool
                            ),
                        };
                    };
                    let (answer, answered) = oneshot::channel();
                    let ask = Ask {
                        tool: call.tool.to_owned(),
                        input: call.input.get().to_owned(),
                        reason: reason.clone(),
                        answer,
                    };
                    if sink.send(Report::Ask(ask)).is_err() {
                        return Judgment::Refuse {
                            message: format!("{} was not approved: the prompt closed", call.tool),
                        };
                    }
                    match answered.await {
                        Ok(true) => Judgment::Run,
                        Ok(false) | Err(_) => Judgment::Refuse {
                            message: format!(
                                "the user refused {} when the pre-tool hook asked{}",
                                call.tool,
                                match reason.is_empty() {
                                    true => String::new(),
                                    false => format!(": {reason}"),
                                }
                            ),
                        },
                    }
                }
            }
        })
    }
}

struct Unheld;

impl HoldLedger for Unheld {
    fn hold<'a>(
        &'a self,
        key: &'a str,
        _call: HoldCall<'a>,
        _card: &'a RawValue,
    ) -> HostFuture<'a, Result<(), LedgerError>> {
        Box::pin(async move {
            Err(LedgerError::Journal(format!(
                "the pre-tool hook does not hold calls ({key})"
            )))
        })
    }

    fn resolve<'a>(
        &'a self,
        key: &'a str,
        _answer: Answer,
    ) -> HostFuture<'a, Result<Resumption, LedgerError>> {
        Box::pin(async move {
            Err(LedgerError::Journal(format!(
                "the pre-tool hook does not hold calls ({key})"
            )))
        })
    }

    fn running<'a>(&'a self, _call_id: &'a str) -> HostFuture<'a, Option<Running>> {
        Box::pin(async { None })
    }

    fn claim<'a>(&'a self, _key: &'a str) -> HostFuture<'a, bool> {
        Box::pin(async { false })
    }

    fn release<'a>(&'a self, _key: &'a str) -> HostFuture<'a, Result<(), LedgerError>> {
        Box::pin(async { Ok(()) })
    }
}

impl crate::app::App {
    pub(crate) fn open_ask(&mut self) {
        let Some(ask) = self.asks.front() else {
            return;
        };
        if matches!(
            self.overlay,
            Some(crate::app::Overlay::Picker(
                crate::app::PickerPurpose::Permission,
                _
            ))
        ) {
            return;
        }
        let reason = match ask.reason.is_empty() {
            true => "the pre-tool hook asks for approval".to_owned(),
            false => ask.reason.clone(),
        };
        let picker = nanocode_tui::picker::Picker::new(
            format!("Allow {}? {reason}", ask.tool),
            vec![
                nanocode_tui::picker::PickerItem::choice("Allow").detail(ask.input.clone()),
                nanocode_tui::picker::PickerItem::choice("Refuse")
                    .detail("the model is told the user refused it"),
            ],
        )
        .footer(match self.asks.len() {
            1 => "enter answers · esc refuses".to_owned(),
            waiting => format!("enter answers · esc refuses · {waiting} calls waiting"),
        });
        self.overlay = Some(crate::app::Overlay::Picker(
            crate::app::PickerPurpose::Permission,
            picker,
        ));
    }

    pub(crate) fn answer_ask(&mut self, allow: bool) {
        if let Some(ask) = self.asks.pop_front() {
            let _ = ask.answer.send(allow);
        }
        self.open_ask();
    }
}
