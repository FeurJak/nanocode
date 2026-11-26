use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use nanocode::{
    ClaudeClient, Nanocode, PromptRequest, Thinking, Tools,
    backend::Prompt,
    claude::transport::{HttpError, HttpFuture, HttpRequest, HttpResponse, HttpTransport},
};
use nanocode_agent_core::events::AgentEventKind;
use nanocode_durability_core::KeyValueStore;
use nanocode_sync::Atom;
use serde_json::{Value, json};

use crate::{
    executor::{Clock, Ended},
    faults::{Durability, Profile},
    rng::Rng,
    steercarry::{epoch, event_stream},
    store::{Mutation as StoreMutation, SimKv},
};

const SESSION: &str = "0190f5a2-7c1e-7000-8000-00000057ee13";
const TOOL: &str = "hold";
const CLAUDE_MODEL: &str = "claude-haiku-5-5";
const BASE_URL: &str = "http://model.sim/v1";
const REQUEST: &str = "turn-order";
const STEER_ID: &str = "steer-order";
const STEER_TEXT: &str = "ORDERMARK use the second ledger";
const STEER_MARK: &str = "ORDERMARK";
const CALLS_MAX: usize = 4;
const POLL: Duration = Duration::from_millis(5);

#[derive(Default)]
struct Wire {
    bodies: Atom<Vec<String>>,
    calls: AtomicU64,
    tool_calls: AtomicUsize,
    entered: AtomicUsize,
    released: AtomicUsize,
}

struct Hold(Arc<Wire>);

impl nanocode::FunctionHandler for Hold {
    fn call<'a>(
        &'a self,
        _name: &'a str,
        _input: Value,
        _context: nanocode::tools::ToolContext<'a>,
    ) -> nanocode::HostFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            let number = self.0.entered.fetch_add(1, Ordering::AcqRel);
            while self.0.released.load(Ordering::Acquire) <= number {
                nanocode_platform::sleep(POLL).await;
            }
            Ok(Value::String(format!("held {number}")))
        })
    }
}

struct ClaudeWire(Arc<Wire>);

impl HttpTransport for ClaudeWire {
    fn post(&self, request: HttpRequest) -> HttpFuture<'_, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let number = self.0.calls.fetch_add(1, Ordering::AcqRel) + 1;
            self.0
                .bodies
                .update(|bodies| bodies.push(request.body.clone()));
            drop(request);
            let sse = if number == 1 {
                tool_response(number, self.0.tool_calls.load(Ordering::Acquire))
            } else {
                crate::server::events(
                    CLAUDE_MODEL,
                    &format!("order{number}"),
                    &json!({"type": "text", "text": ""}),
                    Some(json!({"type": "text_delta", "text": "done"})),
                    "end_turn",
                )
            };
            Ok(HttpResponse::new(
                nanocode_http::types::StatusCode::OK,
                event_stream(),
                Box::pin(nanocode_sync::stream::once(
                    async move { Ok(sse.into_bytes()) },
                )),
            ))
        })
    }
}

fn call_id(number: u64, position: usize) -> String {
    format!("toolu_order{number}x{position}")
}

