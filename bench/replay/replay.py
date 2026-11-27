import argparse, fnmatch, json, os, re, shutil, sqlite3, statistics, subprocess, sys, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "../.."))
PRICES = {
    "claude-opus-5-5": {"input": 4, "cache_read": 0.2, "write_5m": 5, "write_1h": 8, "output": 20},
    "claude-sonnet-5-5": {"input": 2, "cache_read": 0.1, "write_5m": 2.5, "write_1h": 4, "output": 10},
}
STUB = r'''#!/bin/sh
printf '%s\n' "$*" >> "$FORGE_REPLAY_LOG"
if [ "$1" = commit ]; then
  shift
  if [ "${FORGE_REPLAY_IDLE:-0}" -gt 0 ]; then
    marker="$(git rev-parse --git-common-dir)/replay-lock"
    if [ ! -e "$marker.seen" ]; then
      touch "$marker.seen"
      echo "forge commit: the commit lock of $(git rev-parse --git-common-dir) is held by a sibling task. Run forge commit again with a Bash timeout of 600000 ms: it waits for the lock (up to 900 s)." >&2
      exit 75
    fi
    if [ ! -e "$marker.waited" ]; then
      touch "$marker.waited"
      sleep "$FORGE_REPLAY_IDLE"
    fi
  fi
  exec git commit "$@"
fi
echo "forge (replay stub): recorded: $*"
'''


def expand(value):
    return os.path.expanduser(os.path.expandvars(value))


def load(path):
    task = json.load(open(path))
    task["work"] = expand(task.get("work") or os.path.join(tempfile.gettempdir(), "nanocode-replay", task["name"]))
    for key in ("repo", "prompt", "agent", "forge_home", "link_from"):
        if task.get(key):
            task[key] = expand(task[key])
    return task


def git(cwd, *args, check=True):
    return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, check=check).stdout.strip()


def forge_stub(task):
    forge = os.path.join(task["work"], "forge")
    os.makedirs(os.path.join(forge, "scripts"), exist_ok=True)
    os.makedirs(os.path.join(task["work"], "vault"), exist_ok=True)
    for part in ("prompts", "agents"):
        source = os.path.join(task["forge_home"], part)
        if os.path.isdir(source) and not os.path.isdir(os.path.join(forge, part)):
            shutil.copytree(source, os.path.join(forge, part))
    script = os.path.join(forge, "scripts", "forge")
    open(script, "w").write(STUB)
    os.chmod(script, 0o755)
    return forge


def context_file(context, name):
    if context.get("commit"):
        return git(expand(context["repo"]), "show", f"{context['commit']}:{context['from']}/{name}") + "\n"
    return open(os.path.join(expand(context["from"]), name)).read()


def source(task, commit):
    path = os.path.join(task["work"], "source")
    if not os.path.isdir(path):
        subprocess.run(["git", "clone", "-q", "--shared", "--no-checkout", task["repo"], path], check=True)
        git(path, "config", "uploadpack.allowFilter", "true")
        git(path, "config", "uploadpack.allowReachableSHA1InWant", "true")
        git(path, "config", "remote.origin.pushurl", "/nonexistent/replay-never-pushes")
        git(path, "config", "remote.origin.fetch", "+refs/replay-none/*:refs/replay-none/*")
        git(path, "symbolic-ref", "HEAD", "refs/heads/replay-none")
        for ref in git(path, "for-each-ref", "--format=%(refname)").splitlines():
            git(path, "update-ref", "-d", ref)
    git(path, "update-ref", f"refs/heads/replay-{commit}", git(task["repo"], "rev-parse", f"{commit}^{{commit}}"))
    return path, f"replay-{commit}"


