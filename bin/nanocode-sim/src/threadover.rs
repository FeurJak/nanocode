use std::{ops::Range, sync::Arc};

use nanocode::{
    backend::Prompt,
    session_host::{Submission, Submitted},
    turnlog::{OneLog, Policy},
};
use nanocode_turnlog::Record;
use serde_json::json;

use crate::rng::Rng;
use crate::threadscope::{ThreadScopeStats, World, driven, flag, publish, raw};

const THREAD: &str = "over";
const TURNS: Range<u64> = 2..10;

fn foreign(world: &World, node: &crate::threadscope::Node, at: u64) -> Option<()> {
    let log = match driven(world, "a thread get", node.logs.get(THREAD))? {
        Ok(log) => log,
        Err(error) => {
            flag(&world.wrong, format!("over: the thread is reached: {error}"));
            return None;
        }
    };
    let (Some(state), Some(next)) = (log.state(), log.next()) else {
        flag(&world.wrong, "over: the thread is not local".to_owned());
        return None;
    };
    let record = Record::Event {
        at,
        event: raw(&json!({ "type": "foreign", "at": at })),
        seq: state.next_event(),
        turn: None,
    };
    if let Err(error) = world.foreign(THREAD, next, &[record]) {
        flag(&world.wrong, format!("over: a cross-path write lands: {error}"));
        return None;
    }
    Some(())
}

pub(crate) fn run(rng: &mut Rng, world: &World, stats: &mut ThreadScopeStats) {
    let node = world.node("over0");
    publish(world, std::slice::from_ref(&node));
    let logs = Arc::new(node.logs.clone());
    let Some(opened) = driven(world, "a thread over", OneLog::over(logs, THREAD, &Policy::default()))
    else {
        return;
    };
    let over = match opened {
        Ok(over) => over,
        Err(error) => {
            flag(&world.wrong, format!("over: the thread opens: {error}"));
            return;
        }
    };
    let mut submitted = Vec::new();
    for turn in 0..rng.range(TURNS.start, TURNS.end) {
        let crossed = rng.chance(500_000);
        if crossed {
            stats.over_foreign += 1;
            if foreign(world, &node, turn).is_none() {
                return;
            }
        }
        let id = format!("o{turn}");
        let submission = Submission {
            request_id: id.clone(),
            prompt: Prompt::new(format!("turn {turn}")),
            hand: None,
            metadata: raw(&json!(null)),
        };
        let Some(answer) = driven(world, "a submit", over.submit(&submission)) else {
            return;
        };
        match answer {
            Ok(Submitted::Accepted { .. }) => submitted.push(id),
            answer => {
                flag(
                    &world.wrong,
                    format!(
                        "over: turn {id} submitted {} a cross-path write answers {answer:?}",
                        if crossed { "after" } else { "without" }
                    ),
                );
                return;
            }
        }
    }
    let Some(pending) = driven(world, "a pending read", over.pending()) else {
        return;
    };
    let held = match pending {
        Ok(pending) => pending.into_iter().map(|turn| turn.request_id).collect::<Vec<_>>(),
        Err(error) => {
            flag(&world.wrong, format!("over: pending answers {error}"));
            return;
        }
    };
    if held != submitted {
        flag(
            &world.wrong,
            format!("over: the thread holds pending turns {held:?}, but {submitted:?} were accepted"),
        );
    }
}
