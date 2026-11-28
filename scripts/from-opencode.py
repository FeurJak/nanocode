import argparse
import json
import os
import re
import sys
from pathlib import Path

LOCAL_KEYS = {"type", "command", "cwd", "environment", "disabled", "codemode", "timeout", "protocol", "pool"}
REMOTE_KEYS = {"type", "url", "headers", "oauth", "disabled", "codemode", "timeout", "protocol", "pool"}
TIMEOUTS = {"startup", "execution"}
UNSUPPORTED_FRONT = {"agent", "model", "subagent", "subtask"}


def jsonc(text):
    out, index, string = [], 0, False
    while index < len(text):
        char = text[index]
        if string:
            out.append(char)
            if char == "\\":
                out.append(text[index + 1])
                index += 1
            elif char == '"':
                string = False
        elif char == '"':
            string = True
            out.append(char)
        elif text.startswith("//", index):
            index = text.find("\n", index)
            index = len(text) if index < 0 else index
            continue
        elif text.startswith("/*", index):
            index = text.index("*/", index) + 2
            continue
        else:
            out.append(char)
        index += 1
    return json.loads(re.sub(r",(\s*[}\]])", r"\1", "".join(out)))


def read_configs(paths, notes):
    merged = {"servers": {}, "timeout": {}, "commands": {}}
    for path in paths:
        if not path.is_file():
            continue
        data = jsonc(path.read_text())
        notes.append(f"read {path}")
        mcp = data.get("mcp") or {}
        if "servers" in mcp or "timeout" in mcp:
            servers = mcp.get("servers") or {}
            merged["timeout"].update(mcp.get("timeout") or {})
        else:
            servers = {name: v1(entry) for name, entry in mcp.items()}
        merged["servers"].update(servers)
        for key in ("commands", "command"):
            merged["commands"].update(data.get(key) or {})
    return merged


def v1(entry):
    entry = dict(entry)
    if "enabled" in entry:
        entry["disabled"] = not entry.pop("enabled")
    if isinstance(entry.get("timeout"), (int, float)):
        entry["timeout"] = {"startup": int(entry["timeout"])}
    return entry


def server(name, entry, notes):
    kind = entry.get("type")
    allowed = {"local": LOCAL_KEYS, "remote": REMOTE_KEYS}.get(kind)
    if allowed is None:
        notes.append(f"skipped MCP server {name}: type {kind!r} is neither local nor remote")
        return None
    out = {}
    if kind == "remote" and "oauth" not in entry:
        notes.append(f"MCP server {name}: OpenCode signs in to it with OAuth by default; nanocode connects without signing in, so give it oauth: false and a credential in headers if it needs one")
    for key, value in entry.items():
        if key not in allowed:
            notes.append(f"MCP server {name}: dropped {key!r}, which nanocode does not read")
        elif key == "oauth" and value is not False:
            notes.append(f"skipped MCP server {name}: nanocode runs no OAuth sign-in from config; set oauth to false and pass the credential in headers")
            return None
        elif key == "protocol" and value != "legacy":
            notes.append(f"MCP server {name}: dropped protocol {value!r}; nanocode speaks the classic handshake")
        elif key == "codemode" and value is False:
            notes.append(f"MCP server {name}: dropped codemode: false; nanocode reaches MCP tools through Code Mode")
        elif key == "pool" and value:
            notes.append(f"MCP server {name}: dropped pool; nanocode does not share connections")
        elif key == "timeout":
            kept = {k: v for k, v in value.items() if k in TIMEOUTS}
            for dropped in sorted(set(value) - TIMEOUTS):
                notes.append(f"MCP server {name}: dropped timeout.{dropped}; nanocode lists tools within timeout.startup")
            if kept:
                out[key] = kept
        else:
            out[key] = value
    return out


def command_files(directories):
    found = {}
    for directory in directories:
        for path in sorted(directory.rglob("*.md")) if directory.is_dir() else []:
            name = path.relative_to(directory).with_suffix("").as_posix()
            found[name] = path
    return found


def front_matter(text):
    if not text.startswith("---\n") or "\n---\n" not in text[4:]:
        return {}, text
    front, body = text[4:].split("\n---\n", 1)
    keys = {line.split(":", 1)[0].strip(): line.split(":", 1)[1].strip()
            for line in front.splitlines() if ":" in line}
    return keys, body


def check_command(name, text, notes):
    keys, body = front_matter(text)
    for key in sorted(UNSUPPORTED_FRONT & set(keys)):
        notes.append(f"command /{name}: nanocode ignores {key}: {keys[key]}")
    if "!`" in body:
        notes.append(f"command /{name}: its !`shell` blocks reach the model as text; nanocode does not run them")
    if re.search(r"\$[1-9]", body):
        notes.append(f"command /{name}: $1, $2 ... are not filled; only $ARGUMENTS is")