fn tool_response(number: u64, calls: usize) -> String {
    let mut list = vec![(
        "message_start",
        json!({"type":"message_start","message":{"id":format!("msg_order{number}"),"type":"message","role":"assistant","model":CLAUDE_MODEL,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":0}}}),
    )];
    for position in 0..calls {
        list.push((
            "content_block_start",
            json!({"type":"content_block_start","index":position,"content_block":{"type":"tool_use","id":call_id(number, position),"name":TOOL,"input":{}}}),
        ));
        list.push((
            "content_block_delta",
            json!({"type":"content_block_delta","index":position,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
        ));
        list.push((
            "content_block_stop",
            json!({"type":"content_block_stop","index":position}),
        ));
    }
    list.push((
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":1}}),
    ));
    list.push(("message_stop", json!({"type":"message_stop"})));
    list.into_iter()
        .map(|(name, event)| format!("event: {name}\ndata: {event}\n\n"))
        .collect()
}

fn runtime(wire: &Arc<Wire>, kv: SimKv) -> Result<Nanocode, String> {
    let tools = Tools::builder()
        .without_defaults()
        .tool(nanocode::FunctionTool::new(
            TOOL,
            "Wait until the operator releases the hold",
            json!({"type": "object", "properties": {}}),
            Arc::new(Hold(Arc::clone(wire))),
        ))
        .build()
        .map_err(|error| format!("tools: {error}"))?;
    let client = ClaudeClient::messages_api(
        BASE_URL,
        "sk-sim",
        Duration::from_secs(10),
        Duration::from_secs(600),
    )
    .map_err(|error| format!("claude client: {error}"))?
    .with_transport(ClaudeWire(Arc::clone(wire)));
    let store = nanocode::session_store(KeyValueStore::new(kv))
        .map_err(|error| format!("session store: {error}"))?;
    Nanocode::builder()
        .tools(tools)
        .runner_management(false)
        .thinking(Thinking::Low)
        .workspace(std::env::temp_dir())
        .durability(store)
        .claude(nanocode::ClaudeHarness::new(client).direct_tools_for(|_| true))
        .build()
        .map_err(|error| format!("runtime build: {error}"))
}

struct Seen {
    kind: AgentEventKind,
    payload: Value,
}

async fn turn(wire: &Arc<Wire>, kv: SimKv, steer_at: usize) -> Result<Vec<Seen>, String> {
    let runtime = runtime(wire, kv)?;
    let (agent, mut events) = runtime
        .spawn()
        .session_id(SESSION)
        .backend::<nanocode::Claude>()
        .model(nanocode::ClaudeModel::Haiku55)
        .build()
        .await
        .map_err(|error| format!("open: {error}"))?;
    let turn = agent
        .prompt(PromptRequest::new("ORDERTURN check both ledgers").request_id(REQUEST))
        .await
        .map_err(|error| format!("turn: {error}"))?;
    for call in 0..wire.tool_calls.load(Ordering::Acquire) {
        while wire.entered.load(Ordering::Acquire) <= call {
            nanocode_platform::sleep(POLL).await;
        }
        if call == steer_at {
            turn.steer_with_id(STEER_ID.to_owned(), Prompt::new(STEER_TEXT))
                .await
                .map_err(|error| format!("steer: {error}"))?;
        }
        wire.released.store(call + 1, Ordering::Release);
    }
    turn.result()
        .await
        .map_err(|error| format!("turn result: {error}"))?;
    let mut seen = Vec::new();
    while let Some(event) = events.try_recv() {
        seen.push(Seen {
            kind: event.kind,
            payload: serde_json::from_str(event.payload.get()).unwrap_or(Value::Null),
        });
    }
    Ok(seen)
}

fn text<'a>(payload: &'a Value, field: &str) -> Option<&'a str> {
    payload.get(field).and_then(Value::as_str)
}

fn ordered(seen: &[Seen]) -> Vec<String> {
    let mut wrong = Vec::new();
    let mut unread = BTreeSet::new();
    let mut due = BTreeSet::new();
    for event in seen {
        match event.kind {
            AgentEventKind::InputAccepted if text(&event.payload, "kind") == Some("steer") => {
                unread.extend(text(&event.payload, "request_id").map(str::to_owned));
            }
            AgentEventKind::ToolResult => due.extend(unread.iter().cloned()),
            AgentEventKind::RunSteered => {
                if let Some(id) = text(&event.payload, "message_id") {
                    unread.remove(id);
                    due.remove(id);
                }
            }
            AgentEventKind::ToolCall if !due.is_empty() => wrong.push(format!(
                "tool call {:?} started while steers {due:?}, accepted before an earlier call returned, were unread",
                text(&event.payload, "call_id")
            )),
            _ => {}
        }
    }
    if !unread.is_empty() {
        wrong.push(format!("steers {unread:?} were accepted and never read"));
    }
    wrong
}

fn placed(bodies: &[String], calls: usize) -> Vec<String> {
    let mut wrong = Vec::new();
    if bodies.len() != 2 {
        wrong.push(format!("{} model requests, not 2", bodies.len()));
        return wrong;
    }
    if bodies[0].contains(STEER_MARK) {
        wrong.push("the first model request held the steer".to_owned());
    }
    let held = bodies[1].matches(STEER_MARK).count();
    if held != 1 {
        wrong.push(format!(
            "the second model request held the steer {held} times"
        ));
    }
    let last = call_id(1, calls - 1);
    match (bodies[1].rfind(&last), bodies[1].find(STEER_MARK)) {
        (Some(result), Some(steer)) if result < steer => {}
        (result, steer) => wrong.push(format!(
            "the steer at {steer:?} does not follow the last tool result at {result:?}"
        )),
    }
    wrong
}

pub fn check(seed: u64, clock: &Arc<Clock>) -> Result<u64, String> {
    let mut rng = Rng::derive(seed, "steer_order");
    let calls = 2 + rng.pick(CALLS_MAX - 1);
    let steer_at = rng.pick(calls);
    let wire = Arc::new(Wire::default());
    wire.tool_calls.store(calls, Ordering::Release);
    let mut profile = Profile::of(seed);
    profile.before_ppm = 0;
    profile.after_ppm = 0;
    profile.torn_ppm = 0;
    profile.read_ppm = 0;
    profile.durability = Durability::OnAck;
    let kv = SimKv::new(seed, profile, StoreMutation::None, false, Arc::clone(clock));
    let label = format!("steer_order calls={calls} steer_at={steer_at}");
    let (ended, result) = epoch(seed, clock, &label, turn(&wire, kv, steer_at));
    let seen = match (ended, result) {
        (Ended::Finished, Some(Ok(seen))) => seen,
        (Ended::Finished, Some(Err(error))) => return Err(format!("{label}: {error}")),
        (ended, _) => return Err(format!("{label}: ended {ended:?} without a result")),
    };
    let mut wrong = ordered(&seen);
    wrong.extend(placed(&wire.bodies.get(), calls));
    if wrong.is_empty() {
        Ok(wire.calls.load(Ordering::Acquire))
    } else {
        Err(format!("{label}: {}", wrong.join("; ")))
    }
}
