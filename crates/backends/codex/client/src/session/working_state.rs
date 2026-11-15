use std::{collections::BTreeMap, fmt};

use crate::client::{
    ContentItem, FunctionOutputBody, FunctionOutputContent, MessageRole, ResponseItem,
};

use super::compaction::estimate_item_tokens;

const STATE_OPEN: &str = "<working_state>";
const STATE_CLOSE: &str = "</working_state>";
const ELIDED_OUTPUT: &str = "[output elided by the working state budget";
const ELIDED_INPUT: &str = "[input elided by the working state budget";
const EXIT_MARKER: &str = "Process exited with code ";
const PATCH_MARKERS: [&str; 3] = ["*** Add File: ", "*** Update File: ", "*** Delete File: "];
const COMMAND_PREVIEW_CHARS: usize = 160;
const FILES_PREFIX: &str = "Files patched: ";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkingStatePolicy {
    budget_tokens: u64,
    retain_tokens: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub enum WorkingStatePolicyError {
    ZeroBudget,
    RetainAboveBudget {
        budget_tokens: u64,
        retain_tokens: u64,
    },
}

impl fmt::Display for WorkingStatePolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroBudget => formatter.write_str("the working state budget must be above zero"),
            Self::RetainAboveBudget {
                budget_tokens,
                retain_tokens,
            } => write!(
                formatter,
                "the working state keeps {retain_tokens} tokens of tool output, more than its budget of {budget_tokens}"
            ),
        }
    }
}

impl std::error::Error for WorkingStatePolicyError {}

impl WorkingStatePolicy {
    pub const fn new(
        budget_tokens: u64,
        retain_tokens: u64,
    ) -> Result<Self, WorkingStatePolicyError> {
        if budget_tokens == 0 {
            return Err(WorkingStatePolicyError::ZeroBudget);
        }
        if retain_tokens > budget_tokens {
            return Err(WorkingStatePolicyError::RetainAboveBudget {
                budget_tokens,
                retain_tokens,
            });
        }
        Ok(Self {
            budget_tokens,
            retain_tokens,
        })
    }

    #[must_use]
    pub const fn budget_tokens(self) -> u64 {
        self.budget_tokens
    }