def slot(task, label, commit=None):
    path = os.path.join(task["work"], f"slot-{label}")
    shutil.rmtree(path, ignore_errors=True)
    origin, ref = source(task, commit or task["base"])
    git(task["work"], "init", "-q", path)
    for key, value in (("url", origin), ("pushurl", "/nonexistent/replay-never-pushes"), ("promisor", "true"),
                       ("partialclonefilter", "blob:none"), ("fetch", "+refs/replay-none/*:refs/replay-none/*")):
        git(path, "config", f"remote.origin.{key}", value)
    git(path, "fetch", "-q", "--filter=blob:none", "origin", ref)
    git(path, "checkout", "-q", "-b", task.get("branch", "replay"), "FETCH_HEAD")
    if ref != f"replay-{task['base']}":
        git(origin, "update-ref", "-d", f"refs/heads/{ref}")
    for link in task.get("links", []):
        os.symlink(os.path.join(task["link_from"], link), os.path.join(path, link))
    context = task.get("forge_context")
    if context:
        current = os.path.join(path, ".forge", "current")
        os.makedirs(os.path.join(current, "tasks"), exist_ok=True)
        for name in context.get("files", []):
            body = context_file(context, name)
            for file, old, new in context.get("edits", []):
                if file == name:
                    if old not in body:
                        sys.exit(f"forge_context edit for {name} does not match: {old}")
                    body = body.replace(old, new)
            open(os.path.join(current, name), "w").write(body)
        if context.get("decisions_lines"):
            lines = context_file(context, "decisions.md").splitlines()
            open(os.path.join(current, "decisions.md"), "w").write("\n".join(lines[:context["decisions_lines"]]) + "\n")
        with open(os.path.join(path, ".git", "info", "exclude"), "a") as exclude:
            exclude.write("/.forge/\n")
    return path


def impl(task):
    path = os.path.join(task["work"], "impl")
    if task.get("impl") and not os.path.isdir(path):
        slot(task, "impl")
        os.rename(os.path.join(task["work"], "slot-impl"), path)
        subprocess.run(["chmod", "-R", "a-w", path], check=True)
    return path


def prompt(task, path):
    text = open(task["prompt"]).read()
    names = {"slot": path, "work": task["work"], "forge": os.path.join(task["work"], "forge"),
             "vault": os.path.join(task["work"], "vault"), "impl": impl(task)}
    for old, new in task.get("rewrite", []):
        text = text.replace(expand(old), new.format(**names))
    if task.get("agent"):
        agent = open(task["agent"]).read()
        if agent.startswith("---"):
            agent = agent.split("---", 2)[2]
        text = agent.strip() + "\n\n---\n\n" + text
    return text


def changed(task, path):
    names = set(git(path, "diff", "--name-only", task["base"]).splitlines())
    names |= set(git(path, "ls-files", "--others", "--exclude-standard").splitlines())
    return sorted(name for name in names if name and not name.startswith(".forge/"))


def run_check(task, path, check, files):
    present = [name for name in files if os.path.exists(os.path.join(path, name))]
    command = check["command"].replace("{changed}", " ".join(present))
    env = dict(os.environ, **{key: expand(value) for key, value in task.get("check_env", {}).items()})
    started = time.time()
    done = subprocess.run(["sh", "-c", command], cwd=path, env=env, capture_output=True, text=True,
                          timeout=check.get("timeout", 3600))
    output = done.stdout + done.stderr
    result = {"exit": done.returncode, "seconds": round(time.time() - started, 1)}
    for value, kind in re.findall(r"(\d+) (passed|failed|errors?)\b", output):
        result[kind.rstrip("s") if kind.startswith("error") else kind] = int(value)
    if check.get("count"):
        pattern = check["count"]
        lines = [line for line in output.splitlines() if re.search(pattern, line)]
        if check.get("only_changed"):
            lines = [line for line in lines if any(name in line for name in present)]
        result["matching_lines"] = len(lines)
    tail = [line for line in output.strip().splitlines() if line.strip()][-2:]
    result["tail"] = " | ".join(tail)[-240:]
    return result


def checks(task, path):
    files = changed(task, path)
    results = {check["name"]: run_check(task, path, check, files) for check in task.get("checks", [])}
    references = task.get("reference", [])
    for reference in [references] if isinstance(references, dict) else references:
        copied = []
        for source, dest in reference["files"].items():
            target = os.path.join(path, dest)
            open(target, "w").write(git(task["repo"], "show", f"{reference['commit']}:{source}"))
            copied.append(target)
        results[reference.get("name", "reference")] = run_check(task, path, reference, files)
        for target in copied:
            os.remove(target)
    footprint = task.get("footprint", [])
    outside = [name for name in files if footprint and not any(fnmatch.fnmatch(name, glob) for glob in footprint)]
    touched = [name for name in files if any(fnmatch.fnmatch(name, glob) for glob in task.get("protected", []))]
    commits = git(path, "log", "--format=%h %s", f"{task['base']}..HEAD").splitlines()
    dirty = [line for line in git(path, "status", "--porcelain").splitlines() if not line.startswith("??")]
    return {"checks": results, "changed": files, "outside_footprint": outside, "protected_touched": touched,
            "commits": commits, "uncommitted": len(dirty)}


