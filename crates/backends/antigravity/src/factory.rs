use crate::driver::Inner;
use crate::harness::{AntigravityHarness, FAMILY};
use nanocode_agent_config::HarnessRequest;
use nanocode_agent_config::configuration::{
    DelegationTools, HostServices, RuntimeRequest, Settings,
};
use nanocode_agent_core::{
    Agent, AgentEvents, AgentHandle, ForkPoint, ForkRequest, HarnessFamily, Lineage,
    ModelSelection, NanocodeError, Origin, Result, SessionCheckpoint, SpawnOptions, Thinking,
    backend::{AgentFactory, BackendFuture},
};
use nanocode_durability::{DurableSession, SessionRecord, SharedStore, agent_error};
use nanocode_sync::Atom;
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Weak};

pub(crate) struct Recipe {
    pub(crate) harness: AntigravityHarness,
    pub(crate) settings: Settings,
    pub(crate) defaults: Settings,
    pub(crate) workspace: Arc<Path>,
    pub(crate) subagents: Option<Arc<dyn DelegationTools>>,
    pub(crate) services: Arc<dyn HostServices>,
    pub(crate) routed: Arc<dyn AgentFactory>,
    pub(crate) branches: Option<(SharedStore, SessionRecord)>,
}

pub(crate) struct AntigravityFactory {
    recipe: Arc<Recipe>,
    parent: Atom<Option<Weak<Inner>>>,
}

struct Child {
    model: ModelSelection,
    thinking: Thinking,
    session: String,
    lineage: Lineage,
    checkpoint: Option<SessionCheckpoint>,
    host_context: Option<Arc<str>>,
}

impl AntigravityFactory {
    pub(crate) fn new(recipe: Recipe) -> Self {
        Self {
            recipe: Arc::new(recipe),
            parent: Atom::new(None),
        }
    }

    pub(crate) fn bind(&self, inner: &Arc<Inner>) {
        self.parent.set(Some(Arc::downgrade(inner)));
    }

    fn owner(&self) -> Result<Arc<Inner>> {
        self.parent
            .get()
            .as_ref()
            .as_ref()
            .and_then(Weak::upgrade)
            .filter(|inner| inner.life() == crate::driver::OPEN)
            .ok_or(NanocodeError::AgentStopped)
    }

    async fn durable(&self, child: &Child) -> Result<Option<DurableSession>> {
        let Some((store, record)) = &self.recipe.branches else {
            return Ok(None);
        };
        let record = record
            .derive(child.session.clone(), child.lineage.origin)
            .with_lineage(child.lineage.clone());
        DurableSession::prepare_shared_described(
            store.clone(),
            child.session.clone(),
            None,
            Some(record),
        )
        .await
        .map(Some)
        .map_err(agent_error)
    }

    async fn build(&self, parent: AgentHandle, child: Child) -> Result<(Agent, AgentEvents)> {
        let durable_state = self.durable(&child).await?;
        let recipe = &self.recipe;
        let request = RuntimeRequest {
            request: HarnessRequest {
                model: child.model,
                thinking: child.thinking,
                options: SpawnOptions::new().session_id(child.session),
                parent: Some(parent),
                host_context: child.host_context,
                checkpoint: child.checkpoint,
                durable_state,
                spawn_factory: Arc::clone(&recipe.routed),
            },
            settings: recipe.settings.clone(),
            defaults: recipe.defaults.clone(),
            workspace: Arc::clone(&recipe.workspace),
            subagents: recipe.subagents.clone(),
            services: Arc::clone(&recipe.services),
        };
        recipe
            .harness
            .assemble_child(request, Some(child.lineage))
            .await
    }

    fn fork_payload(inner: &Inner, point: ForkPoint) -> Result<Value> {
        let invalid =
            |error: serde_json::Error| NanocodeError::InvalidCheckpoint(error.to_string());
        let payload = match point {
            ForkPoint::Latest => inner.payload(&inner.settled.get()),
            ForkPoint::Turn(result) => {
                let checkpoint = result
                    .checkpoint()
                    .ok_or(NanocodeError::ReplayedCheckpointUnavailable)?;
                checkpoint.payload().decode().map_err(invalid)?
            }
            ForkPoint::Checkpoint(checkpoint) => {
                checkpoint.require_family(HarnessFamily::new(FAMILY))?;
                checkpoint.payload().decode().map_err(invalid)?
            }
            _ => {
                return Err(NanocodeError::UnsupportedCapability {
                    capability: "fork_point",
                });
            }
        };
        if payload
            .get("contents")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
        {
            return Err(NanocodeError::ForkBeforeCompletedTurn);
        }
        Ok(payload)
    }
}

