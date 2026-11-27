# Replaying a Forge task

`replay.py` reruns a finished Forge implementer task with a different model, thinking effort or tool
orchestration, and measures each run: what it cost, how long it took, how many model requests it made, and
whether the work passes the task's own checks. The aim is a fair comparison: every run starts from the commit
before the task, with the same prompt and the same Forge context, and no run can see the original's answer.

## What a run does

1. **Slot.** It fetches `base`, the commit the task started from, into a new repository: only the
   commits reachable from `base`, with blobs fetched on demand from a stripped shared clone of the
   task's repository that has no other refs. So the run cannot read the task's own commit or any later
   one, even by id. It checks out the task's branch; the remote has a push URL that does not exist,
   so nothing can be pushed. `links` symlinks large untracked directories, such as `node_modules`,
   from an existing checkout (`link_from`).
2. **Forge context.** It copies the Impl's plan files into the slot's `.forge/current`. With
   `forge_context.repo` and `forge_context.commit`, it reads them from that commit of the vault, with
   `from` a path in it: take the checkpoint from just before the task was handed out, because a later
   snapshot already shows the task done and cites its evidence. When no checkpoint predates the task,
   take the first one after it and undo what the task changed with `forge_context.edits`, a list of
   `[file, old, new]` replacements; one that does not match stops the run. Of `decisions.md` it
   keeps only the first `decisions_lines` lines, the header, because the decisions the task itself took
   would give the answer away. With `"impl": true` it also makes a read-only clone at `base` to stand in for
   the Impl worktree, so the prompt's "read from the Impl worktree" never reaches the finished work.
3. **Forge stub.** `FORGE` points at a stub `scripts/forge` beside copies of the real `prompts/` and
   `agents/`. The stub records every call in `forge-calls-<label>.log` and changes nothing.
   `forge commit` runs `git commit`. With `--idle N`, the first `forge commit` says a sibling holds the
   commit lock (exit 75, as the real one does), and the retry waits `N` seconds, so the run sits idle with
   its largest context.
4. **Run.** It runs `nanocode exec` in the slot, with the original task prompt (paths rewritten by
   `rewrite`) after the agent's text (`agent`, front matter dropped). The model, `-t` effort and
   `--claude-orchestration` are as given.
5. **Forbidden reads.** The slot holds nothing past `base`, but the run is not sandboxed: it can read
   any path on the machine. The harness scans every tool call of the run for the original repository's
   path, each reference commit's id and the strings in `forbidden` (other checkouts of the repository,
   the vault), and records them as `forbidden_reads` and `forbidden_tool_calls`. A run with any is not
   a clean replay.
