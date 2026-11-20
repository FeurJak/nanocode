use std::{cell::RefCell, collections::BTreeMap, sync::Arc};

use nanocode_agent_core::AgentHandle;
use nanocode_board::sim::{Mutation, mutated};
use nanocode_dispatch::{Agent, AgentPhase};
use nanocode_dispatcher::{Pool, PoolError, PoolFuture, Start};
use serde_json::Value;

use crate::{AgentId, AgentStatus, AgentTask, Registry, start_agent};

struct Placed {
    id: AgentId,
    agent: Agent,
    idle_since_ms: Option<u64>,
}

pub struct SubagentPool {
    parent: AgentHandle,
    registry: Arc<Registry>,
    session: String,
    now_ms: Box<dyn Fn() -> u64>,
    placed: RefCell<BTreeMap<String, Placed>>,
}

impl SubagentPool {
    pub fn new(
        parent: AgentHandle,
        registry: Arc<Registry>,
        session: impl Into<String>,
        now_ms: Box<dyn Fn() -> u64>,
    ) -> Self {
        Self {
            parent,
            registry,
            session: session.into(),
            now_ms,
            placed: RefCell::new(BTreeMap::new()),
        }
    }

    pub fn agent_id(&self, agent: &str) -> Option<AgentId> {
        self.placed.borrow().get(agent).map(|placed| placed.id)
    }

    async fn place(&self, start: Start) -> Result<(), PoolError> {
        if self.placed.borrow().contains_key(&start.agent) {
            return Ok(());
        }
        let task = AgentTask {
            role: start.spawn.domain.role.clone(),
            task: start.brief.text.clone(),
            output_schema: Value::Object(serde_json::Map::new()),
        };
        let report = start_agent(&self.parent, &self.registry, &self.session, task)
            .await
            .map_err(|error| PoolError::Placement(error.to_string()))?;
        let agent = Agent {
            id: start.agent.clone(),
            domain: start.spawn.domain,
            cost_nano_usd: start.spawn.cost_nano_usd,
            intent: Some(start.spawn.intent),
            phase: AgentPhase::Starting,
        };
        let placed = Placed {
            id: report.agent_id,
            agent,
            idle_since_ms: None,
        };
        self.placed.borrow_mut().insert(start.agent, placed);
        Ok(())
    }

    async fn list(&self) -> Vec<Agent> {
        let Ok(listed) = self.registry.directory(&self.session, true, false).await else {
            return self
                .placed
                .borrow()
                .values()
                .map(|placed| placed.agent.clone())
                .collect();
        };
        let now = (self.now_ms)();
        let mut placed = self.placed.borrow_mut();
        placed.retain(|_, placed| {
            let Some(entry) = listed.iter().find(|entry| entry.agent_id == placed.id) else {
                return false;
            };
            placed.agent.phase = match &entry.status {
                AgentStatus::Pending => AgentPhase::Starting,
                AgentStatus::Running => {
                    placed.idle_since_ms = None;
                    AgentPhase::Busy
                }
                AgentStatus::Closing => AgentPhase::Retiring,
                AgentStatus::Closed => return false,
                AgentStatus::Completed { .. }
                | AgentStatus::Interrupted
                | AgentStatus::Failed { .. } => AgentPhase::Idle {
                    since_ms: *placed.idle_since_ms.get_or_insert(now),
                },
            };
            true
        });
        placed.values().map(|placed| placed.agent.clone()).collect()
    }

    async fn close(&self, agent: &str) -> Result<(), PoolError> {
        let Some(id) = self.agent_id(agent) else {
            return Ok(());
        };
        if !mutated(Mutation::PoolRetireKeepsAgent) {
            self.registry
                .close(&self.session, id)
                .await
                .map_err(|error| PoolError::Unknown(error.to_string()))?;
        }
        self.placed.borrow_mut().remove(agent);
        Ok(())
    }
}

impl Pool for SubagentPool {
    fn agents(&self) -> PoolFuture<'_, Vec<Agent>> {
        Box::pin(self.list())
    }

    fn start(&self, start: Start) -> PoolFuture<'_, Result<(), PoolError>> {
        Box::pin(self.place(start))
    }

    fn retire<'a>(&'a self, agent: &'a str) -> PoolFuture<'a, Result<(), PoolError>> {
        Box::pin(self.close(agent))
    }
}
