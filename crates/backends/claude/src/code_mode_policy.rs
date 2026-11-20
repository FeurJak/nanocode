use std::sync::{Arc, LazyLock};

use nanocode_tool_host::{CodeModeToolPolicy, WorkspaceToolRouting};
use serde_json::Value;

use crate::tools::bash::{DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS};

pub const HAND_TOOLS: [&str; 6] = ["Bash", "Edit", "Read", "TaskOutput", "TaskStop", "Write"];

static POLICY: LazyLock<Arc<dyn CodeModeToolPolicy>> = LazyLock::new(|| Arc::new(ClaudeToolPolicy));

pub(crate) fn claude_tool_policy() -> Arc<dyn CodeModeToolPolicy> {
    Arc::clone(&POLICY)
}

struct ClaudeToolPolicy;

impl CodeModeToolPolicy for ClaudeToolPolicy {
    fn workspace_routing(&self, name: &str) -> Option<WorkspaceToolRouting> {
        if name == "Bash" {
            return Some(WorkspaceToolRouting::Shell);
        }
        HAND_TOOLS
            .contains(&name)
            .then_some(WorkspaceToolRouting::File)
    }

    fn exclusive(&self, name: &str) -> bool {
        ["Write", "Edit"].contains(&name)
    }

    fn hidden_host_tool(&self, name: &str) -> bool {
        nanocode_process_contracts::JOB_TOOLS.contains(&name)
    }

    fn returns_within(&self, name: &str, input: &Value) -> Option<std::time::Duration> {
        if name != "Bash" || input.get("run_in_background").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        let timeout = match input.get("timeout") {
            None => DEFAULT_TIMEOUT_MS,
            Some(timeout) => timeout.as_u64()?.min(MAX_TIMEOUT_MS),
        };
        Some(std::time::Duration::from_millis(timeout))
    }
}
