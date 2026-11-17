use std::collections::HashSet;
use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};

use serde_json::{Map, Value};

use nanocode_sync::Atom;

use super::{CellGuard, CodeModeToolset, ToolOutputBody, ToolOutputContent};
use crate::code_mode_description::image_reader;
use crate::embedded::{NestedToolCall, OwnedToolContext};

const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];
const PATH_PUNCTUATION: &str = "/._-+@%";
const WORKING_DIRECTORY: &str = "workdir";
const SHOWN_FIGURES: [&str; 4] = [
    "Figure_1.png",
    "Figure_2.png",
    "Figure_3.png",
    "Figure_4.png",
];

#[derive(Clone, Default)]
pub(super) struct Carried {
    pending: Vec<String>,
    shown: HashSet<u64>,
}

pub(super) struct ImageFiles {
    reader: Option<(String, &'static str)>,
    limit: usize,
    hasher: BuildHasherDefault<DefaultHasher>,
    shown: HashSet<u64>,
    files: Vec<(String, Option<u64>)>,
    touched: bool,
}

impl ImageFiles {
    pub(super) fn new(metadata: &[Value], guard: Option<CellGuard>, carried: &Carried) -> Self {
        let reader = metadata.iter().find_map(|tool| {
            let name = tool
                .get("tool_name")
                .or_else(|| tool.get("name"))
                .and_then(Value::as_str)?;
            image_reader(name).map(|field| (name.to_owned(), field))
        });
        Self {
            reader,
            limit: guard.map_or(usize::MAX, |guard| {
                usize::try_from(guard.calls_per_tool).unwrap_or(usize::MAX)
            }),
            hasher: BuildHasherDefault::default(),
            shown: carried.shown.clone(),
            files: carried
                .pending
                .iter()
                .map(|path| (path.clone(), None))
                .collect(),
            touched: false,
        }
    }

    pub(super) fn shown(&mut self, content: &ToolOutputContent) {
        if let ToolOutputContent::InputImage { image_url, .. } = content {
            self.shown.insert(self.hasher.hash_one(image_url));
        }
    }

    fn reads(&self, name: &str) -> bool {
        self.reader
            .as_ref()
            .is_none_or(|(reader, _)| reader == name)
    }

    pub(super) async fn before(
        &mut self,
        name: &str,
        input: &Value,
        replaying: bool,
        tools: &dyn CodeModeToolset,
        context: &OwnedToolContext,
    ) {
        if self.reads(name) {
            return;
        }
        self.touched = true;
        let mut paths = Vec::new();
        mentioned(input, &mut paths);
        if runs_python(input) {
            for figure in SHOWN_FIGURES {
                if !paths.iter().any(|known| known == figure) {
                    paths.push(figure.to_owned());
                }
            }
        }
        if let Some(directory) = input
            .get(WORKING_DIRECTORY)
            .and_then(Value::as_str)
            .filter(|directory| !directory.is_empty())
        {
            paths = paths
                .iter()
                .chain(self.files.iter().map(|(path, _)| path))
                .map(|path| within(directory, path))
                .collect();
        }
        for path in paths {
            if !self.room(&path) {
                continue;
            }
            let before = if replaying {
                None
            } else {
                self.read(&path, tools, context).await.map(|(hash, _)| hash)
            };
            self.files.push((path, before));
        }
    }

    pub(super) fn settled(&mut self, call: &NestedToolCall) {
        if self.reads(&call.name) || !call.success {
            return;
        }
        let mut paths = Vec::new();
        match &call.output {
            ToolOutputBody::Text(text) => mentioned_text(text, &mut paths),
            ToolOutputBody::Content(content) => {
                for item in content {
                    if let ToolOutputContent::InputText { text } = item {
                        mentioned_text(text, &mut paths);
                    }
                }
            }
        }
        mentioned(&call.structured_result, &mut paths);
        let [path] = paths.as_slice() else {
            return;
        };
        let directory = call
            .input
            .get(WORKING_DIRECTORY)
            .and_then(Value::as_str)
            .filter(|directory| !directory.is_empty());
        let path = directory.map_or_else(|| path.clone(), |directory| within(directory, path));
        if self.room(&path) {
            self.files.push((path, None));
        }
    }

    fn room(&self, path: &str) -> bool {
        self.files.len() < self.limit && !self.files.iter().any(|(known, _)| known == path)
    }

    pub(super) async fn changed(
        &mut self,
        tools: &dyn CodeModeToolset,
        context: &OwnedToolContext,
        carried: &Atom<Carried>,
    ) -> Vec<ToolOutputContent> {
        let files = if self.touched {
            std::mem::take(&mut self.files)
        } else {
            Vec::new()
        };
        let mut images = Vec::new();
        let mut missing = Vec::new();
        for (path, before) in &files {
            let Some((hash, image)) = self.read(path, tools, context).await else {
                missing.push(path.clone());
                continue;
            };
            if *before != Some(hash) && self.shown.insert(hash) {
                images.push(image);
            }
        }
        carried.update(|carried| {
            carried
                .pending
                .retain(|path| !files.iter().any(|(known, _)| known == path));
            carried.pending.extend(missing.iter().cloned());
            carried.shown.extend(self.shown.iter().copied());
        });
        images
    }

    async fn read(
        &self,
        path: &str,
        tools: &dyn CodeModeToolset,
        context: &OwnedToolContext,
    ) -> Option<(u64, ToolOutputContent)> {
        let (reader, field) = self.reader.as_ref()?;
        let mut input = Map::new();
        input.insert((*field).to_owned(), Value::from(path));
        let output = tools
            .execute_nested(reader, Value::Object(input), context.as_context())
            .await;
        if !output.success {
            return None;
        }
        let ToolOutputBody::Content(content) = output.output else {
            return None;
        };
        content.into_iter().find_map(|item| match &item {
            ToolOutputContent::InputImage { image_url, .. } => {
                Some((self.hasher.hash_one(image_url), item))
            }
            _ => None,
        })
    }
}

fn within(directory: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("{}/{path}", directory.trim_end_matches('/'))
    }
}

fn runs_python(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains("python") || text.contains(".py"),
        Value::Array(items) => items.iter().any(runs_python),
        Value::Object(fields) => fields.values().any(runs_python),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn mentioned_text(text: &str, paths: &mut Vec<String>) {
    for token in text.split(|c: char| !c.is_alphanumeric() && !PATH_PUNCTUATION.contains(c)) {
        let image = token.rsplit_once('.').is_some_and(|(stem, extension)| {
            !stem.is_empty()
                && IMAGE_EXTENSIONS
                    .iter()
                    .any(|known| extension.eq_ignore_ascii_case(known))
        });
        if image && !paths.iter().any(|known| known == token) {
            paths.push(token.to_owned());
        }
    }
}

fn mentioned(value: &Value, paths: &mut Vec<String>) {
    match value {
        Value::String(text) => mentioned_text(text, paths),
        Value::Array(items) => items.iter().for_each(|item| mentioned(item, paths)),
        Value::Object(fields) => fields.values().for_each(|item| mentioned(item, paths)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}
