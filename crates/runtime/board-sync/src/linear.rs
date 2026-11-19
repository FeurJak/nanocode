use std::collections::BTreeMap;

use nanocode_board::{NewTask, TaskId, mirror, tasks::Terms};
use serde_json::Value;

use super::github::{Parts, keyed, marker, millis, names, tagged, text};
use super::{Author, Event, Inbound, Mode, PlatformError, Target, seconds, throttled};

pub const SYNC: &str = "linear";

const TAG: &str = "tag.";
const ASSIGNEE: &str = "assignee.";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub bot: Option<String>,
}

fn thread(identifier: &str) -> Target {
    Target::new(SYNC, identifier)
}

fn reconcile(data: &Value) -> (BTreeMap<String, String>, Vec<String>) {
    let mut set = BTreeMap::new();
    let mut own = Vec::new();
    if data.get("labels").is_some_and(Value::is_array) {
        tagged(&mut set, TAG, names(data, "/labels", "/name"));
        own.push(TAG.to_owned());
    }
    if data.get("assignee").is_some() || data.get("assigneeId").is_some() {
        tagged(&mut set, ASSIGNEE, text(data, "/assignee/name").into_iter());
        own.push(ASSIGNEE.to_owned());
    }
    (set, own)
}

fn issue(config: &Config, action: &str, payload: &Value, data: &Value) -> Option<Parts> {
    let identifier = text(data, "/identifier")?;
    let task = TaskId(format!("{SYNC}/{identifier}"));
    let (mut set, own) = reconcile(data);
    if let Some(status) = text(data, "/state/name") {
        set.insert("status".to_owned(), status.to_owned());
    }
    match action {
        "create" => {
            let spec = mirror::spec(text(data, "/title")?, text(data, "/description"));
            set.insert("issue".to_owned(), identifier.to_owned());
            let new = NewTask { id: task, parent: None, spec, labels: set, needs: Vec::new(), terms: Terms::default() };
            let opened = Event::Opened { task: new, thread: thread(identifier), message: format!("issue:{identifier}"), mode: Mode::Always };
            Some(vec![("opened", opened)])
        }
        "update" => {
            let version = text(data, "/updatedAt").and_then(millis)?;
            let mut parts = vec![("labels", Event::Labels { task, set, unset: Vec::new(), own, version })];
            let changed = |field: &str| payload.pointer(&format!("/updatedFrom/{field}")).is_some();
            if changed("title") || changed("description") {
                let user = text(payload, "/actor/id")?;
                let name = text(payload, "/actor/name").unwrap_or(user);
                let author = Author { user: user.to_owned(), name: name.to_owned(), bot: config.bot.as_deref() == Some(user) };
                let title = text(data, "/title")?.to_owned();
                let body = text(data, "/description").map(str::to_owned);
                parts.push(("reworded", Event::Reworded { thread: thread(identifier), author, title, body, version }));
            }
            Some(parts)
        }
        _ => None,
    }
}

fn author(config: &Config, data: &Value) -> Option<Author> {
    let bot = data.get("botActor").is_some_and(|actor| !actor.is_null());
    let user = text(data, "/user/id").or_else(|| text(data, "/botActor/id"))?;
    let name = text(data, "/user/name").or_else(|| text(data, "/botActor/name")).unwrap_or(user);
    Some(Author { user: user.to_owned(), name: name.to_owned(), bot: bot || config.bot.as_deref() == Some(user) })
}

fn linked(payload: &Value, data: &Value) -> Option<String> {
    if let Some(identifier) = text(data, "/issue/identifier") {
        return Some(identifier.to_owned());
    }
    let url = text(data, "/url").or_else(|| text(payload, "/url"));
    let slug = url.and_then(|url| url.split_once("/issue/")).and_then(|(_, rest)| rest.split(['/', '#', '?']).next());
    slug.filter(|slug| !slug.is_empty()).or_else(|| text(data, "/issueId")).map(str::to_owned)
}

fn comment(config: &Config, action: &str, payload: &Value, data: &Value) -> Option<Parts> {
    let identifier = linked(payload, data)?;
    let identifier = identifier.as_str();
    let thread = thread(identifier);
    let message = format!("c:{}", text(data, "/id")?);
    let event = match action {
        "create" => {
            let (body, marker) = marker(text(data, "/body")?);
            Event::Message { thread, message, author: author(config, data)?, body, context: None, sub: None, marker }
        }
        "update" => {
            let version = text(data, "/updatedAt").and_then(millis)?;
            Event::Edited { thread, message, body: marker(text(data, "/body")?).0, version }
        }
        "remove" => Event::Deleted { thread, message },
        _ => return None,
    };
    Some(vec![("comment", event)])
}

pub fn parse(delivery: &str, payload: &Value) -> Vec<Inbound> {
    parse_with(&Config::default(), delivery, payload)
}

pub fn parse_with(config: &Config, delivery: &str, payload: &Value) -> Vec<Inbound> {
    let (Some(kind), Some(action), Some(data)) = (text(payload, "/type"), text(payload, "/action"), payload.get("data")) else {
        return Vec::new();
    };
    let parts = match kind {
        "Issue" => issue(config, action, payload, data),
        "Comment" => comment(config, action, payload, data),
        _ => None,
    };
    keyed(delivery, parts.unwrap_or_default())
}

pub fn limited(headers: &BTreeMap<String, String>, body: &Value, now_ms: u64) -> Option<PlatformError> {
    let errors = body.pointer("/errors").and_then(Value::as_array)?;
    if !errors.iter().any(|error| text(error, "/extensions/code") == Some("RATELIMITED")) {
        return None;
    }
    if let Some(header) = headers.get("retry-after") {
        return Some(throttled(seconds(header)));
    }
    let mut retry_after_ms = None;
    for name in ["x-ratelimit-requests-reset", "x-ratelimit-complexity-reset"] {
        if let Some(header) = headers.get(name) {
            let Ok(reset) = header.trim().parse::<u64>() else {
                return Some(throttled(None));
            };
            retry_after_ms = retry_after_ms.max(Some(reset.saturating_sub(now_ms)));
        }
    }
    Some(throttled(retry_after_ms))
}
