# Crate layout

| Directory | Responsibility |
| --- | --- |
| `nanocode/` | The public facade: re-exports and builders |
| `core/` | Values, contracts and plumbing every layer shares |
| `runtime/` | Backend-generic orchestration: agents, subagents, Code Mode, durable sessions, tool selection |
| `backends/<provider>/` | One tree per provider: `client` (wire protocol, authentication, transport), Claude's `tools`, and the driver crate (agent, prompts, tool assembly, durability format) |
| `integrations/` | Native host adapters: shells and processes, computer use, browsers, sandboxing, MCP, remote hands and runners, stores, telemetry |
| `tui/` | Terminal UI components |
| `../bin/nanocode/` | The `nanocode` / `nc` executable |
| `../bin/cua-driver/` | The macOS computer-use helper |

`scripts/check-dependency-boundaries.mjs` enforces the layering: `core/` depends on nothing above it, `runtime/` only on `core/`, `integrations/` on neither `backends/` nor the facade, and a backend on no other backend and no host implementation. Provider `client` crates stay free of runtime, tool-host, Code Mode and durability dependencies, and `nanocode-types-core` and `nanocode-agent-types` stay free of Tokio.

## core

| Crate | Owns |
| --- | --- |
| `nanocode-types-core` | SDK values and event wire formats |
| `nanocode-types` | Event channels, over `nanocode-types-core` |
| `nanocode-agent-types` | Model, usage and error values |
| `nanocode-cost-fold` | Cost attribution as a pure fold: tokens, dollars and Hand time per step, rolled up by agent, turn, board task and commit |
| `nanocode-live-fold` | Live views as a pure fold: the sync wire frames, views folded from the log into coalesced diffs, diff rings with resume, sealed projections encoded once per access level |
| `nanocode-partitions` | Partitioned ownership without a leader: stable partition hashing, rendezvous assignment over live nodes, fenced per-partition lease rows and the claim/release plan |
| `nanocode-hedge` | Hedged first-byte request policy and the primary-versus-hedge race |
| `nanocode-agent-core` | Agent lifecycle, handles and session contracts |
| `nanocode-durability-core` | `StateStore`, content-addressed payloads, the deterministic state machine, `SqlStore`, `RowStore`, `StagedStore`, `BlobStore` |
| `nanocode-workspace-fs` | A versioned virtual workspace: a manifest of content-addressed files changed all-or-nothing under caller caps, its stores, and the shell filesystem over it |
| `nanocode-tool-contracts`, `nanocode-tool-provider` | Tool invocation traits and dynamic tool providers |
| `nanocode-tools`, `nanocode-tools-macros` | The `#[tool]` macro and the crate its expansion names |
| `nanocode-process-contracts` | Portable shell and file host interfaces, `Redactor` |
| `nanocode-platform` | The host interface WASM embedders install |
| `nanocode-sync` | Lock-free primitives: `Atom`, `Mailbox`, `Actor`, `Slot`, `Signal`, `SingleFlight`, ... |
| `nanocode-machine` | `machine!`: a lifecycle declared once as states, wire names and moves, with its stored-state check and fence text |
| `nanocode-http`, `nanocode-zstd`, `nanocode-tls` | HTTP layer contracts and retry rules, a streaming zstd decoder, cached TLS configuration |
| `nanocode-mcp`, `nanocode-mcp-http` | The MCP client: portable session engine; native Streamable HTTP and OAuth |
| `nanocode-google-auth` | Google Cloud access tokens from Application Default Credentials: user, service account, impersonated and workload identity credentials, the metadata server, gcloud |
| `nanocode-aws` | AWS request signing and credentials: SigV4 for any service, the event-stream decoder (prelude and message CRC32 checked), environment keys, Bedrock bearer tokens, CLI profiles (SSO, assumed roles), ECS container and EC2 instance credentials, the region from the environment or the profile |
| `nanocode-schema` | JSON Schema validation for output contracts |
| `nanocode-home` | The Nanocode home, AGENTS.md and skill discovery, shell detection, user services |
| `nanocode-image`, `nanocode-audio` | Prompt image preparation; audio token accounting |

## runtime

