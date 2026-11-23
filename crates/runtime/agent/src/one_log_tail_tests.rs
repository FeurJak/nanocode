use std::future::Future;
use std::task::{Context, Poll, Waker};

use nanocode_turnlog::{MemoryLog, rows};

use super::*;

const SCOPE: &str = "tail-bound";
const STATE: &str = "agent";
const TURNS: usize = 1_000;
const RESUMED: usize = 400;
const BOUND: usize = 512 * 1024;
const RESUME_BOUND: usize = 64 * 1024;
const REOPEN_EVERY: usize = 7;
const BODY_BYTES: usize = 4 * 1024;
const MESSAGE_BYTES: usize = 3 * 1024;
const PAGE_MESSAGES: usize = 32;
const BOUNDED_FROM: usize = 250;

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}

fn prose(i: usize, what: &str, bytes: usize) -> String {
    let mut text = String::with_capacity(bytes + 96);
    let mut line = 0;
    while text.len() < bytes {
        text.push_str(&format!(
            "{what} {i}, line {line}: the mock model answers the way it always does. "
        ));
        line += 1;
    }
    text
}

fn hash(kind: u64, i: usize) -> String {
    format!("b3:{:064x}", (kind << 48) + i as u64)
}

fn inline(value: &Value) -> Arc<str> {
    Arc::from(format!("={value}"))
}

fn records(i: usize) -> Vec<StoreRecord> {
    let page = i / PAGE_MESSAGES;
    let page_keys: Vec<String> = (page * PAGE_MESSAGES..=i).map(|j| hash(1, j)).collect();
    let pages: Vec<String> = (0..page)
        .map(|p| hash(2, p * PAGE_MESSAGES + PAGE_MESSAGES - 1))
        .chain(std::iter::once(hash(2, i)))
        .collect();
    vec![
        StoreRecord {
            key: hash(1, i),
            value: inline(
                &json!({"role": "assistant", "text": prose(i, "message", MESSAGE_BYTES)}),
            ),
        },
        StoreRecord {
            key: hash(2, i),
            value: inline(&json!({"messages": page_keys})),
        },
        StoreRecord {
            key: hash(3, i),
            value: inline(&json!({"pages": pages})),
        },
    ]
}

fn payload(i: usize) -> String {
    json!({"turn": i, "model": "c0", "history": hash(3, i)}).to_string()
}

fn policy(max: Option<usize>) -> Policy {
    Policy {
        restarts: u64::MAX,
        retain_finished: Some(16),
        retired_max: Some(1_000_000),
        snapshot_ratio: 2,
        max_tail_bytes: max,
        ..Policy::default()
    }
}

async fn open(host: &Arc<MemoryLog>, max: Option<usize>) -> (Arc<OneLog>, OneLogStore, OwnerToken) {
    let host: Arc<dyn LogHost> = host.clone();
    let log = OneLog::open(host, SCOPE, policy(max)).await.expect("opens");
    let mut store = log.store();
    let owner = store
        .acquire(STATE, OwnerId::new())
        .await
        .expect("acquires")
        .owner;
    (log, store, owner)
}

async fn turn(log: &OneLog, store: &mut OneLogStore, owner: &OwnerToken, i: usize) -> u64 {
    let before = log.stats().await.snapshots;
    let request = format!("turn-{i}");
    let submission = Submission {
        request_id: request.clone(),
        prompt: Prompt::new(prose(i, "question", BODY_BYTES)),
        hand: None,
        metadata: RawValue::from_string(format!("{{\"seq\":{i}}}")).expect("metadata"),
    };
    match log.submit(&submission).await.expect("submits") {
        Submitted::Accepted { .. } => {}
        other => panic!("turn {i} was not new: {other:?}"),
    }
    log.start(&request).await.expect("starts").expect("runs");
    store.collect_unreachable(STATE).await.expect("marks");
    if i > 0 {
        let held: Vec<String> = (0..i)
            .map(|j| hash(1, j))
            .chain((0..i).map(|j| hash(2, j)))
            .chain(std::iter::once(hash(3, i - 1)))
            .collect();
        store.read_records(STATE, &held).await.expect("reads");
    }
    let records = records(i);
    store
        .replace(STATE, owner, i as u64, &payload(i), &records)
        .await
        .expect("replaces");
    let result = Map::from_iter([(
        "text".to_owned(),
        Value::String(prose(i, "answer", BODY_BYTES)),
    )]);
    log.finish(&request, TurnState::Completed, &result)
        .await
        .expect("finishes")
        .expect("was running");
    log.stats().await.snapshots - before
}

struct Tail {
    rows: usize,
    plain: usize,
}

async fn tail(host: &Arc<MemoryLog>) -> Tail {
    let stored = host.load(SCOPE).await.expect("loads");
    let decoded = rows(&stored).expect("decodes");
    let last = decoded.iter().rposition(|row| row.snapshot);
    let after = last.map_or(&decoded[..], |at| &decoded[at + 1..]);
    Tail {
        rows: after.len(),
        plain: after.iter().map(|row| row.plain.len()).sum(),
    }
}

