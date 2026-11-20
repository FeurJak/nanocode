use std::collections::{BTreeMap, BTreeSet};

use nanocode_board::{
    ClaimId, Event, EventId, Fold, NewTask, Op, Outcome, TaskId,
    access::{Can, Grant, Who},
    tasks::Terms,
};

use crate::{Checked, rng::Rng};

const ACTORS: [&str; 5] = ["user:a", "user:b", "agent:c", "user:a+p", "bad actor"];
const TASKS: [&str; 6] = ["t0", "t1", "t2", "t0/x", "t1/y", "z"];
const CHANNELS: [&str; 4] = ["*", "t0", "t1/", "z"];
const KEYS: [&str; 3] = ["k0", "k1", "k2"];

fn task(rng: &mut Rng) -> TaskId {
    TaskId(rng.word(&TASKS))
}

fn claim(rng: &mut Rng, ids: &[EventId]) -> ClaimId {
    ClaimId(if ids.is_empty() || rng.chance(1, 8) {
        rng.text(6)
    } else {
        rng.pick(ids).0.clone()
    })
}

fn op(rng: &mut Rng, ids: &[EventId]) -> Op {
    match rng.below(13) {
        0 => Op::Charter {},
        1 => Op::Grant {
            grant: Grant {
                who: (0..rng.range(1, 3))
                    .map(|_| match rng.below(3) {
                        0 => Who::Principal(rng.word(&ACTORS)),
                        1 => Who::Kind(rng.word(&["user", "agent", ""])),
                        _ => Who::Everyone,
                    })
                    .collect(),
                can: (0..rng.range(1, 3))
                    .map(|_| {
                        let can = (0..rng.range(1, 4)).map(|_| *rng.pick(&Can::ALL)).collect();
                        (rng.word(&CHANNELS), can)
                    })
                    .collect(),
            },
        },
        2 => Op::Revoke {
            grant: if ids.is_empty() {
                EventId(rng.text(6))
            } else {
                rng.pick(ids).clone()
            },
        },
        3 => Op::Post {
            task: NewTask {
                id: task(rng),
                parent: rng.chance(1, 3).then(|| task(rng)),
                spec: rng.text(8),
                labels: (0..rng.below(3))
                    .map(|_| (rng.word(&["k", "source"]), rng.text(3)))
                    .collect(),
                needs: (0..rng.below(2)).map(|_| task(rng)).collect(),
                terms: Terms::default(),
            },
        },
        4 => Op::Claim {
            task: task(rng),
            lease_ms: rng.below(1_000),
            fence: None,
        },
        5 => Op::Renew {
            task: task(rng),
            claim: claim(rng, ids),
            lease_ms: rng.below(1_000),
        },
        6 => Op::Release {
            task: task(rng),
            claim: claim(rng, ids),
            note: rng.text(4),
        },
        7 => Op::Finish {
            task: task(rng),
            claim: claim(rng, ids),
            outcome: if rng.chance(1, 2) {
                Outcome::Done(rng.text(4))
            } else {
                Outcome::Failed(rng.text(4))
            },
        },
        8 => Op::Cancel {
            task: task(rng),
            reason: rng.text(4),
        },
        9 => Op::Reply {
            task: task(rng),
            body: rng.text(8),
        },
        10 => Op::Relabel {
            task: task(rng),
            set: (0..rng.below(3))
                .map(|_| (rng.word(&["k", "j"]), rng.text(3)))
                .collect::<BTreeMap<_, _>>(),
            unset: (0..rng.below(2))
                .map(|_| rng.word(&["k", "j"]))
                .collect::<BTreeSet<_>>(),
        },
        11 => Op::Move {
            task: task(rng),
            parent: rng.chance(1, 2).then(|| task(rng)),
        },
        _ => Op::Rank {
            task: task(rng),
            rank: rng.chance(2, 3).then(|| rng.below(10)),
        },
    }
}

fn rows(rng: &mut Rng) -> Vec<(u64, String)> {
    let mut rows = Vec::new();
    let mut ids: Vec<EventId> = Vec::new();
    let mut minted = 0;
    let mut next = 1;
    let mut at_ms = rng.below(1_000);
    let founder = rng.word(&ACTORS[..3]);
    for step in 0..rng.range(1, 40) {
        at_ms += rng.below(200);
        let by = rng.word(&ACTORS);
        minted += 1;
        let owner = if rng.chance(1, 10) {
            rng.word(&ACTORS)
        } else {
            by.clone()
        };
        let id = match rng.below(8) {
            0 => EventId(format!("{owner}#{}", rng.word(&KEYS))),
            1 if !ids.is_empty() => rng.pick(&ids).clone(),
            _ => EventId(format!("{owner}.{minted}")),
        };
        let op = if step == 0 && rng.chance(7, 8) {
            Op::Charter {}
        } else {
            op(rng, &ids)
        };
        let by = if step == 0 { founder.clone() } else { by };
        let body = serde_json::to_string(&Event {
            id: id.clone(),
            by,
            at_ms,
            op,
        })
        .expect("an event serializes");
        ids.push(id);
        let (seq, body) = match rng.below(10) {
            0 => (next, rng.mutate(&body)),
            1 => (rng.below(next + 3), body),
            _ => (next, body),
        };
        if seq == next {
            next += 1;
        }
        rows.push((seq, body));
    }
    rows
}

pub fn check(rng: &mut Rng) -> Checked {
    let rows = rows(rng);
    let mut fold = Fold::new();
    let mut accepted = Vec::new();
    for (seq, body) in &rows {
        let before = fold.clone();
        match fold.apply(*seq, body) {
            Ok(()) => {
                ensure!(
                    fold.next() == seq + 1,
                    "row {seq} left next at {}",
                    fold.next()
                );
                accepted.push((*seq, body.clone()));
            }
            Err(error) => ensure!(
                fold == before,
                "refused row {seq} ({error}) changed the fold: {body}"
            ),
        }
    }
    let mut again = Fold::new();
    for (seq, body) in &rows {
        let _ = again.apply(*seq, body);
    }
    ensure!(
        again == fold,
        "folding the same rows twice gave different folds"
    );
    let mut clean = Fold::new();
    for (seq, body) in &accepted {
        ensure!(
            clean.apply(*seq, body).is_ok(),
            "accepted row {seq} refused on replay"
        );
    }
    ensure!(
        clean == fold,
        "replaying only the accepted rows gave a different fold"
    );
    let snapshot = fold.snapshot();
    let restored =
        Fold::restore(&snapshot).map_err(|error| format!("snapshot refused: {error}"))?;
    ensure!(
        restored == fold,
        "a restored snapshot differs from its fold"
    );
    ensure!(
        restored.snapshot() == snapshot,
        "a restored snapshot serializes differently"
    );
    let split = rng.index(accepted.len() + 1);
    let mut head = Fold::new();
    for (seq, body) in &accepted[..split] {
        let _ = head.apply(*seq, body);
    }
    let mut resumed =
        Fold::restore(&head.snapshot()).map_err(|error| format!("snapshot refused: {error}"))?;
    for (seq, body) in &accepted[split..] {
        ensure!(
            resumed.apply(*seq, body).is_ok(),
            "row {seq} refused after resuming at {split}"
        );
    }
    ensure!(
        resumed == fold,
        "resuming from a snapshot after {split} rows gave a different fold"
    );
    let mangled = rng.mutate(&snapshot);
    if let Ok(other) = Fold::restore(&mangled) {
        ensure!(
            Fold::restore(&other.snapshot()).ok().as_ref() == Some(&other),
            "a mangled snapshot restored to a fold that does not round-trip"
        );
    }
    Ok(())
}
