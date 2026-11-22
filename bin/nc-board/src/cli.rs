use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::Read as _,
    os::unix::process::ExitStatusExt as _,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    sync::{Arc, mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nanocode_board::{
    Board, BoardError, ClaimId, Entry, NewTask, Op, Outcome, Phase, Rejection, Task, TaskId,
    access::{Can, Grant, Identity, Verifier, Who},
    tasks::{OnFail, Planned, TaskOp, Terms},
    watch::{Filter, Kind, Plan},
};
use nanocode_turnlog::{FileLog, FileOptions};
use serde_json::{Value, json};

const SCOPE: &str = "board";

const SPREAD: usize = 8;

const HELP: &str = "nc-board: a nanocode task board kept in a directory
usage: nc-board [--dir DIR] [--as ACTOR] COMMAND
  DIR defaults to $NC_BOARD_DIR, ACTOR to $NC_BOARD_AS; ACTOR is a principal such as agent:b1, optionally with +profile
  init                                     charter the board as ACTOR and let everyone read, post, claim and manage
  post ID [--parent ID] [--label K=V]... [--needs ID]... [--assignee ACTOR] [--on-fail cascade|wait] SPEC
  list [--label K=V]... [--status open|claimed|done|failed|cancelled|blocked|ready]...
  show ID
  claim ID --lease SECS
  claim-next [--label K=V]... [--spread K] --lease SECS [--wait SECS --poll-ms MS]
                                           claim a ready task matching every label: best rank, then ACTOR's own,
                                           then one of the K (default 8) oldest; retries the oldest after a lost race; exits 2 when none is
                                           ready, or with --wait when none became ready in SECS
  plan NAME FILE                           post a JSON array of {key, spec, labels?, needs?: [{key}|{task}], parent?} as tasks NAME/key
                                           in one write; FILE - reads stdin, and the same NAME and FILE again posts nothing
  run (ID | next [--label K=V]... [--spread K]) --lease SECS [--result TEXT] -- COMMAND...
                                           claim, run COMMAND with NC_BOARD_TASK and NC_BOARD_CLAIM set, renew every third of the
                                           lease, then finish on exit 0 or fail; exits with COMMAND's code, or 1 when the claim is lost
  renew ID --lease SECS [--claim CLAIM]
  finish ID [--claim CLAIM] DETAIL
  fail ID [--claim CLAIM] DETAIL
  release ID [--claim CLAIM] NOTE
  block ID KIND REASON
  unblock ID
  reopen ID
  cancel ID REASON
  reply ID BODY
  label ID [--label K=V]... [--unset K]...  set and remove labels on a task, such as pr=owner/repo#12 or status=review
  assign ID [--to ACTOR]                   keep a task for one actor; without --to, return it to anyone who may claim it
  schedule ID --in SECS [--every SECS] [--times N] BODY  reply BODY on the task when due, and every SECS after with --every
  events [--after SEQ] [--label K=V]... [--kind KIND]... [--under ID]  the log's changes after SEQ, each with who wrote it
  wait --timeout SECS --poll-ms MS [--after SEQ] [--label K=V]... [--kind KIND]... [--under ID]
                                           fire due schedules and print the changes after SEQ (default: the head when it
                                           starts) once one matches; exits 2 when none comes before the timeout
every command prints JSON: a task, or a list of tasks; CLAIM defaults to ACTOR's current claim on the task";

type Failure = Box<dyn std::error::Error>;

struct Parsed {
    flags: BTreeMap<String, Vec<String>>,
    words: Vec<String>,
}

impl Parsed {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, Failure> {
        let mut flags = BTreeMap::<String, Vec<String>>::new();
        let mut words = Vec::new();
        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            if argument == "--" {
                words.extend(arguments.by_ref());
            } else if matches!(argument.as_str(), "-h" | "--help") {
                words.insert(0, "help".to_owned());
            } else if let Some(name) = argument.strip_prefix("--") {
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("--{name} needs a value"))?;
                flags.entry(name.to_owned()).or_default().push(value);
            } else {
                words.push(argument);
            }
        }
        Ok(Self { flags, words })
    }

    fn allow(&self, names: &[&str]) -> Result<(), Failure> {
        match self
            .flags
            .keys()
            .find(|name| !names.contains(&name.as_str()) && !matches!(name.as_str(), "dir" | "as"))
        {
            Some(name) => Err(format!("--{name} does not apply here").into()),
            None => Ok(()),
        }
    }

    fn one(&self, name: &str) -> Result<Option<String>, Failure> {
        match self.flags.get(name).map(Vec::as_slice) {
            None => Ok(None),
            Some([value]) => Ok(Some(value.clone())),
            Some(_) => Err(format!("--{name} is given more than once").into()),
        }
    }

    fn many(&self, name: &str) -> Vec<String> {
        self.flags.get(name).cloned().unwrap_or_default()
    }

    fn words<const N: usize>(&self) -> Result<[String; N], Failure> {
        <[String; N]>::try_from(self.words[1..].to_vec()).map_err(|words| {
            format!(
                "{} takes {N} arguments, not {}; see nc-board --help",
                self.words[0],
                words.len()
            )
            .into()
        })
    }

    fn labels(&self) -> Result<BTreeMap<String, String>, Failure> {
        self.many("label")
            .iter()
            .map(|pair| {
                pair.split_once('=')
                    .filter(|(key, _)| !key.is_empty())
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .ok_or_else(|| format!("label {pair} is not KEY=VALUE").into())
            })
            .collect()
    }

    fn lease_ms(&self) -> Result<u64, Failure> {
        self.secs_ms("lease")?
            .ok_or_else(|| "--lease SECS is required".into())
    }

    fn secs_ms(&self, name: &str) -> Result<Option<u64>, Failure> {
        let Some(secs) = self.one(name)? else {
            return Ok(None);
        };
        let secs = secs
            .parse::<u64>()
            .map_err(|error| format!("--{name} {secs}: {error}"))?;
        Ok(Some(secs.checked_mul(1000).ok_or_else(|| {
            format!("--{name} overflows milliseconds")
        })?))
    }

    fn count(&self, name: &str) -> Result<Option<u64>, Failure> {
        self.one(name)?
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|error| format!("--{name} {value}: {error}").into())
            })
            .transpose()
    }

    fn filter(&self) -> Result<Filter, Failure> {
        let kinds = self
            .many("kind")
            .iter()
            .map(|kind| {
                serde_json::from_value::<Kind>(Value::String(kind.clone())).map_err(|_| {
                    format!("--kind {kind} is not a change kind such as replied").into()
                })
            })
            .collect::<Result<BTreeSet<_>, Failure>>()?;
        Ok(Filter {
            under: self.one("under")?.map(TaskId),
            labels: self.labels()?,
            kinds,
        })
    }

    fn setting(&self, flag: &str, variable: &str) -> Result<String, Failure> {
        match self.one(flag)? {
            Some(value) => Ok(value),
            None => std::env::var(variable)
                .map_err(|_| format!("give --{flag} or set {variable}").into()),
        }
    }
}