async fn held(log: &OneLog) -> usize {
    log.read(|fold| {
        let checkpoints: usize = fold
            .checkpoints()
            .map(|(state, checkpoint)| state.len() + checkpoint.payload.len())
            .sum();
        let events: usize = fold
            .events()
            .iter()
            .map(|event| event.event.get().len())
            .sum();
        fold.stored_bytes() + checkpoints + events
    })
    .await
}

async fn resumed(log: &Arc<OneLog>) -> (StoredState, Vec<(String, Option<String>)>) {
    let mut store = log.store();
    let state = store
        .acquire(STATE, OwnerId::new())
        .await
        .expect("acquires")
        .state;
    let mut keys = Vec::new();
    let mut queue: Vec<String> = state
        .payload
        .as_deref()
        .map(|payload| {
            crate::durability::EncodedPayload::references(payload)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let mut reached = Vec::new();
    while let Some(key) = queue.pop() {
        if keys.contains(&key) {
            continue;
        }
        keys.push(key.clone());
        let value = store.read_record(STATE, &key).await.expect("reads");
        if let Some(value) = &value {
            queue.extend(crate::durability::EncodedPayload::references(value).map(str::to_owned));
        }
        reached.push((key, value));
    }
    reached.sort();
    (state, reached)
}

#[test]
fn a_reopened_log_holds_a_bounded_tail_as_its_state_grows() {
    block_on(async {
        let host = Arc::new(MemoryLog::new());
        let mut worst = (0, 0, 0);
        let mut opened = open(&host, Some(BOUND)).await;
        let mut written = Vec::new();
        let mut snapshots = 0;
        let mut collected = 0;
        for i in 0..TURNS {
            if i % REOPEN_EVERY == 0 {
                opened = open(&host, Some(BOUND)).await;
            }
            let (log, store, owner) = &mut opened;
            let before = host.writes().bytes;
            let snapshotted = turn(log, store, owner, i).await;
            snapshots += snapshotted;
            written.push(host.writes().bytes - before);
            let Tail { rows, plain, .. } = tail(&host).await;
            let (reopened, mut store, _) = open(&host, Some(BOUND)).await;
            if snapshotted > 0
                && i >= BOUNDED_FROM
                && store
                    .read_record(STATE, &hash(3, i - 1))
                    .await
                    .expect("reads")
                    .is_none()
            {
                collected += 1;
            }
            let held = held(&reopened).await;
            worst = (worst.0.max(rows), worst.1.max(plain), worst.2.max(held));
            assert!(
                plain < BOUND + 2 * BODY_BYTES + 4 * MESSAGE_BYTES,
                "turn {i}: a reopen reads {plain} plain tail bytes in {rows} rows, over {BOUND}"
            );
            assert!(
                held < 2 * BOUND,
                "turn {i}: a reopened log holds {held} bytes"
            );
        }
        let late: u64 = written[TURNS - 100..].iter().sum::<u64>() / 100;
        let total: u64 = written.iter().sum();
        eprintln!(
            "tail bound {BOUND}: {TURNS} turns, {snapshots} snapshots, {total} bytes written, {late} a turn over the last 100, worst tail {} rows {} plain, worst held {}",
            worst.0, worst.1, worst.2
        );
        let plain_total = (TURNS * (2 * BODY_BYTES + MESSAGE_BYTES)) as u64;
        assert!(
            snapshots <= 2 * plain_total / BOUND as u64 + 16,
            "{snapshots} snapshots in {TURNS} turns"
        );
        assert!(
            collected > 0,
            "no snapshot the bound forced collected the unreachable history"
        );
    });
}

#[test]
fn a_bounded_log_resumes_the_same_state_as_an_unbounded_one() {
    block_on(async {
        let bounded = Arc::new(MemoryLog::new());
        let unbounded = Arc::new(MemoryLog::new());
        let mut left = open(&bounded, Some(RESUME_BOUND)).await;
        let mut right = open(&unbounded, None).await;
        let mut mid_tail = 0;
        let mut forced = 0;
        let mut taken = 0;
        for i in 0..RESUMED {
            if i % REOPEN_EVERY == 0 {
                left = open(&bounded, Some(RESUME_BOUND)).await;
                right = open(&unbounded, None).await;
            }
            forced += turn(&left.0, &mut left.1, &left.2, i).await;
            taken += turn(&right.0, &mut right.1, &right.2, i).await;
            if tail(&bounded).await.rows > 0 {
                mid_tail += 1;
            }
            let a = open(&bounded, Some(RESUME_BOUND)).await.0;
            let b = open(&unbounded, None).await.0;
            assert_eq!(
                resumed(&a).await,
                resumed(&b).await,
                "turn {i}: the bounded log resumes another state"
            );
            let turns = |log: &Arc<OneLog>| {
                let log = Arc::clone(log);
                async move {
                    log.read(|fold| (fold.retired_turns(), fold.turns().len()))
                        .await
                }
            };
            assert_eq!(
                turns(&a).await,
                turns(&b).await,
                "turn {i}: the bounded log resumes other turns"
            );
        }
        assert!(mid_tail > 0, "no reopen landed mid-tail");
        assert!(
            forced > taken,
            "the bound forced no snapshot: {forced} bounded, {taken} unbounded"
        );
    });
}