| Crate | Owns |
| --- | --- |
| `nanocode-agent` | `AgentRuntime`, `TurnInbox`, `SessionLog`, session event batching |
| `nanocode-agent-config` | Assembly interfaces, so backends do not depend on `nanocode-agent` |
| `nanocode-subagents` | The task tree: spawning, waiting, messages, residency, completions, budgets, the journal |
| `nanocode-code-mode` | `exec` and `wait`, cell orchestration and replay, the remote hand protocol |
| `nanocode-code-mode-engine` | The QuickJS cell worker pool (native) and stepped execution (WASM) |
| `nanocode-workspace` | A workspace without a machine: the psh shell and file host interfaces over an embedder's filesystem, owned by one worker |
| `nanocode-durability` | Session drivers, the `SessionStore` catalog, `SharedStore`, `MemoryStore`, branches, the record cache |
| `nanocode-tool-host` | The `Tools` builder, tool selection, admission, `FunctionTool`, description pins |
| `nanocode-hands` | Per-call Code Mode targets (workspace, hosted, pool, native, mac), the `HandHost` contract, lazy start and idle release, sha-checked atomic copies |
| `nanocode-live` | Live views over a partitioned log: one follower and one fold per view instance per node, diffs encoded once per access level and shared by every subscribed socket |
| `nanocode-scope` | Resident scopes: a scope's folded state held by its partition's lease owner, commands group-committed through one queue per scope, content-addressed part-level checkpoints, routing to the holder with a deadline |

## backends

| Crate | Owns |
| --- | --- |
| `nanocode-backend-antigravity-client` | The Gemini wire format, SSE streaming, retries, explicit context caching |
| `nanocode-backend-antigravity-tools` | Antigravity's engine tools, their declarations and output formats |
| `nanocode-backend-antigravity` | The Antigravity driver, its pinned prompts, reminders and compaction |
| `nanocode-backend-claude-client` | The Messages wire format, authentication, the subscription wire, streaming, message threads, and Claude on Amazon Bedrock (`bedrock`: bedrock-runtime and bedrock-mantle behind one HTTP layer, `ClaudeClient::bedrock`) |
| `nanocode-backend-claude-tools` | Claude's tool definitions and handlers |
| `nanocode-backend-claude` | The Claude driver, its pinned upstream prompts and tools, durability format |
| `nanocode-backend-codex-client` | The Responses protocol, ChatGPT and API authentication, WebSocket and HTTPS transports, gateways, pricing |
| `nanocode-backend-codex` | The Codex driver, built-in tools, compaction, durability format |

## integrations

| Crate | Owns |
| --- | --- |
| `nanocode-agent-host` | Native defaults: durable process routing, runner lifecycle, computer use |
| `nanocode-tool-runtime` | Host tool registration, workspace execution, MCP, hands and attachment runners |
| `nanocode-process` | Shell sessions, PTYs, the zsh shell server, warm bash processes |
| `nanocode-mcp-tools` | MCP discovery, catalogs, prepared tools, stdio processes |
| `nanocode-computer` | The `computer` tool over macOS accessibility, Linux AT-SPI, cua-driver and MCP providers; the `cua_repl` tools and browser extension host |
| `nanocode-chatgpt-extension` | Browser tabs through the official ChatGPT extension's native-messaging host |
| `nanocode-sandbox` | Commands confined to read-only paths, one writable directory and an allowlisting egress proxy, optionally each in its own cgroup v2 group with CPU, memory, pids and io limits |
| `nanocode-store-jsonl` | The append-only JSONL store, one file per session: the CLI's default |
| `nanocode-store-sql` | `Sql` hosts for SQLite (`SqliteSql`), Postgres (`PostgresSql`) and a host's own sqlx pool or transaction (`SqlxSql`, `SqlxTransaction`, `commit_staged`) |
| `nanocode-s3` | An S3 client for immutable content-addressed blobs: SigV4 signing (through `nanocode-aws`), put-if-absent, ranged get, head, delete and presigned URLs |
| `nanocode-kube` | A Kubernetes API client over `nanocode-http`: server-side apply, get, list and watch from a resourceVersion with bookmarks, 410 Gone surfaced for relists, bounded event buffers, a `Transport` trait a fake API server can stand in for, in-cluster ServiceAccount and kubeconfig (token or client certificate) credentials, and typed Deployment, DaemonSet, Service, ConfigMap, Lease and Node |
| `nanocode-observability` | Tracing and OpenTelemetry setup |
| `nanocode-telemetry` | GenAI span export, held request traces, and events as log lines and counters, for a server host's own subscriber |

## Embedding

A host builds agents through the `nanocode` facade alone. `crates/nanocode/examples/bedrock_embed.rs` runs every step below against Amazon Bedrock.

