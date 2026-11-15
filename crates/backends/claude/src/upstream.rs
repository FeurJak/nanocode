use std::sync::OnceLock;

use serde_json::Value;

use crate::ToolDefinition;

pub const VERSION: &str = "2.1.304";
pub const PROVENANCE: &str = include_str!("../prompts/upstream.json");

pub fn system_blocks() -> Vec<Value> {
    static BLOCKS: OnceLock<Vec<Value>> = OnceLock::new();
    BLOCKS
        .get_or_init(|| {
            serde_json::from_str(include_str!("../prompts/claude-code-system.json"))
                .expect("verified upstream Claude system blocks")
        })
        .clone()
}

#[derive(serde::Deserialize)]
pub(crate) struct BashFirst {
    pub(crate) bypass: String,
    pub(crate) dedicated_tools: String,
    pub(crate) models: Vec<BashFirstModel>,
}

#[derive(serde::Deserialize)]
pub(crate) struct BashFirstModel {
    pub(crate) model: String,
    pub(crate) steer: String,
}

pub(crate) fn bash_first() -> &'static BashFirst {
    static TEXT: OnceLock<BashFirst> = OnceLock::new();
    TEXT.get_or_init(|| {
        serde_json::from_str(include_str!("../prompts/bash-first.json"))
            .expect("verified upstream bash-first text")
    })
}

pub fn tool_definitions() -> &'static [ToolDefinition] {
    static TOOLS: OnceLock<Vec<ToolDefinition>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        nanocode_backend_claude_tools::upstream::tools()
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name.into(),
                description: tool.description.clone(),
                input_schema: tool.input_schema(),
                strict: None,
                defer_loading: false,
                cache_control: None,
            })
            .collect()
    })
}

pub fn tool_definition(name: &str) -> Option<&'static ToolDefinition> {
    tool_definitions().iter().find(|tool| tool.name == name)
}
