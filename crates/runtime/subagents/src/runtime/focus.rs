use super::{AgentDirectoryEntry, AgentSummary, MessageReceipt, Registry, SubagentControl};
use crate::{
    harness::{HarnessHandle, SideQuestionLease},
    model::{AgentId, MessagePriority, MessagePurpose},
    platform,
};
use nanocode_agent_core::{NanocodeError, SideAnswer, UsdAmount};
use std::sync::Arc;

mod control;

#[derive(Default)]
pub struct TreeCost {
    agents: Vec<AgentDirectoryEntry>,
}

impl TreeCost {
    const fn of(agents: Vec<AgentDirectoryEntry>) -> Self {
        Self { agents }
    }

    #[must_use]
    pub fn agents(&self) -> &[AgentDirectoryEntry] {
        &self.agents
    }

    #[must_use]
    pub fn total(&self) -> nanocode_agent_core::AgentUsage {
        let mut total = nanocode_agent_core::AgentUsage::default();
        for agent in &self.agents {
            total.add(&agent.usage);
        }
        total
    }

    #[must_use]
    pub fn subtree(&self, id: crate::model::AgentId) -> nanocode_agent_core::AgentUsage {
        let mut total = nanocode_agent_core::AgentUsage::default();
        let mut within = vec![id];
        let mut index = 0;
        while let Some(current) = within.get(index).copied() {
            index += 1;
            for agent in &self.agents {
                if agent.agent_id == current {
                    total.add(&agent.usage);
                }
                if agent.parent_agent_id == Some(current) && !within.contains(&agent.agent_id) {
                    within.push(agent.agent_id);
                }
            }
        }
        total
    }

    #[must_use]
    pub fn by_role(&self) -> std::collections::BTreeMap<&str, nanocode_agent_core::AgentUsage> {
        let mut roles = std::collections::BTreeMap::<&str, nanocode_agent_core::AgentUsage>::new();
        for agent in &self.agents {
            roles.entry(&agent.role).or_default().add(&agent.usage);
        }
        roles
    }

    #[must_use]
    pub fn by_model(&self) -> std::collections::BTreeMap<String, nanocode_agent_core::AgentUsage> {
        let mut models =
            std::collections::BTreeMap::<String, nanocode_agent_core::AgentUsage>::new();
        for agent in &self.agents {
            let model = agent
                .model
                .as_ref()
                .map_or_else(String::new, |model| model.as_str().to_owned());
            models.entry(model).or_default().add(&agent.usage);
        }
        models
    }
}

struct SideQuestion {
    lease: Option<SideQuestionLease>,
    registry: Arc<Registry>,
    root_session_id: String,
}

impl Drop for SideQuestion {
    fn drop(&mut self) {
        drop(self.lease.take());
        let registry = Arc::clone(&self.registry);
        let root_session_id = std::mem::take(&mut self.root_session_id);
        drop(platform::spawn(async move {
            registry.enforce_resident_limit(&root_session_id).await;
        }));
    }
}

impl SubagentControl {
    pub async fn send_user_message(
        &self,
        root_session_id: &str,
        id: AgentId,
        priority: MessagePriority,
        body: impl Into<String>,
    ) -> std::io::Result<MessageReceipt> {
        control::send_user_message(self, root_session_id, id, priority, body.into()).await
    }

    pub async fn interrupt(
        &self,
        root_session_id: &str,
        id: AgentId,
    ) -> std::io::Result<Vec<AgentSummary>> {
        control::interrupt(self, root_session_id, id).await
    }

    pub async fn close(
        &self,
        root_session_id: &str,
        id: AgentId,
    ) -> std::io::Result<Vec<AgentSummary>> {
        control::close(self, root_session_id, id).await
    }

    pub async fn set_budget(
        &self,
        root_session_id: &str,
        id: AgentId,
        budget: Option<UsdAmount>,
    ) -> std::io::Result<AgentSummary> {
        control::set_budget(self, root_session_id, id, budget).await
    }

    pub async fn budgets(&self, session_id: &str) -> Vec<(AgentId, UsdAmount)> {
        control::budgets(self, session_id).await
    }

    pub async fn agents(&self, root_session_id: &str) -> std::io::Result<Vec<AgentDirectoryEntry>> {
        control::agents(self, root_session_id).await
    }

    pub async fn tree_cost(&self, root_session_id: &str) -> std::io::Result<TreeCost> {
        Ok(TreeCost::of(control::agents(self, root_session_id).await?))
    }

    pub async fn agents_page(
        &self,
        root_session_id: &str,
        after: Option<AgentId>,
        limit: std::num::NonZeroUsize,
    ) -> std::io::Result<(Vec<AgentDirectoryEntry>, Option<AgentId>)> {
        control::agents_page(self, root_session_id, after, limit).await
    }

    pub(super) async fn active(
        &self,
        root_session_id: &str,
    ) -> std::io::Result<Vec<AgentDirectoryEntry>> {
        control::active(self, root_session_id).await
    }

    pub async fn session_id(&self, root_session_id: &str, id: AgentId) -> std::io::Result<String> {
        control::session_id(self, root_session_id, id).await
    }

    pub async fn side_question(
        &self,
        root_session_id: &str,
        id: AgentId,
        question: impl Into<String>,
    ) -> nanocode_agent_core::Result<SideAnswer> {
        let invalid = |error: std::io::Error| NanocodeError::InvalidRequest(error.to_string());
        let (harness, _side_question) = self
            .lease_side_question(root_session_id, id)
            .await
            .map_err(invalid)?;
        let agent = harness.agent().await.map_err(invalid)?;
        agent.side_question(question).await
    }

    async fn lease_side_question(
        &self,
        root_session_id: &str,
        id: AgentId,
    ) -> std::io::Result<(HarnessHandle, SideQuestion)> {
        control::lease_side_question(self, root_session_id, id).await
    }

    async fn require_root(&self, root_session_id: &str) -> std::io::Result<()> {
        if self.registry.is_root_session(root_session_id).await {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "{root_session_id} is not a root session with subagents"
            )))
        }
    }
}