struct Trusted;

impl Verifier for Trusted {
    fn verify(&self, credential: &str) -> Option<String> {
        Some(credential.to_owned())
    }
}

fn identity(actor: &str) -> Result<Identity, Failure> {
    let (principal, profile) = match actor.split_once('+') {
        Some((principal, profile)) => (principal, Some(profile)),
        None => (actor, None),
    };
    let identity =
        Identity::verify(&Trusted, principal).map_err(|denied| format!("{actor}: {denied:?}"))?;
    match profile {
        Some(profile) => identity
            .profile(profile)
            .map_err(|denied| format!("{actor}: {denied:?}").into()),
        None => Ok(identity),
    }
}

fn now_ms() -> Result<u64, Failure> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

const fn status(task: &Task) -> &'static str {
    match task.phase {
        Phase::Open => "open",
        Phase::Claimed { .. } => "claimed",
        Phase::Done { .. } => "done",
        Phase::Failed { .. } => "failed",
        Phase::Cancelled { .. } => "cancelled",
        Phase::Blocked { .. } => "blocked",
    }
}

fn render(board: &Board, id: &TaskId, task: &Task, at_ms: u64) -> Value {
    let mut out = json!({
        "id": id,
        "parent": task.spec.parent,
        "spec": task.spec.spec,
        "labels": task.spec.labels,
        "needs": task.spec.needs,
        "assignee": task.assignee,
        "status": status(task),
        "ready": board.fold().claimable(id, at_ms).is_ok(),
        "attempts": task.attempts,
        "replies": task.replies.iter().map(|(by, body)| json!({"by": by, "body": body})).collect::<Vec<_>>(),
    });
    let phase = match &task.phase {
        Phase::Open => json!({}),
        Phase::Claimed {
            claim,
            worker,
            until_ms,
        } => {
            json!({"claim": claim, "worker": worker, "until_ms": until_ms, "expired": *until_ms <= at_ms})
        }
        Phase::Done { result, .. } => json!({"result": result}),
        Phase::Failed { error, .. } => json!({"error": error}),
        Phase::Cancelled { reason } => json!({"reason": reason}),
        Phase::Blocked { kind, reason } => json!({"kind": kind, "reason": reason}),
    };
    if let (Value::Object(out), Value::Object(phase)) = (&mut out, phase) {
        out.extend(phase);
    }
    out
}