def forbidden(task):
    marks = [task["repo"], *task.get("forbidden", [])]
    for reference in [task["reference"]] if isinstance(task.get("reference"), dict) else task.get("reference", []):
        marks.append(git(task["repo"], "rev-parse", "--short=7", reference["commit"]))
    return [expand(mark) for mark in marks]


def reads(mine, marks):
    seen = {}
    for row in mine[-1:]:
        for message in row["body"].get("messages", []):
            for block in message.get("content") if isinstance(message.get("content"), list) else []:
                if block.get("type") == "tool_use":
                    found = [mark for mark in marks if re.search(re.escape(mark) + r"(?![\w-])", json.dumps(block["input"]))]
                    if found:
                        seen[block["id"]] = found
    return sorted({mark for found in seen.values() for mark in found}), len(seen)


def usage(log, session, model, marks):
    price = PRICES[model]
    rows = [json.loads(line) for line in open(log)] if os.path.exists(log) else []
    mine = [row for row in rows if session in json.dumps(row["body"].get("metadata", {}))]
    touched, calls_touching = reads(mine, marks)
    calls = []
    for row in mine:
        use = row["usage"]
        creation = use.get("cache_creation") or {}
        calls.append({"at": row["at"], "status": row["status"], "input": use.get("input_tokens", 0),
                      "cache_read": use.get("cache_read_input_tokens", 0),
                      "write_5m": creation.get("ephemeral_5m_input_tokens", 0),
                      "write_1h": creation.get("ephemeral_1h_input_tokens", 0), "output": use.get("output_tokens", 0)})
    sums = {key: sum(call[key] for call in calls) for key in price}
    usd = sum(sums[key] * price[key] for key in price) / 1e6
    return {"requests": len(calls), **sums, "usd": round(usd, 4), "calls": calls,
            "forbidden_reads": touched, "forbidden_tool_calls": calls_touching}


def run(task, label, model, orchestration, effort, idle, hour_cache, log, port):
    if model not in PRICES:
        sys.exit(f"replay.py has no price for {model}: add its rates from crates/core/agent-types/src/pricing.rs")
    path = slot(task, label)
    forge = forge_stub(task)
    home = os.path.join(task["work"], f"home-{label}")
    shutil.rmtree(home, ignore_errors=True)
    env = dict(os.environ, CLAUDE_CODE_USE_BEDROCK="1", CLAUDE_CODE_SKIP_BEDROCK_AUTH="1",
               ANTHROPIC_BEDROCK_BASE_URL=f"http://127.0.0.1:{port}", HERDR_ENV="0",
               FORGE_REPLAY_LOG=os.path.join(task["work"], f"forge-calls-{label}.log"), FORGE_REPLAY_IDLE=str(idle))
    if hour_cache:
        env["ENABLE_PROMPT_CACHING_1H"] = "1"
    binary = os.environ.get("NANOCODE_BIN", os.path.join(ROOT, "target/debug/nanocode"))
    started = time.time()
    done = subprocess.run([binary, "exec", "--home", home, "-m", model, "-t", effort,
                           "--claude-orchestration", orchestration, prompt(task, path)],
                          cwd=path, env=env, capture_output=True, text=True, timeout=task.get("timeout", 7200))
    wall = round(time.time() - started)
    session = (re.findall(r"session (\S+)", done.stderr) or ["?"])[-1]
    open(os.path.join(task["work"], f"{label}.out"), "w").write(done.stdout + "\n--- stderr ---\n" + done.stderr)
    forge_log = os.path.join(task["work"], f"forge-calls-{label}.log")
    calls = open(forge_log).read().splitlines() if os.path.exists(forge_log) else []
    return {"run": label, "task": task["name"], "model": model, "orchestration": orchestration, "effort": effort,
            "idle": idle, "hour_cache": hour_cache, "exit": done.returncode, "wall_s": wall, "session": session,
            **usage(log, session, model, forbidden(task)), **checks(task, path),
            "forge_calls": {kind: sum(line.startswith(kind) for line in calls)
                            for kind in ("ship decide", "board post", "commit", "ship task")},
            "report": done.stdout.strip()[-600:]}


def record(results, row):
    with open(results, "a") as out:
        out.write(json.dumps(row) + "\n")


