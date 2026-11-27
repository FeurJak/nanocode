use super::Registry;
use crate::model::AgentId;
use nanocode_agent_core::UsdAmount;
use std::sync::Arc;

struct Exceeded {
    agent: AgentId,
    budget: UsdAmount,
    spent: UsdAmount,
}

impl Registry {
    pub(super) async fn charge(
        self: &Arc<Self>,
        root_session_id: &str,
        id: AgentId,
        nano_usd: u64,
    ) {
        let root = root_session_id.to_owned();
        let exceeded = self
            .state
            .call(move |state| {
                let scope = state.scopes.get_mut(&root)?;
                let session = scope.sessions.get_mut(&id)?;
                session.turn_spend_nano_usd = session.turn_spend_nano_usd.saturating_add(nano_usd);
                if session.over_budget {
                    return None;
                }
                let mut next = Some(id);
                while let Some(current) = next {
                    let budget = scope
                        .sessions
                        .get(&current)
                        .and_then(|session| session.descriptor.budget_usd);
                    if let Some(budget) = budget {
                        let spent = scope
                            .topology
                            .subtree_postorder(current)
                            .unwrap_or_default()
                            .iter()
                            .filter_map(|member| scope.sessions.get(member))
                            .fold(0u64, |spent, member| {
                                spent
                                    .saturating_add(member.usage.cost_usd.unwrap_or_default().nano_usd())
                                    .saturating_add(member.turn_spend_nano_usd)
                            });
                        if spent >= budget.nano_usd() {
                            let exceeded = Exceeded {
                                agent: current,
                                budget,
                                spent: UsdAmount::from_nano_usd(spent),
                            };
                            let session = scope.sessions.get_mut(&id)?;
                            session.over_budget = true;
                            session.last_output = Some(serde_json::json!({
                                "error": "budget_exceeded",
                                "message": format!(
                                    "stopped: agent {} has a budget of ${} and its work has spent ${}",
                                    exceeded.agent,
                                    exceeded.budget.decimal(),
                                    exceeded.spent.decimal()
                                ),
                            }));
                            return Some(exceeded);
                        }
                    }
                    next = scope.topology.parent(current);
                }
                None
            })
            .await;
        let Some(exceeded) = exceeded else {
            return;
        };
        nanocode_trace::warn!(
            root_session_id,
            agent_id = %id,
            budget_agent_id = %exceeded.agent,
            budget_usd = %exceeded.budget.decimal(),
            spent_usd = %exceeded.spent.decimal(),
            "a subagent's spend reached a budget; interrupting it"
        );
        let registry = Arc::clone(self);
        let root = root_session_id.to_owned();
        drop(crate::platform::spawn(async move {
            if let Err(error) = registry.interrupt(&root, id).await {
                nanocode_trace::warn!(%error, agent_id = %id, "an over-budget subagent could not be interrupted");
            }
        }));
    }
}