def main():
    parser = argparse.ArgumentParser(description="Translate an OpenCode config's MCP servers, commands and skills into nanocode's.")
    where = parser.add_mutually_exclusive_group(required=True)
    where.add_argument("--global", dest="scope", action="store_const", const="global",
                       help="read ~/.config/opencode, write <nanocode home>")
    where.add_argument("--project", metavar="DIR", help="read DIR's opencode.json(c) and .opencode/, write DIR/.nanocode/")
    parser.add_argument("--home", help="nanocode home (default: $NANOCODE_HOME or ~/.nanocode)")
    parser.add_argument("--link", action="store_true", help="symlink command files instead of copying them")
    parser.add_argument("--force", action="store_true", help="replace command files that differ")
    parser.add_argument("--claude-hooks", metavar="FILE", help="also take PreToolUse and PostToolUse command hooks from a Claude Code settings file")
    parser.add_argument("--dry-run", action="store_true", help="print what would change, write nothing")
    args = parser.parse_args()
    home = Path(args.home or os.environ.get("NANOCODE_HOME") or Path.home() / ".nanocode").expanduser()
    if args.project:
        root = Path(args.project).expanduser().resolve()
        configs = [root / name for name in ("opencode.json", "opencode.jsonc")] + \
                  [root / ".opencode" / name for name in ("opencode.json", "opencode.jsonc")]
        directories = [root / ".opencode" / "command", root / ".opencode" / "commands"]
        target = root / ".nanocode"
        skill_sources, skill_target = [root / ".opencode" / "skills"], root / ".agents" / "skills"
    else:
        base = Path(os.environ.get("XDG_CONFIG_HOME") or Path.home() / ".config") / "opencode"
        configs = [base / "config.json", base / "opencode.json", base / "opencode.jsonc"]
        directories = [base / "command", base / "commands"]
        target = home
        skill_sources, skill_target = [base / "skills"], home / "skills"
    notes = []
    source = read_configs(configs, notes)
    config_path = target / "config.json"
    config = json.loads(config_path.read_text()) if config_path.is_file() else {}
    mcp = config.setdefault("mcp", {})
    servers = mcp.setdefault("servers", {})
    for name, entry in source["servers"].items():
        translated = server(name, entry, notes)
        if translated is None:
            continue
        if name in servers and servers[name] != translated:
            notes.append(f"MCP server {name}: replaced the entry already in {config_path}")
        servers[name] = translated
    for dropped in sorted(set(source["timeout"]) - TIMEOUTS):
        notes.append(f"dropped mcp.timeout.{dropped}; nanocode lists tools within timeout.startup")
    timeout = {k: v for k, v in source["timeout"].items() if k in TIMEOUTS}
    if timeout:
        mcp["timeout"] = {**mcp.get("timeout", {}), **timeout}
    if not servers:
        config.pop("mcp", None)
    commands = target / "commands"
    planned = []
    for name, path in command_files(directories).items():
        flat = name.replace("/", ":")
        if flat != name:
            notes.append(f"command /{name}: nested names become /{flat}")
        check_command(flat, path.read_text(), notes)
        planned.append((flat, path, None))
    for name, entry in source["commands"].items():
        template = entry.get("template")
        if not template:
            notes.append(f"skipped command /{name}: it has no template")
            continue
        description = entry.get("description")
        text = (f"---\ndescription: {description}\n---\n" if description else "") + template.strip() + "\n"
        check_command(name, text, notes)
        planned.append((name, None, text))
    for name, path, text in planned:
        destination = commands / f"{name}.md"
        wanted = text if text is not None else path.read_text()
        if destination.exists() or destination.is_symlink():
            same = destination.resolve() == path.resolve() if path and args.link else destination.read_text() == wanted
            if same:
                continue
            if not args.force:
                notes.append(f"kept {destination}: it differs from {path or 'the config entry'}; --force replaces it")
                continue
        notes.append(f"{'would write' if args.dry_run else 'wrote'} {destination}")
        if args.dry_run:
            continue
        commands.mkdir(parents=True, exist_ok=True)
        if destination.exists() or destination.is_symlink():
            destination.unlink()
        if args.link and path:
            destination.symlink_to(path.resolve())
        else:
            destination.write_text(wanted)
    for directory in skill_sources:
        for skill in sorted(directory.iterdir()) if directory.is_dir() else []:
            if not (skill / "SKILL.md").is_file():
                continue
            destination = skill_target / skill.name
            if destination.exists() or destination.is_symlink():
                if destination.resolve() != skill.resolve():
                    notes.append(f"kept {destination}: it is not a link to {skill}")
                continue
            notes.append(f"{'would link' if args.dry_run else 'linked'} skill {destination} -> {skill}")
            if not args.dry_run:
                skill_target.mkdir(parents=True, exist_ok=True)
                destination.symlink_to(skill.resolve())
    if args.claude_hooks:
        claude = jsonc(Path(args.claude_hooks).expanduser().read_text()).get("hooks") or {}
        hooks = config.setdefault("hooks", {})
        for event, key in (("PreToolUse", "pre_tool"), ("PostToolUse", "post_tool")):
            entries = []
            for group in claude.get(event) or []:
                for hook in group.get("hooks") or []:
                    if hook.get("type") != "command":
                        notes.append(f"{event}: skipped a {hook.get('type')!r} hook; nanocode runs command hooks")
                        continue
                    entry = {"matcher": group.get("matcher", ""), "command": ["sh", "-c", hook["command"]]}
                    if "timeout" in hook:
                        entry["timeout"] = int(hook["timeout"] * 1000)
                    entries.append(entry)
            if entries:
                hooks[key] = entries
                notes.append(f"{key}: {len(entries)} hooks from {args.claude_hooks}")
        for event in sorted(set(claude) - {"PreToolUse", "PostToolUse"}):
            notes.append(f"{event}: skipped; nanocode runs only pre-tool and post-tool hooks")
        if not hooks:
            config.pop("hooks")
    rendered = json.dumps(config, indent=2) + "\n"
    if (config or config_path.is_file()) and (not config_path.is_file() or config_path.read_text() != rendered):
        notes.append(f"{'would write' if args.dry_run else 'wrote'} {config_path} ({len(servers)} MCP servers)")
        if not args.dry_run:
            target.mkdir(parents=True, exist_ok=True)
            config_path.write_text(rendered)
    print("\n".join(notes) if notes else "nothing to translate")


if __name__ == "__main__":
    sys.exit(main())
