use crate::{ContentBlock, ToolResultContent};

use super::history::HistoryMessage;

const CLEARED: &str = "[Old tool result content cleared]";
const CLEAR_AT_LEAST_TOKENS: u64 = 20_000;
const KEEP_TOOL_RESULTS: usize = 12;
const MIN_CLEARABLE_CHARS: usize = 64;

pub(super) fn result_tokens(block: &ContentBlock) -> Option<u64> {
    let ContentBlock::ToolResult { content, .. } = block else {
        return None;
    };
    let (chars, tokens) = match content {
        ToolResultContent::Text(text) => (text.chars().count(), super::estimate_text_tokens(text)),
        ToolResultContent::Blocks(blocks) => {
            if blocks.iter().any(|block| block["type"] != "text") {
                return None;
            }
            blocks.iter().fold((0, 0), |(chars, tokens), block| {
                let text = block["text"].as_str().unwrap_or_default();
                (
                    chars + text.chars().count(),
                    tokens + super::estimate_text_tokens(text),
                )
            })
        }
    };
    (chars >= MIN_CLEARABLE_CHARS).then_some(tokens)
}

pub(super) fn clear_tool_results(messages: &mut [HistoryMessage]) {
    let results: Vec<(usize, usize, u64)> = messages
        .iter()
        .enumerate()
        .flat_map(|(message, entry)| {
            entry
                .result_tokens(result_tokens)
                .iter()
                .map(move |&(block, tokens)| (message, block, tokens))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut cleared = 0;
    let mut pending = 0;
    for seen in KEEP_TOOL_RESULTS + 1..=results.len() {
        pending += results[seen - KEEP_TOOL_RESULTS - 1].2;
        if pending >= CLEAR_AT_LEAST_TOKENS {
            cleared = seen - KEEP_TOOL_RESULTS;
            pending = 0;
        }
    }
    let mut at = 0;
    while at < cleared {
        let message = results[at].0;
        let content = &mut messages[message].make_mut().content;
        while at < cleared && results[at].0 == message {
            if let ContentBlock::ToolResult { content, .. } = &mut content[results[at].1] {
                *content = ToolResultContent::Text(CLEARED.to_owned());
            }
            at += 1;
        }
    }
}
