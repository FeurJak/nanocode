# nanocode as an interactive agent

How the `nanocode` TUI reports to a terminal multiplexer, which MCP servers it starts, and what it records per model call. The last section lists what it does not do yet, measured against OpenCode, so a workspace can tell whether nanocode fits it.

## Status for Herdr (OSC 7501)

Herdr treats a terminal whose program writes OSC 7501 as an agent and shows its state. nanocode writes one record each time its state changes:

```
ESC ] 7501 ; state=<idle|working|done|blocked|error>[:kind=<permission|question|auth>]:app=nanocode:msg=<base64> ESC \
```

- **TUI states:** `working` while a turn runs, `done` or `error` when it ends, and `idle` when it waits for input. It reports `blocked` with `kind=permission` while a pre-tool hook's question waits for an answer, and `kind=auth` while a `/login` waits for the browser.
- **Message and title:** `msg` is the first line of the session's first prompt, which nanocode also sets as the terminal title (OSC 2). Before the first prompt, `msg` is the session id. `msg` holds at most 2,048 bytes after decoding, the most Herdr accepts. A longer line keeps its longest prefix that fits and ends with `…`, and control characters become spaces.
- **`nanocode exec`:** it reports `working` with the prompt, then `done` with the session id, or `error`.
- **When it reports:**
  - **On** in Herdr's terminals (`HC_TERMINAL_ID` or `HC_TERMINAL_SOCKET` set), or with `"herdr": true` in `config.json`.
  - **`HERDR_ENV=1`** or **`HERDR_ENV=0`** overrides both.
  - **Never to a pipe:** exec writes nothing when neither stdout nor stderr is a terminal.

## config.json

nanocode reads two files at start, if they exist:

1. `<home>/config.json`, where home is `~/.nanocode` unless `--home` or `NANOCODE_HOME` says otherwise.
2. `<workspace>/.nanocode/config.json`.

A key in the project file wins over the user file. Unknown keys stop nanocode at start, with the file named.

```json
{
  "mcp": {
    "timeout": { "startup": 30000 },
    "servers": {
      "files": {
        "type": "local",
        "command": ["npx", "-y", "@modelcontextprotocol/server-filesystem", "."],
        "cwd": ".",
        "environment": { "LOG_LEVEL": "info" }
      },
      "docs": {
        "type": "remote",
        "url": "https://mcp.example.com/mcp",
        "oauth": false,
        "headers": { "Authorization": "Bearer {env:DOCS_TOKEN}" }
      },
      "old": { "type": "local", "command": ["old-server"], "disabled": true }
    }
  },
  "hooks": {
    "pre_tool": [
      { "matcher": "Bash|Edit|Write", "command": ["./scripts/guard"], "timeout": 15000 }
    ]
  },
  "herdr": true,
  "forge_usage": false
}
```

### MCP servers

- **Format:** `mcp` takes OpenCode's V2 `mcp` block, so a block can be copied either way.
  - `local` runs `command` (the executable, then its arguments) over stdio, in `cwd` (relative to the workspace, which is also the default) with `environment` added.
  - `remote` speaks Streamable HTTP to `url` with `headers`.
- **Options:**
  - `disabled: true` keeps a server listed but not started.
  - `codemode: false` puts its tools on the model's direct tool list instead of in Code Mode.
  - `timeout.startup` (connection, initialization and tool listing) and `timeout.execution` (each tool call) are in milliseconds. A server's own value overrides the default under `mcp.timeout`.
  - `{env:NAME}` is replaced by that environment variable in commands, environment values, URLs and headers. An unset variable stops start.
- **Tool names:** a server's tools are named `mcp__<server>__<tool>`.
- **`/mcp`:** lists every configured server with its transport and file, then its state (ready with its tool names, failed with the reason, disabled), and then any servers nanocode adds itself.

### Pre-tool hook

Each entry of `hooks.pre_tool` is a command that sees a tool call before it runs. It speaks the contract of Claude Code's `PreToolUse` hook, so a guard written for Claude Code works unchanged.