6. **Checks.** It runs each check in `checks` in the slot, then copies the original commit's test files in
   (`reference.files`, under a new name so they sit beside the run's own tests) and runs
   `reference.command`. It reports the files changed outside `footprint`, any `protected` file touched,
   the commits made and any uncommitted changes.

## Task file

A task is a JSON file. Keep task files, prompts and results out of the repository, in `output/`:

```json
{
  "name": "example-task",
  "repo": "~/src/example",
  "base": "<commit before the task>",
  "branch": "<the task's branch>",
  "prompt": "output/replay/tasks/example-task.prompt.txt",
  "agent": "~/.forge/agents/forge-implementer.md",
  "forge_home": "~/.forge",
  "impl": true,
  "links": ["node_modules"],
  "link_from": "~/src/example-impl",
  "forge_context": {"from": "<vault>/implementations/IMPL-NNN-.../snapshot",
                    "files": ["plan.md", "tasks.md", "verification.md", "pr-checklist.md"], "decisions_lines": <lines of the header>},
  "rewrite": [["<original slot path>", "{slot}"], ["<original Impl worktree>", "{impl}"],
              ["FORGE=<original>", "FORGE={forge}"], ["FORGE_VAULT=<original>", "FORGE_VAULT={vault}"]],
  "check_env": {"PATH": "<toolchain>/bin:$PATH"},
  "checks": [{"name": "specs", "command": "unshare -rn npx jest src/thing"},
             {"name": "tsc", "command": "npx tsc --noEmit -p .", "count": "error TS", "only_changed": true},
             {"name": "eslint", "command": "npx eslint --quiet {changed}"}],
  "reference": {"commit": "<the task's commit>",
                "files": {"src/thing.acceptance.test.ts": "src/zz-reference.acceptance.test.ts"},
                "command": "unshare -rn npx jest src/zz-reference.acceptance.test.ts"},
  "footprint": ["src/thing.ts", "src/thing.test.ts", "src/thing.acceptance.test.ts"],
  "protected": []
}
```

- **Placeholders.** `rewrite` replacements may use `{slot}`, `{impl}`, `{forge}`, `{vault}` and `{work}`.
  `work` is where slots and logs go (default `$TMPDIR/nanocode-replay/<name>`).
- **Reading a check.** For each check the harness records its exit code and the `N passed` / `N failed`
  counts in its output. With `count`, it also records how many output lines match that regular
  expression; `only_changed` keeps only the lines that name a changed file, for a type check whose base
  already has errors.
- **Several reference specs.** `reference` may be a list of such objects, each with a `name`, so each
  original spec is reported on its own. A spec that imports a helper the prompt never names fails to
  compile against a correct replay that named it differently; report it beside the specs that test
  through existing entry points, not as a failure of the run.
- **`{changed}`** in a command is replaced by the changed files that still exist.

Run `replay.py check <task> <label> --commit <commit>` on `base` and on the task's commit before measuring
anything. The reference test should fail on `base` and pass on the original commit. If it does not, the
oracle proves nothing.

To get the prompt of a task run by OpenCode, copy the first user message of the implementer's session:
`replay.py prompt-from-opencode <session id> output/replay/tasks/<name>.prompt.txt`.

## Measuring

`proxy.py` signs requests for Bedrock runtime and writes one line per model request to
`output/replay/proxy.jsonl`. Each line holds the time the request started, its status, the usage Bedrock
reported (input, cache read, 5-minute and 1-hour cache writes, output) and the request body. Runs are told
apart by the session id in the request's `metadata`.

```sh
AWS_PROFILE=<profile> AWS_REGION=<region> python3 bench/replay/proxy.py 18601 &
cargo build -p nanocode-cli --bin nanocode
AWS_REGION=<region> python3 bench/replay/replay.py rounds output/replay/tasks/example-task.json --rounds 2 \
  --variant claude-sonnet-5-5:code-mode --variant claude-opus-5-5:code-mode \
  --results output/replay/example-task.jsonl
python3 bench/replay/replay.py summary output/replay/example-task.jsonl
```

- **Rounds.** `rounds` runs one run of every `--variant` (`model:orchestration`) at the same time, so
  each round compares the variants under the same load and the same Bedrock conditions.
- **One run.** `run <task> <label>` starts a single run; `--idle N` and `--hour-cache` (which sets
  `ENABLE_PROMPT_CACHING_1H=1`) apply to both commands.
- **Results.** Each run appends one JSON line to `--results`: wall time, request count, token totals,
  USD, one entry per request under `calls`, the check results, the footprint, the commits and the
  counts of Forge calls by kind. The slot, the transcript (`<label>.out`) and the Forge call log stay
  under `work` for review.

USD is priced from the proxy's usage at these rates (US$ per million tokens, Bedrock global ids, the same
rates as `crates/core/agent-types/src/pricing.rs`); regional ids cost 10% more. A model missing from
`PRICES` in `replay.py` is refused; add its rates from `pricing.rs` first:

| Model | Input | Cache read | 5-minute write | 1-hour write | Output |
|---|---|---|---|---|---|
| `claude-opus-5-5` | 4 | 0.20 | 5 | 8 | 20 |
| `claude-sonnet-5-5` | 2 | 0.10 | 2.50 | 4 | 10 |
