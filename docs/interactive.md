# nanocode as an interactive agent

How the `nanocode` TUI reports to a terminal multiplexer, which MCP servers it starts, and what it records per model call. The last section lists what it does not do yet, measured against OpenCode, so a workspace can tell whether nanocode fits it.

## Status for Herdr (OSC 7501)

Herdr treats a terminal whose program writes OSC 7501 as an agent and shows its state. nanocode writes one record each time its state changes:

```
ESC ] 7501 ; state=<idle|working|done|blocked|error>[:kind=<permission|question|auth>]:app=nanocode:msg=<base64> ESC \
```

- **TUI states:** `working` while a turn runs, `done` or `error` when it ends, and `idle` when it waits for input. It reports `blocked` with `kind=permission` while a pre-tool hook's question waits for an answer, `kind=question` while the model's question waits, and `kind=auth` while a `/login` waits for the browser.
- **Message and title:** `msg` is the first line of the session's first prompt, which nanocode also sets as the terminal title (OSC 2). Before the first prompt, `msg` is the session id. `msg` holds at most 2,048 bytes after decoding, the most Herdr accepts. A longer line keeps its longest prefix that fits and ends with `…`, and control characters become spaces.
- **`nanocode exec`:** it reports `working` with the prompt, then `done` with the session id, or `error`.
- **When it reports:**
  - **On** in Herdr's terminals (`HC_TERMINAL_ID` or `HC_TERMINAL_SOCKET` set), or with `"herdr": true` in `config.json`.
  - **`HERDR_ENV=1`** or **`HERDR_ENV=0`** overrides both.
  - **Never to a pipe:** exec writes nothing when neither stdout nor stderr is a terminal.

## Questions from the model

In the TUI the model has Claude Code's `AskUserQuestion` tool, with the definition Claude Code 2.1.325 sends when a host can answer. The definition is pinned in `crates/backends/claude/tools/src/tools/upstream.json`.

