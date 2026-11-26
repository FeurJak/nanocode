use std::{
    cell::RefCell,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use nanocode::{
    ClaudeClient, Nanocode, PromptRequest, Thinking, Tools,
    backend::Prompt,
    claude::transport::{HttpError, HttpFuture, HttpRequest, HttpResponse, HttpTransport},
};
use nanocode_durability_core::KeyValueStore;
use nanocode_sync::Atom;
use serde_json::json;

use crate::{
    executor::{Clock, Ended, Executor, Task},
    faults::{Durability, Profile},
    rng::{FNV_OFFSET, Rng, fnv},
    store::{Mutation as StoreMutation, SimKv},
};

const SCOPE: &str = "backend-facts-stream";
const SESSION: &str = "0190f5a2-7c1e-7000-8000-0000005a7e01";
const CLAUDE_MODEL: &str = "claude-haiku-5-5";
const BASE_URL: &str = "http://model.sim/v1";
const STEER_MARK: &str = "STREAMSTEER";
const POLL: Duration = Duration::from_millis(5);
const ANSWER_WITHIN: Duration = Duration::from_secs(5);
const CRASH_AFTER: Duration = Duration::from_secs(600);

#[derive(Default)]
struct Wire {
    bodies: Atom<Vec<String>>,
    entered: AtomicBool,
    released: AtomicBool,
    calls: AtomicU64,
}

struct SlowWire(Arc<Wire>);

impl HttpTransport for SlowWire {
    fn post(&self, request: HttpRequest) -> HttpFuture<'_, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let number = self.0.calls.fetch_add(1, Ordering::AcqRel) + 1;
            let body = String::from_utf8_lossy(request.body.as_bytes()).into_owned();
            self.0.bodies.update(|bodies| bodies.push(body.clone()));
            drop(request);
            let sse = crate::server::events(
                CLAUDE_MODEL,
                &format!("stream{number}"),
                &json!({"type": "text", "text": ""}),
                Some(json!({"type": "text_delta", "text": "done"})),
                "end_turn",
            );
            let wire = Arc::clone(&self.0);
            let slow = number == 1;
            Ok(HttpResponse::new(
                nanocode_http::types::StatusCode::OK,
                event_stream(),
                Box::pin(nanocode_sync::stream::once(async move {
                    if slow {
                        wire.entered.store(true, Ordering::Release);
                        while !wire.released.load(Ordering::Acquire) {
                            nanocode_platform::sleep(POLL).await;
                        }
                    }
                    Ok(sse.into_bytes())
                })),
            ))
        })
    }
}

fn event_stream() -> nanocode_http::types::HeaderMap {
    let mut headers = nanocode_http::types::HeaderMap::new();
    headers.insert(
        nanocode_http::types::header::CONTENT_TYPE,
        nanocode_http::types::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        "request-id",
        nanocode_http::types::HeaderValue::from_static("req_sim"),
    );
    headers
}

fn runtime(wire: &Arc<Wire>, kv: SimKv) -> Result<Nanocode, String> {
    let client = ClaudeClient::messages_api(
        BASE_URL,
        "sk-sim",
        Duration::from_secs(10),
        Duration::from_secs(600),
    )
    .map_err(|error| format!("claude client: {error}"))?
    .with_transport(SlowWire(Arc::clone(wire)));
    let tools = Tools::builder()
        .without_defaults()
        .build()
        .map_err(|error| format!("tools: {error}"))?;
    let store = nanocode::session_store(KeyValueStore::new(kv))
        .map_err(|error| format!("session store: {error}"))?;
    Nanocode::builder()
        .tools(tools)
        .runner_management(false)
        .thinking(Thinking::Low)
        .workspace(std::env::temp_dir())
        .durability(store)
        .claude(nanocode::ClaudeHarness::new(client))
        .build()
        .map_err(|error| format!("runtime build: {error}"))
}

