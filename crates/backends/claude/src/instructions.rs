const CODE_MODE: &str = include_str!("../prompts/code_mode.md");

const OPUS_5_5_PARAGRAPH: &str = "\n\nDo build original games,";
const SURFACES: &str = " - Claude Code is available as a CLI in the terminal, desktop app (Mac/Windows), web app (claude.ai/code), and IDE extensions (VS Code, JetBrains).";
const FAST_MODE: &str = "\n - Fast mode for Claude Code uses Claude Opus with faster output (it does not downgrade to a smaller model). It can be toggled with /fast.";

pub(crate) fn default_system_blocks(
    code_only: bool,
    model: &str,
    upstream_context: bool,
) -> std::sync::Arc<Vec<serde_json::Value>> {
    let mut blocks = crate::upstream::system_blocks();
    if let Some(serde_json::Value::String(text)) =
        blocks.last_mut().and_then(|block| block.get_mut("text"))
    {
        if !crate::environment::opus_5_5_prompt(model)
            && let Some(start) = text.find(OPUS_5_5_PARAGRAPH)
        {
            let end = text[start + 2..]
                .find("\n\n")
                .map_or(text.len(), |end| start + 2 + end);
            text.replace_range(start..end, "");
        }
        if upstream_context && let Some(at) = text.find(SURFACES) {
            text.insert_str(at + SURFACES.len(), FAST_MODE);
        }
        if crate::environment::total_tokens_reminder(model) {
            text.push_str(&format!(
                "\n\n<total_tokens>{} tokens left</total_tokens>",
                crate::environment::TOKEN_BUDGET
            ));
        }
    }
    if code_only {
        blocks.push(serde_json::json!({"type": "text", "text": CODE_MODE}));
    }
    std::sync::Arc::new(blocks)
}
