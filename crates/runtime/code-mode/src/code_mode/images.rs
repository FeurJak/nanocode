use std::collections::HashSet;
use std::hash::{BuildHasher, RandomState};

use serde_json::{Map, Value};

use nanocode_sync::Atom;

use super::{CellGuard, CodeModeToolset, ToolOutputBody, ToolOutputContent};
use crate::code_mode_description::image_reader;
use crate::embedded::OwnedToolContext;

const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];
const PATH_PUNCTUATION: &str = "/._-+@%";
const WORKING_DIRECTORY: &str = "workdir";
/// The files python.wasm's default matplotlib backend writes on `plt.show()`, one per open
/// figure, in the working directory; a call that runs Python tracks the first few.
const SHOWN_FIGURES: [&str; 4] = [
    "Figure_1.png",
    "Figure_2.png",
    "Figure_3.png",
    "Figure_4.png",
];

pub(super) struct ImageFiles {
    reader: Option<(String, &'static str)>,
    limit: usize,
    hasher: RandomState,
    shown: HashSet<u64>,
    files: Vec<(String, Option<u64>)>,
    touched: bool,
}

impl ImageFiles {
    pub(super) fn new(metadata: &[Value], guard: Option<CellGuard>, pending: &[String]) -> Self {
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
            hasher: RandomState::new(),
            shown: HashSet::new(),
            files: pending.iter().map(|path| (path.clone(), None)).collect(),
            touched: false,
        }
    }

    pub(super) fn shown(&mut self, content: &ToolOutputContent) {
        if let ToolOutputContent::InputImage { image_url, .. } = content {
            self.shown.insert(self.hasher.hash_one(image_url));
        }
    }

    pub(super) async fn before(
        &mut self,
        name: &str,
        input: &Value,
        tools: &dyn CodeModeToolset,
        context: &OwnedToolContext,
    ) {
        if self
            .reader
            .as_ref()
            .is_none_or(|(reader, _)| reader == name)
        {
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
            if self.files.len() >= self.limit || self.files.iter().any(|(known, _)| *known == path)
            {
                continue;
            }
            let before = self.read(&path, tools, context).await.map(|(hash, _)| hash);
            self.files.push((path, before));
        }
    }

    pub(super) async fn changed(
        &mut self,
        tools: &dyn CodeModeToolset,
        context: &OwnedToolContext,
        pending: &Atom<Vec<String>>,
    ) -> Vec<ToolOutputContent> {
        let mut images = Vec::new();
        if !self.touched {
            return images;
        }
        let mut missing = Vec::new();
        for (path, before) in std::mem::take(&mut self.files) {
            let Some((hash, image)) = self.read(&path, tools, context).await else {
                missing.push(path);
                continue;
            };
            if before != Some(hash) && self.shown.insert(hash) {
                images.push(image);
            }
        }
        pending.set(missing);
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

fn mentioned(value: &Value, paths: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            for token in text.split(|c: char| !c.is_alphanumeric() && !PATH_PUNCTUATION.contains(c))
            {
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
        Value::Array(items) => items.iter().for_each(|item| mentioned(item, paths)),
        Value::Object(fields) => fields.values().for_each(|item| mentioned(item, paths)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}