async fn answered<T, E: std::fmt::Display>(
    what: &str,
    call: impl Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match nanocode_platform::timeout(ANSWER_WITHIN, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("{what} during a held model stream failed: {error}")),
        Err(_) => Err(format!(
            "{what} waited behind the model stream: no answer within {} ms while the stream was held",
            ANSWER_WITHIN.as_millis()
        )),
    }
}

async fn held(
    wire: &Wire,
    agent: &nanocode::Agent,
    turn: &nanocode::Turn,
    cancel: bool,
) -> Result<(), String> {
    while !wire.entered.load(Ordering::Acquire) {
        nanocode_platform::sleep(POLL).await;
    }
    answered("a checkpoint read", agent.checkpoint()).await?;
    answered(
        "a steer",
        turn.steer(Prompt::new(format!("{STEER_MARK} use the other ledger"))),
    )
    .await?;
    if cancel {
        answered("an interrupt", turn.cancel()).await?;
    }
    if wire.released.load(Ordering::Acquire) {
        return Err("the held model stream ended before the checks ran".into());
    }
    Ok(())
}

async fn check(wire: &Arc<Wire>, kv: SimKv, cancel: bool) -> Result<u64, String> {
    let runtime = runtime(wire, kv)?;
    let (agent, events) = runtime
        .spawn()
        .session_id(SESSION)
        .backend::<nanocode::Claude>()
        .model(nanocode::ClaudeModel::Haiku55)
        .build()
        .await
        .map_err(|error| format!("open: {error}"))?;
    drop(events);
    let turn = agent
        .prompt(PromptRequest::new("STREAMTURN look up the balance"))
        .await
        .map_err(|error| format!("turn: {error}"))?;
    let checked = held(wire, &agent, &turn, cancel).await;
    wire.released.store(true, Ordering::Release);
    let result = turn.result().await;
    checked?;
    if cancel {
        return Ok(wire.calls.load(Ordering::Acquire));
    }
    result.map_err(|error| format!("the turn after a held stream failed: {error}"))?;
    let bodies = wire.bodies.get();
    if !bodies.iter().skip(1).any(|body| body.contains(STEER_MARK)) {
        return Err(format!(
            "a steer accepted during the held stream reached none of the {} later model calls",
            bodies.len().saturating_sub(1)
        ));
    }
    Ok(wire.calls.load(Ordering::Acquire))
}

pub(super) fn run(seed: u64) -> Result<u64, String> {
    let cancel = Rng::derive(seed, SCOPE).chance(500_000);
    let clock = Clock::new();
    let wire = Arc::new(Wire::default());
    let result = RefCell::new(None);
    let mut profile = Profile::of(seed);
    profile.before_ppm = 0;
    profile.after_ppm = 0;
    profile.torn_ppm = 0;
    profile.read_ppm = 0;
    profile.durability = Durability::OnAck;
    let ended = crate::host::entered(seed, &clock, || {
        let kv = SimKv::new(
            seed,
            profile,
            StoreMutation::None,
            false,
            Arc::clone(&clock),
        );
        let mut executor = Executor::new(Arc::clone(&clock), Rng::derive(seed, SCOPE));
        let task: Task<'_> = Box::pin(async {
            result.replace(Some(check(&wire, kv, cancel).await));
        });
        let crash_at = clock.now_ns() + u64::try_from(CRASH_AFTER.as_nanos()).unwrap_or(u64::MAX);
        let ended = executor.run(vec![task], Some(crash_at));
        drop(executor);
        while !crate::host::spawned().is_empty() {}
        ended
    });
    match (ended, result.into_inner()) {
        (Ended::Finished, Some(Ok(calls))) => Ok(fnv(
            fnv(FNV_OFFSET, &calls.to_le_bytes()),
            &[u8::from(cancel)],
        )),
        (Ended::Finished, Some(Err(wrong))) => Err(wrong),
        (ended, _) => Err(format!(
            "the held-stream check ended {ended:?} without a result"
        )),
    }
}