fn author(event: &str) -> &str {
    event
        .split_once('#')
        .or_else(|| event.rsplit_once('.'))
        .map_or(event, |(by, _)| by)
}

fn changes(board: &Board, after: u64, filter: &Filter) -> Vec<Value> {
    let tasks = board.fold().tasks();
    board
        .entries()
        .filter(|entry| entry.seq > after && filter.matches(tasks, entry))
        .map(|entry: &Entry| {
            json!({
                "seq": entry.seq,
                "event": entry.event,
                "by": author(&entry.event.0),
                "at_ms": entry.at_ms,
                "task": entry.task,
                "change": entry.change,
            })
        })
        .collect()
}

fn shown(board: &Board, id: &TaskId, at_ms: u64) -> Result<Value, Failure> {
    let task = board
        .task(id)
        .ok_or_else(|| format!("no task {id} on this board"))?;
    Ok(render(board, id, task, at_ms))
}

fn held(board: &Board, id: &TaskId, given: Option<String>) -> Result<ClaimId, Failure> {
    if let Some(claim) = given {
        return Ok(ClaimId(claim));
    }
    match board.task(id).map(|task| &task.phase) {
        Some(Phase::Claimed { claim, worker, .. }) if *worker == board.writer() => {
            Ok(claim.clone())
        }
        Some(Phase::Claimed { worker, .. }) => Err(format!(
            "{id} is claimed by {worker}, not {}; pass --claim",
            board.writer()
        )
        .into()),
        Some(_) => Err(format!("{id} is not claimed").into()),
        None => Err(format!("no task {id} on this board").into()),
    }
}

async fn claim_next(
    parsed: &Parsed,
    board: &mut Board,
    at_ms: u64,
) -> Result<Option<TaskId>, Failure> {
    let lease_ms = parsed.lease_ms()?;
    let labels = parsed.labels()?;
    let spread = match parsed.one("spread")? {
        Some(spread) => spread
            .parse::<usize>()
            .ok()
            .filter(|spread| *spread > 0)
            .ok_or(format!("--spread {spread} is not a positive count"))?,
        None => SPREAD,
    };
    let Some(task) = board
        .fold()
        .spread(board.writer(), &labels, at_ms, spread)
        .cloned()
    else {
        return Ok(None);
    };
    let claim = |task: &TaskId| Op::Claim {
        task: task.clone(),
        lease_ms,
        fence: None,
        cap: None,
    };
    match board.submit(at_ms, claim(&task)).await {
        Err(BoardError::Rejected(Rejection::Held { .. } | Rejection::Terminal)) if spread > 1 => {
            let Some(head) = board.fold().pick(board.writer(), &labels, at_ms).cloned() else {
                return Ok(None);
            };
            submit(board, at_ms, claim(&head)).await?;
            Ok(Some(head))
        }
        landed => {
            landed.map_err(|error| error.to_string())?;
            Ok(Some(task))
        }
    }
}

async fn submit(board: &mut Board, at_ms: u64, op: Op) -> Result<(), Failure> {
    board
        .submit(at_ms, op)
        .await
        .map(drop)
        .map_err(|error| match error {
            BoardError::Unsettled { .. } => {
                format!("{error}; run show to see whether it landed").into()
            }
            error => error.to_string().into(),
        })
}