impl AgentFactory for AntigravityFactory {
    fn ensure_available(&self, _parent: AgentHandle) -> BackendFuture<Result<()>> {
        let available = self.owner().map(drop);
        Box::pin(async move { available })
    }

    fn settings(&self, _parent: AgentHandle) -> BackendFuture<Result<(ModelSelection, Thinking)>> {
        let settings = self
            .owner()
            .map(|inner| (inner.config.model.clone(), inner.thinking()));
        Box::pin(async move { settings })
    }

    fn spawn(
        &self,
        parent: AgentHandle,
        options: SpawnOptions,
        host_context: Option<Arc<str>>,
    ) -> BackendFuture<Result<(Agent, AgentEvents)>> {
        let owner = self.owner();
        let this = Self {
            recipe: Arc::clone(&self.recipe),
            parent: Atom::new(self.parent.get().as_ref().clone()),
        };
        Box::pin(async move {
            let inner = owner?;
            let session = options
                .session_id
                .clone()
                .unwrap_or_else(|| nanocode_platform::uuid_v7().to_string());
            let options = options.resolve(inner.config.model.clone(), inner.thinking())?;
            let model = options
                .selected_harness_model()
                .ok_or(NanocodeError::BackendContract {
                    detail: "resolved spawn options carry a model",
                })?;
            if model.family() != HarnessFamily::new(FAMILY) {
                return Err(NanocodeError::UnsupportedCapability {
                    capability: "cross_family_spawn",
                });
            }
            let child = Child {
                thinking: options.thinking.unwrap_or_else(|| inner.thinking()),
                model,
                lineage: Lineage::child_of(
                    &inner.config.lineage,
                    inner.config.session.clone(),
                    Origin::Subagent,
                ),
                session,
                checkpoint: None,
                host_context,
            };
            this.build(parent, child).await
        })
    }

    fn fork(
        &self,
        parent: AgentHandle,
        request: ForkRequest,
    ) -> BackendFuture<Result<(Agent, AgentEvents)>> {
        let owner = self.owner();
        let this = Self {
            recipe: Arc::clone(&self.recipe),
            parent: Atom::new(self.parent.get().as_ref().clone()),
        };
        Box::pin(async move {
            let inner = owner?;
            let (point, origin) = request.into_parts();
            if !matches!(origin, Origin::Fork | Origin::SideConversation) {
                return Err(NanocodeError::InvalidRequest(
                    "a fork must be a fork or a side conversation".into(),
                ));
            }
            let mut payload = Self::fork_payload(&inner, point)?;
            if let Some(map) = payload.as_object_mut() {
                map.insert("operation".to_owned(), Value::Null);
                map.insert("progress".to_owned(), Value::Null);
            }
            let session = nanocode_platform::uuid_v7().to_string();
            let lineage =
                Lineage::child_of(&inner.config.lineage, inner.config.session.clone(), origin);
            let thinking = inner.thinking();
            let checkpoint = SessionCheckpoint::native(
                session.clone(),
                inner.config.model.clone(),
                thinking,
                lineage.clone(),
                session.clone(),
                None,
                true,
                payload,
            );
            let child = Child {
                model: inner.config.model.clone(),
                thinking,
                session,
                lineage,
                checkpoint: Some(checkpoint),
                host_context: None,
            };
            this.build(parent, child).await
        })
    }

    fn restore(
        &self,
        parent: AgentHandle,
        checkpoint: SessionCheckpoint,
        host_context: Option<Arc<str>>,
    ) -> BackendFuture<Result<(Agent, AgentEvents)>> {
        let owner = self.owner();
        let this = Self {
            recipe: Arc::clone(&self.recipe),
            parent: Atom::new(self.parent.get().as_ref().clone()),
        };
        Box::pin(async move {
            owner?;
            checkpoint.validate()?;
            checkpoint.require_family(HarnessFamily::new(FAMILY))?;
            let child = Child {
                model: checkpoint.model(),
                thinking: checkpoint.thinking(),
                session: checkpoint.session_id().to_owned(),
                lineage: checkpoint.lineage().clone(),
                checkpoint: Some(checkpoint),
                host_context,
            };
            this.build(parent, child).await
        })
    }
}
