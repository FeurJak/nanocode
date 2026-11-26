use std::{collections::BTreeMap, ops::Range, sync::Arc};

use nanocode::{
    backend::Prompt,
    session_host::{Submission, Submitted},
    turnlog::{OneLog, OneLogOver, Policy, ThreadJournals, ThreadStore},
};
use nanocode_code_mode::code_mode::{
    CELL_JOURNAL_VERSION, CellJournalHeader, CellJournalUpdate, JournalEntry, JournaledCall,
    JournaledCallState,
};
use nanocode_durability_core::{OwnerId, StateStore};
use nanocode_turnlog::{EffectClass, Record};
use serde_json::json;

use crate::rng::Rng;
use crate::threadscope::{ThreadScopeStats, World, driven, flag, publish, raw};

const THREAD: &str = "over";
const TURNS: Range<u64> = 2..10;

fn foreign(world: &World, node: &crate::threadscope::Node, at: u64) -> Option<()> {
    let log = match driven(world, "a thread get", node.logs.get(THREAD))? {
        Ok(log) => log,
        Err(error) => {
            flag(
                &world.wrong,
                format!("over: the thread is reached: {error}"),
            );
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
        flag(
            &world.wrong,
            format!("over: a cross-path write lands: {error}"),
        );
        return None;
    }
    Some(())
}

const STATE: &str = "over-state";
const SESSION: &str = "over-session";
const TOOL: &str = "over-deploy";

fn store_crossed(world: &World, node: &crate::threadscope::Node, mut store: ThreadStore, at: u64) {
    store.settle_durably();
    let owner = OwnerId::from("over".to_owned());
    let acquired = match driven(world, "an acquire", store.acquire(STATE, owner)) {
        Some(Ok(acquired)) => acquired,
        Some(Err(error)) => {
            flag(
                &world.wrong,
                format!("over: a handed-out store acquires: {error}"),
            );
            return;
        }
        None => return,
    };
    if foreign(world, node, at).is_none() {
        return;
    }
    let revision = acquired.state.revision;
    let replaced = driven(
        world,
        "a replace",
        store.replace(STATE, &acquired.owner, revision, "crossed", &[]),
    );
    match replaced {
        Some(Ok(landed)) if landed == revision + 1 => {}
        Some(answer) => flag(
            &world.wrong,
            format!(
                "over: a handed-out store replaces at {revision} after a cross-path write and answers {answer:?}"
            ),
        ),
        None => {}
    }
}

fn update(cell: u64) -> CellJournalUpdate {
    let call = JournaledCall {
        id: 0,
        call_id: format!("over-call{cell}"),
        name: TOOL.to_owned(),
        input: json!({ "cell": cell }),
        host: false,
        state: JournaledCallState::Running,
    };
    CellJournalUpdate {
        session_id: SESSION.to_owned(),
        cell_id: cell,
        seq: 0,
        header: Some(CellJournalHeader {
            version: CELL_JOURNAL_VERSION,
            origin_call_id: format!("over-origin{cell}"),
            script: format!("script {cell}"),
            tools: [json!({ "name": TOOL })].into(),
            stored: BTreeMap::new(),
            started_at_ms: None,
            prelude: false,
            yield_time_ms: 0,
            max_output_tokens: None,
        }),
        entries_from: 0,
        entries: vec![JournalEntry::Call {
            id: 0,
            name: TOOL.to_owned(),
            input: json!({ "cell": cell }),
            host: false,
        }],
        calls: vec![Arc::new(call)],
        observer_call_id: None,
        observer_yield_ms: None,
        observer_since_ms: None,
        content_returned: 0,
        finished: false,
    }
}

fn journals_crossed(
    world: &World,
    node: &crate::threadscope::Node,
    over: &OneLogOver,
    journals: &ThreadJournals,
    at: u64,
) {
    over.declare(TOOL, EffectClass::External);
    if foreign(world, node, at).is_none() {
        return;
    }
    let sink = journals.sink();
    match driven(world, "a save", sink.save(update(at))) {
        Some(Ok(())) => {}
        Some(Err(error)) => {
            flag(
                &world.wrong,
                format!("over: a handed-out journal saves after a cross-path write: {error}"),
            );
            return;
        }
        None => return,
    }
    match driven(world, "a cell load", sink.load_cell(SESSION, at)) {
        Some(Ok(Some(_))) | None => {}
        Some(answer) => flag(
            &world.wrong,
            format!("over: a handed-out journal's saved cell {at} loads as {answer:?}"),
        ),
    }
}

pub(crate) fn run(rng: &mut Rng, world: &World, stats: &mut ThreadScopeStats) {
    let node = world.node("over0");
    publish(world, std::slice::from_ref(&node));
    let logs = Arc::new(node.logs.clone());
    let Some(opened) = driven(
        world,
        "a thread over",
        OneLog::over(logs, THREAD, &Policy::default()),
    ) else {
        return;
    };
    let over = match opened {
        Ok(over) => over,
        Err(error) => {
            flag(&world.wrong, format!("over: the thread opens: {error}"));
            return;
        }
    };
    let (store, journals) = (over.store(), over.journals());
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
        Ok(pending) => pending
            .into_iter()
            .map(|turn| turn.request_id)
            .collect::<Vec<_>>(),
        Err(error) => {
            flag(&world.wrong, format!("over: pending answers {error}"));
            return;
        }
    };
    if held != submitted {
        flag(
            &world.wrong,
            format!(
                "over: the thread holds pending turns {held:?}, but {submitted:?} were accepted"
            ),
        );
    }
    let at = TURNS.end;
    stats.over_foreign += 2;
    store_crossed(world, &node, store, at);
    journals_crossed(world, &node, &over, &journals, at + 1);
}