async fn run(parsed: &Parsed, board: &mut Board) -> Result<Option<Value>, Failure> {
    let at_ms = now_ms()?;
    let command = parsed.words[0].as_str();
    let id = |word: &str| TaskId(word.to_owned());
    let written = match command {
        "init" => {
            parsed.allow(&[])?;
            parsed.words::<0>()?;
            board
                .charter(at_ms)
                .await
                .map_err(|error| error.to_string())?;
            let grant = Grant {
                who: vec![Who::Everyone],
                can: BTreeMap::from([("*".to_owned(), Can::ALL.to_vec())]),
            };
            board
                .grant("everyone", at_ms, grant)
                .await
                .map_err(|error| error.to_string())?;
            return Ok(Some(
                json!({"owner": board.writer(), "granted": "everyone may do everything"}),
            ));
        }
        "list" => {
            parsed.allow(&["label", "status"])?;
            parsed.words::<0>()?;
            let labels = parsed.labels()?;
            let statuses = parsed.many("status");
            let listed = board
                .tasks()
                .filter(|(_, task)| {
                    labels
                        .iter()
                        .all(|(key, value)| task.spec.labels.get(key) == Some(value))
                })
                .map(|(id, task)| render(board, id, task, at_ms))
                .filter(|task| {
                    statuses.is_empty()
                        || statuses.iter().any(|wanted| {
                            task["status"] == wanted.as_str()
                                || (wanted == "ready" && task["ready"] == true)
                        })
                })
                .collect::<Vec<_>>();
            return Ok(Some(Value::Array(listed)));
        }
        "show" => {
            parsed.allow(&[])?;
            let [task] = parsed.words()?;
            return shown(board, &id(&task), at_ms).map(Some);
        }
        "claim-next" => {
            parsed.allow(&["label", "lease", "spread", "wait", "poll-ms"])?;
            parsed.words::<0>()?;
            let Some(task) = claim_next(parsed, board, at_ms).await? else {
                return Ok(None);
            };
            task
        }
        "plan" => {
            parsed.allow(&[])?;
            let [name, file] = parsed.words()?;
            let mut text = String::new();
            if file == "-" {
                std::io::stdin().read_to_string(&mut text)?;
            } else {
                text =
                    std::fs::read_to_string(&file).map_err(|error| format!("{file}: {error}"))?;
            }
            let mut plan =
                serde_json::from_str::<Value>(&text).map_err(|error| format!("{file}: {error}"))?;
            for planned in plan.as_array_mut().into_iter().flatten() {
                if let Value::Object(planned) = planned {
                    planned.entry("labels").or_insert_with(|| json!({}));
                    planned.entry("needs").or_insert_with(|| json!([]));
                }
            }
            let tasks = serde_json::from_value::<Vec<Planned>>(plan)
                .map_err(|error| format!("{file}: {error}"))?;
            let placed = board
                .batch(at_ms, id(&name), tasks)
                .await
                .map_err(|error| error.to_string())?;
            return placed
                .iter()
                .map(|task| shown(board, task, at_ms))
                .collect::<Result<Vec<_>, _>>()
                .map(|tasks| Some(Value::Array(tasks)));
        }
        "post" => {
            parsed.allow(&["parent", "label", "needs", "assignee", "on-fail"])?;
            let [task, spec] = parsed.words()?;
            let on_fail = match parsed.one("on-fail")?.as_deref() {
                None | Some("cascade") => OnFail::Cascade,
                Some("wait") => OnFail::Wait,
                Some(other) => {
                    return Err(format!("--on-fail {other} is not cascade or wait").into());
                }
            };
            let new = NewTask {
                id: id(&task),
                parent: parsed.one("parent")?.map(|parent| id(&parent)),
                spec,
                labels: parsed.labels()?,
                needs: parsed.many("needs").iter().map(|need| id(need)).collect(),
                terms: Terms {
                    assignee: parsed.one("assignee")?,
                    on_fail,
                    ..Terms::default()
                },
            };
            submit(board, at_ms, Op::Post { task: new }).await?;
            id(&task)
        }
        "claim" => {
            parsed.allow(&["lease"])?;
            let [task] = parsed.words()?;
            let lease_ms = parsed.lease_ms()?;
            submit(
                board,
                at_ms,
                Op::Claim {
                    task: id(&task),
                    lease_ms,
                    fence: None,
                    cap: None,
                },
            )
            .await?;
            id(&task)
        }
        "renew" => {
            parsed.allow(&["lease", "claim"])?;
            let [task] = parsed.words()?;
            let lease_ms = parsed.lease_ms()?;
            let claim = held(board, &id(&task), parsed.one("claim")?)?;
            submit(
                board,
                at_ms,
                Op::Renew {
                    task: id(&task),
                    claim,
                    lease_ms,
                },
            )
            .await?;
            id(&task)
        }
        "finish" | "fail" => {
            parsed.allow(&["claim"])?;
            let [task, detail] = parsed.words()?;
            let claim = held(board, &id(&task), parsed.one("claim")?)?;
            let outcome = if command == "finish" {
                Outcome::Done(detail)
            } else {
                Outcome::Failed(detail)
            };
            submit(
                board,
                at_ms,
                Op::Finish {
                    task: id(&task),
                    claim,
                    outcome,
                },
            )
            .await?;
            id(&task)
        }
        "release" => {
            parsed.allow(&["claim"])?;
            let [task, note] = parsed.words()?;
            let claim = held(board, &id(&task), parsed.one("claim")?)?;
            submit(
                board,
                at_ms,
                Op::Release {
                    task: id(&task),
                    claim,
                    note,
                },
            )
            .await?;
            id(&task)
        }
        "block" => {
            parsed.allow(&[])?;
            let [task, kind, reason] = parsed.words()?;
            let claim = match board.task(&id(&task)).map(|task| &task.phase) {
                Some(Phase::Claimed { claim, worker, .. }) if *worker == board.writer() => {
                    Some(claim.clone())
                }
                _ => None,
            };
            submit(
                board,
                at_ms,
                Op::Tasks(TaskOp::Block {
                    task: id(&task),
                    claim,
                    kind,
                    reason,
                }),
            )
            .await?;
            id(&task)
        }
        "unblock" | "reopen" => {
            parsed.allow(&[])?;
            let [task] = parsed.words()?;
            let op = if command == "unblock" {
                TaskOp::Unblock { task: id(&task) }
            } else {
                TaskOp::Reopen { task: id(&task) }
            };
            submit(board, at_ms, Op::Tasks(op)).await?;
            id(&task)
        }
        "cancel" => {
            parsed.allow(&[])?;
            let [task, reason] = parsed.words()?;
            submit(
                board,
                at_ms,
                Op::Cancel {
                    task: id(&task),
                    reason,
                },
            )
            .await?;
            id(&task)
        }
        "reply" => {
            parsed.allow(&[])?;
            let [task, body] = parsed.words()?;
            submit(
                board,
                at_ms,
                Op::Reply {
                    task: id(&task),
                    body,
                },
            )
            .await?;
            id(&task)
        }
        "label" => {
            parsed.allow(&["label", "unset"])?;
            let [task] = parsed.words()?;
            let set = parsed.labels()?;
            let unset = parsed.many("unset").into_iter().collect::<BTreeSet<_>>();
            if set.is_empty() && unset.is_empty() {
                return Err("label needs --label K=V or --unset K".into());
            }
            submit(
                board,
                at_ms,
                Op::Relabel {
                    task: id(&task),
                    set,
                    unset,
                },
            )
            .await?;
            id(&task)
        }
        "assign" => {
            parsed.allow(&["to"])?;
            let [task] = parsed.words()?;
            submit(
                board,
                at_ms,
                Op::Tasks(TaskOp::Assign {
                    task: id(&task),
                    to: parsed.one("to")?,
                }),
            )
            .await?;
            id(&task)
        }
        "schedule" => {
            parsed.allow(&["in", "every", "times"])?;
            let [task, body] = parsed.words()?;
            let wait_ms = parsed.secs_ms("in")?.ok_or("--in SECS is required")?;
            let first_ms = at_ms
                .checked_add(wait_ms)
                .ok_or("--in overflows the clock")?;
            let schedule = format!("{}/{task}/{at_ms}", board.writer());
            board
                .schedule(
                    at_ms,
                    Plan {
                        id: schedule.clone(),
                        send: Op::Reply {
                            task: id(&task),
                            body,
                        },
                        first_ms,
                        every_ms: parsed.secs_ms("every")?,
                        until_ms: None,
                        times: parsed.count("times")?,
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            return Ok(Some(
                json!({"schedule": schedule, "task": task, "due_ms": first_ms}),
            ));
        }
        "events" => {
            parsed.allow(&["after", "label", "kind", "under"])?;
            parsed.words::<0>()?;
            let after = parsed.count("after")?.unwrap_or(0);
            return Ok(Some(Value::Array(changes(board, after, &parsed.filter()?))));
        }
        other => return Err(format!("unknown command {other}; see nc-board --help").into()),
    };
    shown(board, &written, now_ms()?).map(Some)
}

fn open(dir: &Path) -> Result<std::fs::File, Failure> {
    std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("board.lock"))
        .map_err(|error| format!("{}: {error}", dir.join("board.lock").display()))?;
    lock.lock()
        .map_err(|error| format!("{}: {error}", dir.join("board.lock").display()))?;
    Ok(lock)
}

async fn opened(dir: &Path, identity: Identity) -> Result<Board, Failure> {
    let host = Arc::new(FileLog::open(
        &dir.join("board.log"),
        FileOptions {
            scope: SCOPE,
            sync: true,
        },
    )?);
    Board::open(host, SCOPE, identity)
        .await
        .map_err(|error| error.to_string().into())
}

fn locked<T>(
    dir: &Path,
    identity: &Identity,
    runtime: &nanocode_executor::Runtime,
    work: impl AsyncFnOnce(&mut Board) -> Result<T, Failure>,
) -> Result<T, Failure> {
    let lock = open(dir)?;
    let result = runtime.block_on(async {
        let mut board = opened(dir, identity.clone()).await?;
        work(&mut board).await
    });
    drop(lock);
    result
}

fn pulling(
    parsed: &Parsed,
    dir: &Path,
    identity: &Identity,
    runtime: &nanocode_executor::Runtime,
) -> Result<Option<Value>, Failure> {
    let wait_ms = parsed.secs_ms("wait")?.ok_or("--wait SECS is required")?;
    let poll_ms = parsed
        .count("poll-ms")?
        .filter(|poll| *poll > 0)
        .ok_or("--wait needs --poll-ms MS, positive")?;
    let deadline_ms = now_ms()?.saturating_add(wait_ms);
    loop {
        if let Some(found) = locked(dir, identity, runtime, async |board| {
            run(parsed, board).await
        })? {
            return Ok(Some(found));
        }
        let now = now_ms()?;
        if now >= deadline_ms {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(poll_ms.min(deadline_ms - now)));
    }
}

fn running(
    parsed: &Parsed,
    dir: &Path,
    actor: &str,
    identity: &Identity,
    runtime: &nanocode_executor::Runtime,
) -> Result<Option<Value>, Failure> {
    parsed.allow(&["lease", "label", "spread", "result"])?;
    let lease_ms = parsed.lease_ms()?;
    let renew_every = Duration::from_millis(lease_ms / 3);
    if renew_every.is_zero() {
        return Err("run needs a lease of at least 3 ms to renew every third of it".into());
    }
    let (target, command) = parsed.words[1..]
        .split_first()
        .ok_or("run takes ID or next, then -- COMMAND")?;
    let (program, arguments) = command.split_first().ok_or("run needs -- COMMAND")?;
    let (task, claim) = locked(dir, identity, runtime, async |board| {
        let at_ms = now_ms()?;
        let task = if target == "next" {
            claim_next(parsed, board, at_ms)
                .await?
                .ok_or("no ready task matches")?
        } else {
            let task = TaskId(target.clone());
            submit(
                board,
                at_ms,
                Op::Claim {
                    task: task.clone(),
                    lease_ms,
                    fence: None,
                    cap: None,
                },
            )
            .await?;
            task
        };
        let claim = held(board, &task, None)?;
        Ok((task, claim))
    })?;
    let mut child = Command::new(program)
        .args(arguments)
        .env("NC_BOARD_TASK", &task.0)
        .env("NC_BOARD_CLAIM", &claim.0)
        .env("NC_BOARD_DIR", dir)
        .env("NC_BOARD_AS", actor)
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    let pid = child.id();
    let (exited, exit) = mpsc::channel();
    std::thread::spawn(move || exited.send(child.wait()));
    let status = loop {
        match exit.recv_timeout(renew_every) {
            Ok(status) => break status?,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("lost the command's exit status".into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let renewed = locked(dir, identity, runtime, async |board| {
            let op = Op::Renew {
                task: task.clone(),
                claim: claim.clone(),
                lease_ms,
            };
            board
                .submit(now_ms()?, op)
                .await
                .map(drop)
                .map_err(|error| error.to_string().into())
        });
        if let Err(error) = renewed {
            let _ = Command::new("kill").arg(pid.to_string()).status();
            let _ = exit.recv();
            return Err(format!("claim of {task} lost, command stopped: {error}").into());
        }
    };
    let code = status
        .code()
        .unwrap_or_else(|| status.signal().map_or(1, |signal| 128 + signal));
    let outcome = if status.success() {
        Outcome::Done(parsed.one("result")?.unwrap_or_else(|| "exit 0".to_owned()))
    } else {
        Outcome::Failed(format!("exit {code}"))
    };
    locked(dir, identity, runtime, async |board| {
        let op = Op::Finish {
            task: task.clone(),
            claim: claim.clone(),
            outcome,
        };
        submit(board, now_ms()?, op).await?;
        Ok(Some(
            json!({"task": shown(board, &task, now_ms()?)?, "exit_code": code}),
        ))
    })
}

fn waiting(
    parsed: &Parsed,
    dir: &Path,
    identity: &Identity,
    runtime: &nanocode_executor::Runtime,
) -> Result<Option<Value>, Failure> {
    parsed.allow(&["after", "label", "kind", "under", "timeout", "poll-ms"])?;
    parsed.words::<0>()?;
    let filter = parsed.filter()?;
    let timeout_ms = parsed
        .secs_ms("timeout")?
        .ok_or("--timeout SECS is required")?;
    let poll_ms = parsed
        .count("poll-ms")?
        .filter(|poll| *poll > 0)
        .ok_or("--poll-ms MS is required and positive")?;
    let deadline_ms = now_ms()?.saturating_add(timeout_ms);
    let mut after = parsed.count("after")?;
    loop {
        let lock = open(dir)?;
        let pass = runtime.block_on(async {
            let mut board = opened(dir, identity.clone()).await?;
            let from = after.unwrap_or_else(|| board.fold().next().saturating_sub(1));
            board
                .fire_due(now_ms()?)
                .await
                .map_err(|error| error.to_string())?;
            Ok::<_, Failure>((from, changes(&board, from, &filter)))
        });
        drop(lock);
        let (from, found) = pass?;
        if !found.is_empty() {
            return Ok(Some(Value::Array(found)));
        }
        after = Some(from);
        let now = now_ms()?;
        if now >= deadline_ms {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(poll_ms.min(deadline_ms - now)));
    }
}

fn session(parsed: &Parsed) -> Result<Option<Value>, Failure> {
    let dir = PathBuf::from(parsed.setting("dir", "NC_BOARD_DIR")?);
    let actor = match parsed.words[0].as_str() {
        "list" | "show" | "events" => parsed
            .setting("as", "NC_BOARD_AS")
            .unwrap_or_else(|_| "agent:reader".to_owned()),
        _ => parsed.setting("as", "NC_BOARD_AS")?,
    };
    let identity = identity(&actor)?;
    let runtime = nanocode_executor::Runtime::new()?;
    match parsed.words[0].as_str() {
        "wait" => return waiting(parsed, &dir, &identity, &runtime),
        "run" => return running(parsed, &dir, &actor, &identity, &runtime),
        "claim-next" if parsed.flags.contains_key("wait") => {
            return pulling(parsed, &dir, &identity, &runtime);
        }
        _ => {}
    }
    let lock = open(&dir)?;
    let result = runtime.block_on(async {
        let mut board = opened(&dir, identity).await?;
        let reader = matches!(parsed.words[0].as_str(), "list" | "show" | "events");
        if reader && board.fold().access().owner().is_some() && !board.may("*", Can::Read) {
            return Err(format!("{actor} may not read this board").into());
        }
        run(parsed, &mut board).await
    });
    drop(lock);
    result
}

pub fn main() -> ExitCode {
    let parsed = match Parsed::parse(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("nc-board: {error}");
            return ExitCode::FAILURE;
        }
    };
    if parsed.words.first().is_none_or(|word| word == "help") {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match session(&parsed) {
        Ok(Some(value)) => {
            println!("{value}");
            match value.get("exit_code").and_then(Value::as_i64) {
                Some(0) | None => ExitCode::SUCCESS,
                Some(code) => u8::try_from(code).map_or(ExitCode::FAILURE, ExitCode::from),
            }
        }
        Ok(None) if parsed.words[0] == "wait" => {
            eprintln!("nc-board: no change matched before the timeout");
            ExitCode::from(2)
        }
        Ok(None) => {
            eprintln!("nc-board: no ready task matches");
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("nc-board: {error}");
            ExitCode::FAILURE
        }
    }
}