| Need | API |
| --- | --- |
| Bash in the host process | `nanocode::shell::serve_if_requested()` first in `main`, then `nanocode::shell::install()` (unix) |
| Claude on Bedrock | `ClaudeClient::bedrock(ClaudeClient::http_client(), Bedrock::from_env()?)`, or `Bedrock::new(BedrockConfig::from_lookup(..)?, lookup, sender)` |
| The runtime | `Nanocode::builder().claude(ClaudeHarness::new(client)).durability(store).workspace(dir)` |
| Tool call policy | `.holds(Holds::new(policy, ledger))` wraps the tools of every agent, subagents included |
| Spawn routing | `.spawn_router(router)` and `.spawn_policy(policy)` |
| A child's directory | `SpawnOptions::workdir(dir)` on the route |
| Cost | `TurnResult::usage()` for the root, `runtime.subagents()?.tree_cost(root)` for the children |
| Resume | `runtime.open_session(id)` reopens the root, and its subagents restore with it |

`nanocode::claude::bedrock` reads Claude Code's variables: `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_MANTLE`, `AWS_REGION`, `AWS_PROFILE`, `AWS_BEARER_TOKEN_BEDROCK`, `ANTHROPIC_BEDROCK_REGION_PREFIX`, `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`, `ANTHROPIC_BEDROCK_BASE_URL`, `ANTHROPIC_BEDROCK_MANTLE_BASE_URL` and `ENABLE_PROMPT_CACHING_1H`. Model names map to `global.anthropic.<model>` on bedrock-runtime, or to `anthropic.<model>` on bedrock-mantle with `CLAUDE_CODE_USE_MANTLE`.

### Spawning

The `spawn_agent` tool takes `role`, `task`, `model`, `thinking`, `output_contract` and, with `SpawnPolicy::budgets`, `budget_usd`. A `SpawnRouter` returns a `SpawnRoute` for each spawn. The route must select a model and thinking, and it may set a working directory. `bind` reports the child session the route produced.

A child with a workdir runs its Bash, Read, Write and Edit tools there, and its system prompt names it. The session record keeps the directory, so a resumed child reopens in it. Other children share the runtime workspace.

### Holds

`HoldPolicy::judge` receives a `HoldCall`: session, turn, call id, tool name, input and `directory`, the working directory of the calling agent. It answers `Run`, `Refuse` or `Hold`. A `HoldLedger` records holds and their answers. `nanocode::turnlog::OneLog::ledger()` provides one, and a host that never holds can answer `running` with `None`.

### Budgets

The Claude driver prices each model call in `model.call.completed`. The registry adds that price to the child and to its ancestors' subtrees. Once a subtree reaches a budget, the spending child stops with a `budget_exceeded` output. Calls in flight at that moment can pass the budget, and backends without a per-call price are not stopped. `SubagentControl::set_budget` changes a budget and clears the stop.

### Notes

- Take `runtime.take_subagent_updates()` before the first spawn. Each agent build drops an unclaimed receiver.
- `TreeCost` covers subagents only. The root's spend is the sum of its turns' usage, which the host keeps.

## Durability

Each backend implements `DurableAgentExt`, `DurableBackendRegistration` and a `CheckpointFormat` in its own `durability` module, and the composed runtime registers them on the store it is given. Native hosts default to `nanocode-store-jsonl` in the Nanocode home. `SqlStore`, `RowStore` and `SessionLog` run over any `Sql` host: `nanocode-store-sql` natively, a host's own SQL (Durable Objects, D1, libSQL, sqlite-wasm) on WASM. Memory is the WASM default and is not durable.

## Composition

| Host | Crates |
| --- | --- |
| Native TUI or CLI | `nanocode`, `nanocode-tui`, `nanocode-observability` |
| Native server | `nanocode`, `nanocode-store-sql`, `nanocode-observability` |
| WASM brain | `nanocode` with `default-features = false` (no QuickJS) and a host `Sql` or `StateStore` |
| WASM with local cells | `nanocode` with `local-cells`, built against wasi-sdk |

## WASM

Every reusable crate builds for native and `wasm32-unknown-unknown`. There is no packaged WASM SDK and no wasm-bindgen ABI; bindings live in other repositories. A WASM embedding installs one `Rc<dyn nanocode_platform::Host>` per execution thread, which schedules local futures, implements cancellable sleeps and supplies clocks and secure random bytes. Codex networking goes through `HostTransport` and Claude's through `HttpTransport`; native terminal, filesystem, database, process and telemetry adapters are gated out.

## Replacements and patches

- `nanocode-schema` is an adaptation of Boon (JSON Schema drafts 4 through 2020-12), in place of jsonschema; see [its README](core/schema/README.md).
- `nanocode-mcp` replaces rmcp and oauth2; see [its README](core/mcp/README.md). MCP tool search uses its own BM25 index; see [the scoring contract](integrations/mcp-tools/src/SEARCH.md).
- `nanocode-zstd` decodes zstd responses without a C library.
- `patches/` holds rustls-platform-verifier and iana-time-zone, patched so macOS builds link neither Security.framework nor CoreFoundation.
