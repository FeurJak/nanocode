#[cfg(unix)]
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[cfg(unix)]
mod embed {
    use nanocode::subagents::{SpawnPolicy, SpawnRoute, SpawnRouter};
    use nanocode::tools::hold::{
        Answer, HoldCall, HoldLedger, HoldPolicy, Holds, Judgment, LedgerError, Resumption, Running,
    };
    use nanocode::{HostFuture, Thinking};
    use serde_json::value::RawValue;
    use std::sync::Arc;

    pub struct Guard;

    impl HoldPolicy for Guard {
        fn judge<'a>(&'a self, call: HoldCall<'a>) -> HostFuture<'a, Judgment> {
            Box::pin(async move {
                eprintln!(
                    "hold policy: session {} tool {} in {}",
                    call.session_id,
                    call.tool,
                    call.directory.unwrap_or("-")
                );
                match call.tool == "Bash" && call.input.get().contains("rm -rf") {
                    true => Judgment::Refuse {
                        message: "this host refuses recursive deletes".to_owned(),
                    },
                    false => Judgment::Run,
                }
            })
        }
    }

    pub struct NoHolds;

    impl HoldLedger for NoHolds {
        fn hold<'a>(
            &'a self,
            key: &'a str,
            _call: HoldCall<'a>,
            _card: &'a RawValue,
        ) -> HostFuture<'a, Result<(), LedgerError>> {
            Box::pin(async move {
                Err(LedgerError::Journal(format!(
                    "this host keeps no holds ({key})"
                )))
            })
        }

        fn resolve<'a>(
            &'a self,
            key: &'a str,
            _answer: Answer,
        ) -> HostFuture<'a, Result<Resumption, LedgerError>> {
            Box::pin(async move {
                Err(LedgerError::Journal(format!(
                    "this host keeps no holds ({key})"
                )))
            })
        }

        fn running<'a>(&'a self, _call_id: &'a str) -> HostFuture<'a, Option<Running>> {
            Box::pin(async { None })
        }

        fn claim<'a>(&'a self, _key: &'a str) -> HostFuture<'a, bool> {
            Box::pin(async { false })
        }

        fn release<'a>(&'a self, _key: &'a str) -> HostFuture<'a, Result<(), LedgerError>> {
            Box::pin(async { Ok(()) })
        }
    }

    pub struct ByRole {
        pub workspace: std::path::PathBuf,
        pub spawned: std::sync::atomic::AtomicU32,
    }

    impl SpawnRouter for ByRole {
        fn resolve<'life0, 'life1, 'life2, 'life3, 'life4, 'async_trait>(
            &'life0 self,
            _parent_session_id: &'life1 str,
            role: &'life2 str,
            _task: &'life3 str,
            options: nanocode::agent::SpawnOptions,
            _host_context: Option<&'life4 str>,
        ) -> nanocode::subagents::RouteFuture<'async_trait, std::io::Result<SpawnRoute>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            'life3: 'async_trait,
            'life4: 'async_trait,
            Self: 'async_trait,
        {
            let model = match role {
                "explore" => nanocode::ClaudeModel::Haiku55,
                _ => nanocode::ClaudeModel::Sonnet55,
            };
            let reference = format!("role {role}");
            let options = options
                .harness_model(nanocode::ModelSelection::Claude(model))
                .thinking(Thinking::Low);
            let workdir = (role == "implement").then(|| {
                let count = self
                    .spawned
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.workspace
                    .join("children")
                    .join(format!("implement-{count}"))
            });
            Box::pin(async move {
                let options = match workdir {
                    Some(workdir) => {
                        std::fs::create_dir_all(&workdir)?;
                        options.workdir(workdir)
                    }
                    None => options,
                };
                Ok(SpawnRoute { options, reference })
            })
        }

        fn bind(
            &self,
            parent_session_id: &str,
            child_session_id: &str,
            reference: &str,
            _host_context: Option<&str>,
        ) -> std::io::Result<()> {
            eprintln!("spawn router: {parent_session_id} -> {child_session_id} ({reference})");
            Ok(())
        }
    }

    pub fn holds() -> Holds {
        Holds::new(Arc::new(Guard), Arc::new(NoHolds))
    }

    pub fn policy() -> SpawnPolicy {
        SpawnPolicy {
            budgets: true,
            ..SpawnPolicy::default()
        }
    }
}

#[cfg(unix)]
async fn run() -> Result<()> {
    use nanocode::claude::bedrock::Bedrock;
    use nanocode::{ClaudeClient, ClaudeHarness, JsonlStore, Nanocode, PromptRequest};
    use std::sync::Arc;

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 || args.first().is_some_and(|arg| arg == "--help") {
        return Err("usage: bedrock_embed STORE_DIR WORKSPACE [--resume SESSION] PROMPT...\nClaude on Amazon Bedrock with CLAUDE_CODE_USE_BEDROCK's variables (AWS_PROFILE, AWS_REGION, ...): a hold policy on every agent's tool calls, a spawn router by role, budgets on.".into());
    }
    let client = ClaudeClient::bedrock(ClaudeClient::http_client(), Bedrock::from_env()?);
    let store = nanocode::session_store(JsonlStore::open_async(&args[0]).await?)?;
    let runtime = Nanocode::builder()
        .claude(ClaudeHarness::new(client))
        .durability(store)
        .workspace(&args[1])
        .holds(embed::holds())
        .spawn_router(Arc::new(embed::ByRole {
            workspace: std::path::PathBuf::from(&args[1]),
            spawned: std::sync::atomic::AtomicU32::new(0),
        }))
        .spawn_policy(embed::policy())
        .without_cua()
        .runner_management(false)
        .build_async()
        .await?;
    let outcome: Result<()> = async {
        let (resume, prompt) = match args.get(2).map(String::as_str) {
            Some("--resume") if args.len() > 4 => (Some(&args[3]), &args[4..]),
            _ => (None, &args[2..]),
        };
        let (agent, _events) = match resume {
            Some(session) => runtime.open_session(session).await?,
            None => {
                runtime
                    .spawn()
                    .backend::<nanocode::Claude>()
                    .model(nanocode::ClaudeModel::Opus55)
                    .build()
                    .await?
            }
        };
        eprintln!("session {}", agent.session_id());
        let result = agent
            .prompt(PromptRequest::new(prompt.join(" ")))
            .await?
            .result()
            .await?;
        println!("{}", result.final_message());
        eprintln!("root usage {:?}", result.usage());
        if let Some(control) = runtime.subagents() {
            for entry in control.agents(agent.session_id()).await? {
                eprintln!(
                    "child {} role {} model {:?} budget {:?} usage {:?}",
                    entry.agent_id, entry.role, entry.model, entry.budget_usd, entry.usage
                );
            }
        }
        agent.flush().await?;
        Ok(())
    }
    .await;
    let shutdown = runtime.shutdown().await;
    outcome?;
    shutdown?;
    Ok(())
}

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    if let Some(code) = nanocode::shell::serve_if_requested() {
        return code;
    }
    if let Err(error) = nanocode::shell::install() {
        eprintln!("{error}");
        return std::process::ExitCode::FAILURE;
    }
    let outcome = nanocode_executor::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(Into::into)
        .and_then(|runtime| runtime.block_on(run()));
    match outcome {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
fn main() {}
