---
name: nc-board
description: Work from a shared nanocode task board with the nc-board command line - join it, claim ready work, renew the claim, post findings, finish, and wait for changes. Use when told to take work from an nc-board board (NC_BOARD_SQL or NC_BOARD_DIR is set, or a board URL or directory is given).
---

# nc-board

A board holds tasks that several agents share. Each task is open, claimed by one agent under a lease, then done, failed, cancelled or blocked. Every command prints JSON. Exit 0 is success, 2 means nothing was ready or nothing changed before the timeout, and 1 is an error, whose message is on stderr.

To install it from a nanocode checkout, run `cargo install --path bin/nc-board`. To install this skill, copy this file to `.claude/skills/nc-board/SKILL.md` for Claude Code, or to `~/.codex/skills/nc-board/SKILL.md` for Codex.

## Join

```sh
export NC_BOARD_SQL=postgres://user:password@host:5432/db   # a board every machine shares; or NC_BOARD_DIR=/path for one machine
export NC_BOARD_AS=agent:<your-name>                         # unique per agent: claims, replies and results carry it
nc-board whoami                                               # the board, what you may do, the claims you hold
```

Set exactly one of NC_BOARD_SQL and NC_BOARD_DIR. The `--sql URL`, `--dir DIR` and `--as ACTOR` flags override them. Only the first agent on a new board runs `nc-board init`.

## Start agents only once they hold a claim

Whoever starts agent sessions should start each one through `nc-board run`. That way a session starts only after it holds a claim, and no session wakes up to find the work already taken:

```sh
nc-board run next --wait 300 --poll-ms 5000 --lease 900 --on-exit release [--label lane=docs] -- \
  sh -c 'claude -p "Do nc-board task $NC_BOARD_TASK: read it with nc-board show, then follow the nc-board skill"'
```

`run` waits until a task is ready and claims it before it starts the command, so wakes that would find nothing claimable start nothing. It sets NC_BOARD_TASK and NC_BOARD_CLAIM for the command, which expands them inside its single-quoted `sh -c`, and renews the claim every third of the lease while the command runs.

When the command exits still holding the claim, `--on-exit release` hands the task back for another agent. The default, `--on-exit finish`, marks the task done on exit 0 and failed otherwise. If the command finished, failed or released the task itself, `run` keeps that result. On SIGTERM, SIGINT or SIGHUP, `run` stops the command and releases the task. If `run` is killed outright, its lease runs out and the task becomes claimable again.

## Work loop for an agent

Inside a `run`, you already hold NC_BOARD_TASK: start at step 2 and skip renewing, since `run` renews for you. Without `run`, start at step 1.

1. Claim the next ready task, waiting up to 5 minutes for one:
   `nc-board claim-next --lease 900 --wait 300 --poll-ms 5000 [--label lane=docs]`
   It prints the task. `id` names it and `spec` says what to do. Exit 2 means nothing became ready, so stop or claim again.
2. Do the work. Without `run`, renew the claim before a third of the lease is left: `nc-board renew ID --lease 900`. If renew fails, you lost the claim: stop, and don't finish it.
3. Post what you find as you go: `nc-board reply ID "found X in src/y.rs:42"`. Attach links with `nc-board label ID --label pr=owner/repo#12`, and post follow-up work with `nc-board post ID/next --parent ID "what to do"`.
4. End the claim with exactly one of these:
   - `nc-board finish ID "what changed, with links"` when it's done
   - `nc-board fail ID "why"` when it can't be done
   - `nc-board release ID "why"` to hand it back for someone else
   - `nc-board block ID KIND "reason"` when it waits on something outside the board, such as KIND review
5. Without `run`, go back to step 1. Inside `run`, exit.

## Shared checkouts and machines

- Work only in your own worktree or clone, such as `git worktree add ../w-$NC_BOARD_TASK origin/master`. Never edit, reset or check out another agent's worktree.
- Format and fix only the files you changed: `rustfmt path/to/file.rs`, never repo-wide `cargo fmt` or `cargo clippy --fix`.
- Never kill processes you didn't start. Stop your own by the PID you saved, never with `pkill` or `killall` by name.

## Posting work that may fail

A failed task is final, and it cancels the tasks that need it. When a failure should get another try, say so when you post the task:

- `--attempts N [--backoff SECS]` opens a failed task again for up to N claims, after SECS, doubling each time. Use it for work such as landing and evaluating, where a retry often succeeds.
- `--on-fail wait` keeps the tasks that need it waiting instead of cancelling them. A failed task then stays until `nc-board reopen ID` brings it back.

In a `plan` file, give the same as `"terms": {"on_fail": "wait", "retry": {"max_attempts": 3, "backoff_ms": 0}}` on each task.

## Wait for changes

`nc-board wait --timeout 300 --poll-ms 5000 --under ID --kind replied` prints the changes once one matches: replies under ID here. Pass `--after SEQ` with the last `seq` you saw so nothing is missed between calls. On Postgres it wakes within milliseconds of the write through LISTEN/NOTIFY, and re-reads every `--poll-ms` as well.

Keep `--wait` and `--timeout` below your harness's command timeout and loop. Claude Code's Bash tool stops a command after 2 minutes unless you give a timeout of up to 10 minutes.

## Look around

`nc-board list --status ready`, `nc-board show ID`, `nc-board events --after SEQ` and `nc-board may ID`. `nc-board --help` lists every command.

## Rules

- Finish, renew or release only the tasks you claimed.
- Choose a lease that frees your work soon after a crash and leaves time to renew. 10 to 15 minutes, renewed every few minutes, suits most work.
- Write results someone else can act on: what changed, where it is, and what is left.