- **Who gets it:** every harness's agents, its subagents included.
- **Asking:** each question opens a picker of its options. "Other" takes a typed answer from the prompt, a multi-select question marks options until "Done", and Esc declines.
- **What the model reads back:** the same text Claude Code returns, whether the answers were options, typed, or declined.
- **Not in exec:** `nanocode exec` does not offer the tool, as `claude -p` does not, because nobody can answer.

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
    ],
    "post_tool": [
      { "matcher": "Edit|Write", "command": ["./scripts/review"] }
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
  - `codemode: false` is refused at start: nanocode reaches MCP tools through Code Mode and tool search only.
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

### Post-tool hook

Each entry of `hooks.post_tool` runs after a matching call succeeded, with Claude Code's `PostToolUse` contract. The matcher, directory and environment work as for the pre-tool hook. Its stdin adds `tool_response`, the tool's result.

- **To the model:** what a hook says is added to the tool's result, with Claude Code's wording:
  - Exit 2 adds `PostToolUse hook blocking error from command: "<command>": <stderr>`.
  - A JSON reply with `{"decision": "block", "reason": "…"}` adds the reason the same way.
  - `{"hookSpecificOutput": {"additionalContext": "…"}}` adds `PostToolUse hook additional context: …`.
- **To you:** stderr from a hook that exits 0 is shown in the transcript, and so is a hook that fails or times out.
- **What it can't do:** the call has already run, so nothing is undone. Failed calls don't run the hook.

### Your own commands

A file `<name>.md` in `<workspace>/.nanocode/commands/` or `<home>/commands/` is the command `/<name>`. The project's file wins over the home one, and a built-in command keeps its name.

- **Format:** OpenCode's and Claude Code's command files. Optional front matter between `---` lines can give:
  - `description`, which `/commands` lists;
  - `model` (`provider/model#variant` or a nanocode model id; `claude-sonnet-5.5` and `claude-sonnet-5-5` both work), `agent` and `subagent` (or the older `subtask`).
- **Arguments:** as in OpenCode.
  - `$ARGUMENTS` is the whole argument string.
  - `$1`, `$2` … are words, grouped by single or double quotes, and the highest-numbered one takes the rest. Missing positions are empty.
  - A body with no placeholder gets the arguments after a blank line.
- **Shell blocks:** each `` !`command` `` runs in the workspace with `$SHELL` after the arguments are filled in. Its output, stdout then stderr, replaces it before the prompt is sent. Only use command files you trust.
- **Where it runs:**
  - In a subagent when `subagent: true`, or when `subagent` is left out and the file names an `agent` or a model other than the thread's. The subagent takes `agent` as its role (`general` when none) and the given model and thinking. The thread is told which subagent took the command, and the result comes back to it when the child finishes.
  - Otherwise in the current thread, on the thread's model: nanocode fixes a thread's model.
  - `nanocode exec "/<name> words"` always runs it in its own session.
- **In usage records:** a command's turn, and every call of a subagent it started, records `command` (`/<name>`), `command_source` and `args`. A command run in the thread keeps that until the next prompt starts a turn.

### Usage records

- **What is written:** one JSON line per completed model call, root agent and subagents alike, appended to `~/.forge/telemetry/usage-YYYY-MM.jsonl` (or under `FORGE_USAGE_DIR`) in that ledger's schema 1:
  - `session`, `command_session` (the root session), and `parent` and `agent` (a subagent's spawning session and role).
  - `command`, `command_source` and `args` while one of your commands runs; `null` otherwise.
  - `provider`, `model`, `cwd` and `message` (the response id).
  - `tokens`: `in`, `out`, `reasoning`, `cache_read`, `cache_write`; plus `cache_write_1h` when the call wrote the 1-hour cache.
  - `cost` in USD when nanocode prices the call, else `null`; plus `ms` and `finish`.
- **When:** on when `~/.forge` exists; `"forge_usage"` turns it on or off.
- **Reading it:** `forge usage` reads these rows like any other harness's.

## Switching from OpenCode

`scripts/from-opencode.py` copies an OpenCode setup into nanocode's files and never changes OpenCode's:

```sh
python3 scripts/from-opencode.py --global --link --claude-hooks ~/.claude/settings.json
python3 scripts/from-opencode.py --project ~/src/app --link
```

- **What `--global` reads and writes:** `~/.config/opencode` into nanocode's home.
- **What `--project DIR` reads and writes:** `DIR/opencode.json(c)` and `DIR/.opencode/` into `DIR/.nanocode/` and `DIR/.agents/skills/`.
- **MCP servers:** the V2 block, or V1's servers with `enabled` turned into `disabled`, are merged into `config.json`. Servers that need what nanocode lacks are left out (an OAuth object), or their fields are dropped (`pool`, a non-legacy `protocol`, `codemode: false`, `timeout.catalog`), and the script says which.
- **Commands:** command files (nested names become `a:b`) and JSON `commands` become command files. They are copied, or linked with `--link`. A file that differs is kept unless `--force`.
- **Skills:** skill folders are linked.
- **Hooks:** `--claude-hooks` takes the `PreToolUse` and `PostToolUse` command hooks of a Claude Code settings file.
- **Warnings:** what nanocode will not honor is printed: `agent`, `model` or `subagent` front matter, `!` shell blocks, `$1` placeholders, and remote servers that relied on OAuth by default.
- **Trying it first:** `--dry-run` prints the plan and writes nothing; running it again changes only what changed.

## Pilot: one Herdr space on nanocode for an hour

1. **Install** nanocode and run the two commands above for the space's user and its project. Pass `--claude-hooks` the settings snippet your workflow ships for Claude Code, so its pre-edit and post-edit checks come along.
2. **Copy the environment:** export, in the space's shell, every variable your commands and hooks expect that OpenCode used to set. Commands that call `"$TOOL/scripts/…"` fail with exit 127 when it is unset.
3. **Check before you start:** in `nc` in the project, see that `/mcp` lists every server ready, `/commands` lists the commands, and `/skills` lists the skills.
4. **Start it in a space:** `hc run --keep --label herdr.space=<id> --label herdr.tab=<id> -- nc -m <model>`. Herdr should show it as an agent with app `nanocode`.
5. **After an hour:**
   - **Herdr:** the space went working, done and idle, and showed blocked whenever a hook asked or the model asked a question.
   - **The transcript:** hook failures show as notices. Read them: a failed hook lets the call through.
   - **Usage records:** `forge usage --harness nanocode --since 1h --by command,agent` should cover the hour's work, with your commands named. Compare its cost with an OpenCode space doing similar work.
   - **Warnings:** each run's `~/.nanocode/log/otel/<run>/traces.jsonl` holds them as span events with level `WARN`, such as a usage record that was not written: `grep -h '"WARN"' ~/.nanocode/log/otel/*/traces.jsonl`.
   - **`/mcp`:** no server should have failed since start.
6. **To go back:** stop the space and start OpenCode there again. The script wrote only nanocode's files.

## Not there yet, against OpenCode

- **Approval only through a hook:** without a pre-tool hook, the CLI runs every tool call without asking; there are no built-in permission rules. Hooks see tool calls only, with no session, prompt or stop events.
- **Questions are options or text:** a question can't show option previews or take notes on an answer, and Codex models get Claude Code's tool rather than Codex's own `request_user_input`.
- **Commands:** a command file is not shown in the `/` menu; `/commands` lists them. `agent` names a subagent role, not a configured agent with its own prompt and tools.
- **Skills are pointers:** `/skills` inserts the skill's name and its `SKILL.md` path into the prompt; the model is not told which skills exist unless you pick one.
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
