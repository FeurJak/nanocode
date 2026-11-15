use std::sync::{Arc, OnceLock};

use nanocode_types_core::responses::{JsonSchema, StaticSchema, compact_json, compact_json_length};
use serde_json::{Value, json, value::RawValue};

pub const TOOL_DEFINITIONS: &str = include_str!("tools/upstream.json");

const LENGTH: usize = compact_json_length(TOOL_DEFINITIONS);
static COMPACT: [u8; LENGTH] = compact_json::<LENGTH>(TOOL_DEFINITIONS);

pub struct Tool {
    pub description: Arc<str>,
    pub name: &'static str,
    schema: StaticSchema,
}

impl Tool {
    #[must_use]
    pub const fn input_schema(&'static self) -> JsonSchema {
        JsonSchema::from_static(&self.schema)
    }
}

#[derive(serde::Deserialize)]
struct Entry {
    description: String,
    #[serde(borrow)]
    input_schema: &'static RawValue,
    name: &'static str,
}

pub fn tools() -> &'static [Tool] {
    static TOOLS: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        let json = core::str::from_utf8(&COMPACT).expect("compact upstream Claude tool contracts");
        serde_json::from_str::<Vec<Entry>>(json)
            .expect("verified upstream Claude tool contracts")
            .into_iter()
            .map(|entry| Tool {
                description: entry.description.into(),
                name: entry.name,
                schema: StaticSchema::new(entry.input_schema.get().as_bytes()),
            })
            .collect()
    })
}

pub fn tool(name: &str) -> &'static Tool {
    tools()
        .iter()
        .find(|tool| tool.name == name)
        .expect("verified upstream Claude tool name")
}

pub fn tool_value(name: &str) -> Value {
    let tool = tool(name);
    json!({"name": tool.name, "description": &*tool.description, "input_schema": tool.input_schema().as_value()})
}
