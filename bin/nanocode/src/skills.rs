use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use nanocode::tools::{
    Tool, ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult, contract::ToolCallFuture,
    runtime::DynamicToolProvider,
};
use serde::Deserialize;

pub const NAME: &str = "Skill";
const CLAUDE_HEADING: &str = "The following skills are available for use with the Skill tool:";
const CODEX_INTRO: &str = "A skill is a set of local instructions to follow that is stored in a `SKILL.md` file. Below is the list of skills that can be used. Each entry includes a name, description, and a short path that can be expanded into an absolute path using the skill roots table.";

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub skill_md: PathBuf,
    pub root: PathBuf,
}

fn front_matter(text: &str) -> (&str, &str) {
    text.strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .unwrap_or(("", text))
}

fn description(text: &str) -> Option<String> {
    let (front, _) = front_matter(text);
    let mut lines = front.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(value) = line.strip_prefix("description:") else {
            continue;
        };
        let value = value.trim();
        if !matches!(value, "|" | ">" | "|-" | ">-" | "") {
            return Some(value.trim_matches('"').trim_matches('\'').to_owned());
        }
        let mut folded = Vec::new();
        while let Some(next) = lines.peek() {
            if !next.starts_with(' ') && !next.starts_with('\t') {
                break;
            }
            folded.push(next.trim().to_owned());
            lines.next();
        }
        return Some(folded.join(" ")).filter(|text| !text.is_empty());
    }
    None
}

pub fn discover(nanocode_home: &std::path::Path, workspace: &std::path::Path) -> Vec<Skill> {
    let Ok(home) = nanocode_home::AgentHome::from_env() else {
        return Vec::new();
    };
    let home = nanocode_home::AgentHome::new(nanocode_home, home.claude_home())
        .with_user_home(home.user_home().map(std::path::Path::to_path_buf));
    home.skills_for(&home.project(workspace))
        .skills
        .into_iter()
        .filter_map(|entry| {
            let text = std::fs::read_to_string(&entry.skill_md).ok()?;
            Some(Skill {
                description: description(&text).unwrap_or_default(),
                name: entry.name,
                dir: entry.dir,
                skill_md: entry.skill_md,
                root: entry.root.path,
            })
        })
        .collect()
}

pub fn claude_listing(skills: &[Skill]) -> String {
    let lines = skills
        .iter()
        .map(|skill| format!("- {}: {}", skill.name, skill.description))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{CLAUDE_HEADING}\n\n{lines}")
}

pub fn codex_catalog(skills: &[Skill]) -> String {
    let mut roots: BTreeMap<PathBuf, String> = BTreeMap::new();
    for skill in skills {
        let next = format!("r{}", roots.len());
        roots.entry(skill.root.clone()).or_insert(next);
    }
    let table = roots
        .iter()
        .map(|(root, alias)| format!("- `{alias}` = `{}`", root.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let entries = skills
        .iter()
        .map(|skill| {
            let alias = &roots[&skill.root];
            let relative = skill.skill_md.strip_prefix(&skill.root).map_or_else(
                |_| skill.skill_md.display().to_string(),
                |path| format!("{alias}/{}", path.display()),
            );
            format!("- {}: {} (file: {relative})", skill.name, skill.description)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "<skills_instructions>\n## Skills\n{CODEX_INTRO}\n### Skill roots\n{table}\n### Available skills\n{entries}\n</skills_instructions>"
    )
}

#[derive(Clone)]
pub struct Skills {
    skills: Arc<[Skill]>,
}

impl Skills {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self {
            skills: skills.into(),
        }
    }
}

#[derive(Deserialize)]
struct Input {
    skill: String,
    #[serde(default)]
    args: Option<String>,
}

struct SkillTool {
    skills: Arc<[Skill]>,
}

impl Tool for SkillTool {
    fn definition(&self) -> ToolDefinition {
        let upstream = nanocode::claude::tool_contracts::tool(NAME);
        ToolDefinition::function(NAME, upstream.description.clone(), upstream.input_schema())
    }

    fn execute<'life0, 'life1, 'async_trait>(
        &'life0 self,
        input: ToolInput,
        _context: ToolContext<'life1>,
    ) -> ToolCallFuture<'async_trait, ToolResult>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let Input { skill, args } = input.decode_json::<Input>()?;
            let name = skill.trim_start_matches('/');
            let Some(found) = self.skills.iter().find(|entry| entry.name == name).cloned() else {
                return Ok(ToolOutput::error(format!("Unknown skill: {name}")));
            };
            let read = nanocode_platform::spawn_blocking(move || {
                std::fs::read_to_string(&found.skill_md).map(|text| (found, text))
            })
            .await;
            let (found, text) = match read {
                Ok(Ok(read)) => read,
                Ok(Err(error)) => return Ok(ToolOutput::error(format!("skill {name}: {error}"))),
                Err(error) => return Ok(ToolOutput::error(format!("skill {name}: {error}"))),
            };
            let (_, body) = front_matter(&text);
            let arguments = args
                .filter(|args| !args.trim().is_empty())
                .map(|args| format!("\n\nARGUMENTS: {args}"))
                .unwrap_or_default();
            Ok(ToolOutput::text(format!(
                "Launching skill: {name}\n\nBase directory for this skill: {}\n\n{}{arguments}",
                found.dir.display(),
                body.trim()
            )))
        })
    }
}

impl DynamicToolProvider for Skills {
    fn direct_tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(SkillTool {
            skills: Arc::clone(&self.skills),
        })]
    }
}