    #[must_use]
    pub const fn retain_tokens(self) -> u64 {
        self.retain_tokens
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct WorkingStateReport {
    pub elided_outputs: Vec<Box<str>>,
    pub elided_inputs: Vec<Box<str>>,
    pub tokens_before: u64,
    pub tokens_after: u64,
}

#[must_use]
pub fn apply(
    items: &[ResponseItem],
    policy: WorkingStatePolicy,
) -> Option<(Vec<ResponseItem>, WorkingStateReport)> {
    let live = items
        .iter()
        .enumerate()
        .filter(|(_, item)| output_call_id(item).is_some() && !is_elided_output(item))
        .map(|(index, item)| (index, estimate_item_tokens(item)))
        .collect::<Vec<_>>();
    let mut live_tokens = live.iter().map(|(_, tokens)| tokens).sum::<u64>();
    if live_tokens <= policy.budget_tokens {
        return None;
    }
    let calls = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| call_id(item).map(|id| (id, index)))
        .collect::<BTreeMap<_, _>>();
    let tokens_before = items.iter().map(estimate_item_tokens).sum::<u64>();
    let mut report = WorkingStateReport::default();
    let mut elided = Vec::new();
    for (index, tokens) in live {
        if live_tokens <= policy.retain_tokens {
            break;
        }
        live_tokens -= tokens;
        elided.push(index);
    }
    let mut history = items.to_vec();
    let mut results = Vec::new();
    for &index in &elided {
        let Some(id) = output_call_id(&items[index]) else {
            continue;
        };
        let text = output_text(&items[index]);
        let exit = exit_code(&text);
        let call = calls.get(id).map(|&call| &items[call]);
        results.push(format!(
            "- {} -> {}",
            call.map_or_else(|| "tool call".to_owned(), describe_call),
            exit.map_or_else(|| "completed".to_owned(), |code| format!("exit {code}"))
        ));
        set_output(
            &mut history[index],
            format!(
                "{ELIDED_OUTPUT}: {} tokens, {}; see <working_state>]",
                estimate_item_tokens(&items[index]),
                exit.map_or_else(|| "no exit code".to_owned(), |code| format!("exit {code}"))
            ),
        );
        report.elided_outputs.push(id.into());
        if let Some(&call) = calls.get(id) {
            let files = patched_files(call_text(&items[call]));
            let stub = format!(
                "{ELIDED_INPUT}: {} tokens{}]",
                estimate_item_tokens(&items[call]),
                if files.is_empty() {
                    String::new()
                } else {
                    format!("; patched {}", files.join(", "))
                }
            );
            if set_input(&mut history[call], &stub) {
                report.elided_inputs.push(id.into());
            }
        }
    }
    let previous = history
        .iter()
        .filter_map(state_text)
        .flat_map(|text| {
            text.lines()
                .filter(|line| line.starts_with("- "))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut files = history
        .iter()
        .filter_map(state_text)
        .flat_map(|text| {
            text.lines()
                .filter_map(|line| line.strip_prefix(FILES_PREFIX))
                .flat_map(|line| line.split(", ").map(str::to_owned).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    for item in items {
        for file in patched_files(call_text(item)) {
            if !files.contains(&file) {
                files.push(file);
            }
        }
    }
    let last_elided = elided.last().copied().unwrap_or_default();
    let mut rebuilt = Vec::with_capacity(history.len() + 1);
    for (index, item) in history.into_iter().enumerate() {
        if state_text(&item).is_none() {
            rebuilt.push(item);
        }
        if index == last_elided {
            rebuilt.push(state_message(&files, &previous, &results));
        }
    }
    report.tokens_before = tokens_before;
    report.tokens_after = rebuilt.iter().map(estimate_item_tokens).sum();
    Some((rebuilt, report))
}

fn state_message(files: &[String], previous: &[String], results: &[String]) -> ResponseItem {
    let mut text = format!(
        "{STATE_OPEN}\nOlder tool results and large tool inputs above were elided to keep the context within its budget. Files on disk are unchanged; rerun a command or read a file to see it again.\n"
    );
    if !files.is_empty() {
        text.push_str(FILES_PREFIX);
        text.push_str(&files.join(", "));
        text.push('\n');
    }
    text.push_str("Elided results (call -> outcome):\n");
    for line in previous.iter().chain(results) {
        text.push_str(line);
        text.push('\n');
    }
    text.push_str(STATE_CLOSE);
    ResponseItem::message(MessageRole::Developer, [ContentItem::input_text(text)])
}

fn state_text(item: &ResponseItem) -> Option<&str> {
    let ResponseItem::Message {
        role: MessageRole::Developer,
        content,
        ..
    } = item
    else {
        return None;
    };
    match content.as_slice() {
        [ContentItem::InputText { text }]
            if text.starts_with(STATE_OPEN) && text.ends_with(STATE_CLOSE) =>
        {
            Some(text)
        }
        _ => None,
    }
}

fn call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::CustomToolCall { call_id, .. } => Some(call_id),
        _ => None,
    }
}

fn output_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCallOutput { call_id, .. }
        | ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id),
        _ => None,
    }
}

fn output_text(item: &ResponseItem) -> String {
    let (ResponseItem::FunctionCallOutput { output, .. }
    | ResponseItem::CustomToolCallOutput { output, .. }) = item
    else {
        return String::new();
    };
    match output {
        FunctionOutputBody::Text(text) => text.to_string(),
        FunctionOutputBody::Content(content) => content
            .iter()
            .filter_map(|part| match part {
                FunctionOutputContent::InputText { text } => Some(&**text),
                _ => None,
            })
            .collect(),
    }
}

fn is_elided_output(item: &ResponseItem) -> bool {
    output_text(item).starts_with(ELIDED_OUTPUT)
}

fn set_output(item: &mut ResponseItem, text: String) {
    if let ResponseItem::FunctionCallOutput { output, .. }
    | ResponseItem::CustomToolCallOutput { output, .. } = item
    {
        *output = FunctionOutputBody::Text(text.into_boxed_str());
    }
}

fn call_text(item: &ResponseItem) -> &str {
    match item {
        ResponseItem::FunctionCall { arguments, .. } => arguments,
        ResponseItem::CustomToolCall { input, .. } => input,
        _ => "",
    }
}

fn set_input(item: &mut ResponseItem, stub: &str) -> bool {
    let ResponseItem::CustomToolCall { input, .. } = item else {
        return false;
    };
    if input.starts_with(ELIDED_INPUT) || input.len() <= stub.len() {
        return false;
    }
    *input = stub.into();
    true
}

fn describe_call(item: &ResponseItem) -> String {
    let (name, text) = match item {
        ResponseItem::FunctionCall {
            name, arguments, ..
        } => {
            let command = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|value| {
                    value
                        .get("cmd")
                        .and_then(|cmd| cmd.as_str().map(str::to_owned))
                });
            (name, command.unwrap_or_else(|| arguments.to_string()))
        }
        ResponseItem::CustomToolCall { name, input, .. } => (name, input.to_string()),
        _ => return "tool call".to_owned(),
    };
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    let preview = line.chars().take(COMMAND_PREVIEW_CHARS).collect::<String>();
    if preview.len() < line.len() {
        format!("{name}: {preview}…")
    } else {
        format!("{name}: {preview}")
    }
}

fn exit_code(text: &str) -> Option<i64> {
    let start = text.rfind(EXIT_MARKER)? + EXIT_MARKER.len();
    let digits = text[start..]
        .chars()
        .take_while(|char| char.is_ascii_digit() || *char == '-')
        .collect::<String>();
    digits.parse().ok()
}

fn patched_files(text: &str) -> Vec<String> {
    let mut files = Vec::new();
    for marker in PATCH_MARKERS {
        for (start, _) in text.match_indices(marker) {
            let rest = &text[start + marker.len()..];
            let end = rest
                .find(['\n', '\r', '"', '\'', '`'])
                .map_or(rest.len(), |end| end.min(rest.find("\\n").unwrap_or(end)));
            let file = rest[..end].trim();
            if !file.is_empty() && !files.iter().any(|known| known == file) {
                files.push(file.to_owned());
            }
        }
    }
    files
}