def rounds(task, args):
    variants = [variant.split(":") for variant in args.variant]
    for model, _ in variants:
        if model not in PRICES:
            sys.exit(f"replay.py has no price for {model}: add its rates from crates/core/agent-types/src/pricing.rs")
    impl(task)
    forge_stub(task)
    for number in range(args.first, args.first + args.rounds):
        threads = []
        for model, orchestration in variants:
            label = f"r{number}-{model.split('-')[1]}-{orchestration}"
            def work(model=model, orchestration=orchestration, label=label):
                record(args.results, run(task, label, model, orchestration, args.effort, args.idle, args.hour_cache,
                                         args.log, args.port))
            thread = threading.Thread(target=work)
            thread.start()
            threads.append(thread)
        for thread in threads:
            thread.join()
        print(f"round {number} done", flush=True)


def summary(results):
    rows = [json.loads(line) for line in open(results)]
    print("| Run | Model | Orchestration | Wall s | Requests | Cache read | Cache write | Output | USD | "
          + " | ".join(sorted({name for row in rows for name in row["checks"]})) + " | Footprint |")
    names = sorted({name for row in rows for name in row["checks"]})
    for row in rows:
        cells = []
        for name in names:
            check = row["checks"].get(name, {})
            cells.append(f"exit {check.get('exit')}, {check.get('passed', '-')} passed, {check.get('failed', 0)} failed"
                         + (f", {check['matching_lines']} lines" if "matching_lines" in check else ""))
        footprint = "inside" if not row["outside_footprint"] and not row["protected_touched"] else \
            f"outside: {row['outside_footprint'] + row['protected_touched']}"
        print(f"| {row['run']} | {row['model']} | {row['orchestration']} | {row['wall_s']} | {row['requests']} | "
              f"{row['cache_read']:,} | {row['write_5m'] + row['write_1h']:,} | {row['output']:,} | {row['usd']:.3f} | "
              + " | ".join(cells) + f" | {footprint} |")
    groups = {}
    for row in rows:
        groups.setdefault((row["model"], row["orchestration"]), []).append(row)
    for (model, orchestration), group in groups.items():
        print(f"mean {model} {orchestration}: ${statistics.mean(row['usd'] for row in group):.3f}, "
              f"{statistics.mean(row['wall_s'] for row in group):.0f} s, "
              f"{statistics.mean(row['requests'] for row in group):.1f} requests over {len(group)} runs")


def extract(database, session, out):
    rows = sqlite3.connect(f"file:{expand(database)}?mode=ro", uri=True).execute(
        "select data from session_message where session_id = ? and type = 'user' order by seq", (session,)).fetchall()
    if not rows:
        sys.exit(f"no user message in session {session}")
    open(out, "w").write(json.loads(rows[0][0])["text"])


def main():
    parser = argparse.ArgumentParser(prog="replay.py")
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("run", "rounds"):
        command = commands.add_parser(name)
        command.add_argument("task")
        command.add_argument("--effort", default="high")
        command.add_argument("--idle", type=int, default=0)
        command.add_argument("--hour-cache", action="store_true")
        command.add_argument("--log", default=os.path.join(ROOT, "output/replay/proxy.jsonl"))
        command.add_argument("--port", type=int, default=18601)
        command.add_argument("--results", required=True)
    commands.choices["run"].add_argument("label")
    commands.choices["run"].add_argument("--model", default="claude-opus-5-5")
    commands.choices["run"].add_argument("--orchestration", default="code-mode", choices=["code-mode", "direct"])
    commands.choices["rounds"].add_argument("--rounds", type=int, default=2)
    commands.choices["rounds"].add_argument("--first", type=int, default=1)
    commands.choices["rounds"].add_argument("--variant", action="append", required=True)
    check = commands.add_parser("check")
    check.add_argument("task")
    check.add_argument("label")
    check.add_argument("--commit")
    commands.add_parser("summary").add_argument("results")
    prompt_from = commands.add_parser("prompt-from-opencode")
    prompt_from.add_argument("session")
    prompt_from.add_argument("out")
    prompt_from.add_argument("--database", default="~/.local/share/opencode/opencode.db")
    args = parser.parse_args()
    if args.command == "summary":
        return summary(args.results)
    if args.command == "prompt-from-opencode":
        return extract(args.database, args.session, args.out)
    task = load(args.task)
    os.makedirs(task["work"], exist_ok=True)
    if args.command == "check":
        print(json.dumps(checks(task, slot(task, args.label, args.commit)), indent=1))
    elif args.command == "run":
        record(args.results, run(task, args.label, args.model, args.orchestration, args.effort, args.idle,
                                 args.hour_cache, args.log, args.port))
    else:
        rounds(task, args)


if __name__ == "__main__":
    main()