- **Which calls:** `matcher` lists tool names separated by `|`; a name may end with `*`, and an empty matcher means every tool. Calls made from inside a Code Mode cell match by their own tool names, such as `Bash` and `Edit`.
- **What it gets:** the command runs in the agent's working directory with `CLAUDE_PROJECT_DIR` set to it. Its stdin is one JSON object with `session_id`, `cwd`, `permission_mode`, `hook_event_name` (`PreToolUse`), `tool_name`, `tool_input` and `tool_use_id`.
- **How it decides:**
  - **Allow:** exit 0.
  - **Refuse:** exit 2; stderr is the reason the model gets.
  - **By JSON reply:** exit 0 and print `{"hookSpecificOutput": {"permissionDecision": "allow" | "deny" | "ask", "permissionDecisionReason": "…"}}`. The older `{"decision": "block", "reason": "…"}` and `{"continue": false}` refuse too.
- **Asking:** the TUI shows the call and the reason, and Enter or Esc answers. A refusal tells the model the user refused. `nanocode exec` has nobody to ask, so it refuses with the reason.
- **Several hooks:** every matching entry runs, in order. The first refusal wins, then any ask.
- **When a hook fails:** it may time out (`timeout` in milliseconds; 60 seconds by default, as in Claude Code), exit with another code, or fail to start. The call then runs, as in Claude Code, and the failure is shown in the transcript (on stderr in exec).

### Your own commands

A file `<name>.md` in `<workspace>/.nanocode/commands/` or `<home>/commands/` is the command `/<name>`. The project's file wins over the home one.

- **Format:** the same as Claude Code's and OpenCode's command files, so those files can be copied or linked. Optional front matter between `---` lines gives a `description`, which `/commands` lists; other front matter keys are ignored.
- **Arguments:** `/<name> some words` sends the body as the prompt, with `$ARGUMENTS` replaced by the words. A body without `$ARGUMENTS` gets them appended as `ARGUMENTS: …`. Other `$NAME`s, such as positional `$1`, are left for the model, because command bodies often hold shell.
- **Where they run:** in the TUI, and in `nanocode exec "/<name> words"`. A built-in command with the same name wins.
- **In usage records:** a command's turn, and every subagent call under it, records `command` (`/<name>`), `command_source` and `args`, until the next prompt starts a turn.

### Usage records

- **What is written:** one JSON line per completed model call, root agent and subagents alike, appended to `~/.forge/telemetry/usage-YYYY-MM.jsonl` (or under `FORGE_USAGE_DIR`) in that ledger's schema 1:
  - `session`, `command_session` (the root session), and `parent` and `agent` (a subagent's spawning session and role).
  - `command`, `command_source` and `args` while one of your commands runs; `null` otherwise.
  - `provider`, `model`, `cwd` and `message` (the response id).
  - `tokens`: `in`, `out`, `reasoning`, `cache_read`, `cache_write`; plus `cache_write_1h` when the call wrote the 1-hour cache.
  - `cost` in USD when nanocode prices the call, else `null`; plus `ms` and `finish`.
- **When:** on when `~/.forge` exists; `"forge_usage"` turns it on or off.
- **Reading it:** `forge usage` reads these rows like any other harness's.

## Not there yet, against OpenCode

- **Approval only through a hook:** without a pre-tool hook, the CLI runs every tool call without asking; there are no built-in permission rules. There is no post-tool hook.
- **No question tool:** the model cannot stop to ask the user, so `kind=question` is never reported.
- **Commands are plain prompts:** a command file is not shown in the `/` menu; `/commands` lists them. Its `model` and `agent` front matter is ignored, and it runs no shell or file includes.
- **No plugins:** there is no plugin or event-hook API.
- **MCP gaps:**
  - It does not run an OAuth sign-in from config (`oauth` must be `false`, with the credential in `headers`).
  - It speaks only the classic `initialize` handshake (`protocol: legacy`) and does not share connections (`pool`).
  - It lists tools within the startup timeout, so there is no `timeout.catalog`.
  - It shows no MCP prompts as commands and no input forms for elicitation; it declines them.
  - It reads the config only at start; OpenCode reloads it on save.
- **Usage records name only your commands:** outside one of them, `command` is `null`. `artifact`, `domain`, `repo` and `branch` are always `null`. `provider` is inferred from the model and the Bedrock switch.
- **No language servers or formatters:** nanocode has no LSP diagnostics and does not run formatters after edits.
- **No shared server:** each `nanocode` process owns its sessions. There is no background server that several clients attach to.
