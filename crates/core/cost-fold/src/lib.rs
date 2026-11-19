use std::collections::{BTreeMap, BTreeSet};

use nanocode_agent_types::usage::TurnUsage;
use serde::{Deserialize, Serialize};

pub mod sim;

use sim::{Mutation, mutated};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
}

impl Tokens {
    #[must_use]
    pub fn of(usage: &TurnUsage) -> Self {
        Self {
            input: usage.input_tokens(),
            cache_read: usage.cached_input_tokens(),
            cache_write: usage.cache_write_input_tokens(),
            output: usage.output_tokens(),
            reasoning: usage.reasoning_output_tokens(),
        }
    }

    #[must_use]
    pub const fn total(self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.output)
    }

    fn add(&mut self, other: Self) {
        self.input = self.input.saturating_add(other.input);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.output = self.output.saturating_add(other.output);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub hand: String,
    pub hand_ms: u64,
    pub hand_nano_usd: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub key: String,
    pub agent: String,
    pub turn: String,
    pub model: String,
    pub tokens: Tokens,
    pub model_nano_usd: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
}

impl Step {
    #[must_use]
    pub fn of(
        key: impl Into<String>,
        agent: impl Into<String>,
        turn: impl Into<String>,
        model: impl Into<String>,
        usage: &TurnUsage,
    ) -> Self {
        Self {
            key: key.into(),
            agent: agent.into(),
            turn: turn.into(),
            model: model.into(),
            tokens: Tokens::of(usage),
            model_nano_usd: usage.estimated_cost().map(|cost| cost.amount().nano_usd()),
            placement: None,
        }
    }

    #[must_use]
    pub fn placed(mut self, placement: Placement) -> Self {
        self.placement = Some(placement);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cost", rename_all = "snake_case")]
pub enum CostEvent {
    Agent {
        agent: String,
        parent: Option<String>,
        task: Option<String>,
    },
    Step(Step),
    Commit {
        sha: String,
        agent: String,
        turn: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cost {
    pub steps: u64,
    pub tokens: Tokens,
    pub model_nano_usd: u64,
    pub unpriced_model_steps: u64,
    pub hand_ms: u64,
    pub hand_nano_usd: u64,
    pub unpriced_hand_steps: u64,
}

impl Cost {
    #[must_use]
    pub const fn nano_usd(&self) -> u64 {
        self.model_nano_usd.saturating_add(self.hand_nano_usd)
    }

    pub fn add(&mut self, other: &Self) {
        self.steps = self.steps.saturating_add(other.steps);
        self.tokens.add(other.tokens);
        self.model_nano_usd = self.model_nano_usd.saturating_add(other.model_nano_usd);
        self.unpriced_model_steps = self
            .unpriced_model_steps
            .saturating_add(other.unpriced_model_steps);
        self.hand_ms = self.hand_ms.saturating_add(other.hand_ms);
        self.hand_nano_usd = self.hand_nano_usd.saturating_add(other.hand_nano_usd);
        self.unpriced_hand_steps = self
            .unpriced_hand_steps
            .saturating_add(other.unpriced_hand_steps);
    }

    #[must_use]
    pub fn of(step: &Step) -> Self {
        let mut cost = Self {
            steps: 1,
            tokens: step.tokens,
            ..Self::default()
        };
        match step.model_nano_usd {
            Some(nano) => cost.model_nano_usd = nano,
            None => cost.unpriced_model_steps = 1,
        }
        if let Some(placement) = &step.placement {
            cost.hand_ms = placement.hand_ms;
            match placement.hand_nano_usd {
                Some(nano) => cost.hand_nano_usd = nano,
                None => cost.unpriced_hand_steps = 1,
            }
        }
        cost
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitCost {
    pub sha: String,
    pub agent: String,
    pub turn: String,
    pub task: Option<String>,
    pub turn_cost: Cost,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum CostError {
    Corrupt { detail: String },
    UnknownAgent { agent: String },
    AgentChanged { agent: String },
    TaskMoved { task: String },
    TaskCycle { task: String },
    StepChanged { key: String },
    CommitChanged { sha: String },
}

impl std::fmt::Display for CostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Corrupt { detail } => write!(formatter, "cost row is corrupt: {detail}"),
            Self::UnknownAgent { agent } => write!(formatter, "cost names unknown agent {agent}"),
            Self::AgentChanged { agent } => write!(
                formatter,
                "agent {agent} was recorded again with another parent or task"
            ),
            Self::TaskMoved { task } => write!(
                formatter,
                "task {task} was given an agent under another parent task"
            ),
            Self::TaskCycle { task } => write!(formatter, "task {task} would be its own ancestor"),
            Self::StepChanged { key } => {
                write!(formatter, "step {key} was recorded again with other usage")
            }
            Self::CommitChanged { sha } => {
                write!(formatter, "commit {sha} was linked again to another turn")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    parent: Option<String>,
    task: Option<String>,
    children: BTreeSet<String>,
    own: Cost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    Recorded,
    Duplicate,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    agents: BTreeMap<String, Node>,
    task_parents: BTreeMap<String, String>,
    task_children: BTreeMap<String, BTreeSet<String>>,
    steps: BTreeMap<String, Step>,
    turns: BTreeMap<String, Cost>,
    tasks: BTreeMap<Option<String>, Cost>,
    commits: BTreeMap<String, (String, String)>,
}

impl Ledger {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            agents: BTreeMap::new(),
            task_parents: BTreeMap::new(),
            task_children: BTreeMap::new(),
            steps: BTreeMap::new(),
            turns: BTreeMap::new(),
            tasks: BTreeMap::new(),
            commits: BTreeMap::new(),
        }
    }

    pub fn apply_json(&mut self, body: &str) -> Result<Applied, CostError> {
        let event = serde_json::from_str(body).map_err(|error| CostError::Corrupt {
            detail: error.to_string(),
        })?;
        self.apply(event)
    }

    pub fn apply(&mut self, event: CostEvent) -> Result<Applied, CostError> {
        match event {
            CostEvent::Agent {
                agent,
                parent,
                task,
            } => self.agent(agent, parent, task),
            CostEvent::Step(step) => self.step(step),
            CostEvent::Commit { sha, agent, turn } => self.commit(sha, agent, turn),
        }
    }

    fn agent(
        &mut self,
        agent: String,
        parent: Option<String>,
        task: Option<String>,
    ) -> Result<Applied, CostError> {
        if let Some(node) = self.agents.get(&agent) {
            if node.parent == parent && node.task == task {
                return Ok(Applied::Duplicate);
            }
            return Err(CostError::AgentChanged { agent });
        }
        let parent_task = match &parent {
            Some(parent) if self.agents.contains_key(parent) => {
                self.task_of(parent).map(str::to_owned)
            }
            Some(parent) => {
                return Err(CostError::UnknownAgent {
                    agent: parent.clone(),
                });
            }
            None => None,
        };
        let link = match (&task, parent_task) {
            (Some(task), Some(above)) if *task != above => {
                if let Some(seen) = self.task_parents.get(task) {
                    if *seen != above {
                        return Err(CostError::TaskMoved { task: task.clone() });
                    }
                }
                let mut at = Some(above.as_str());
                while let Some(ancestor) = at {
                    if ancestor == task {
                        return Err(CostError::TaskCycle { task: task.clone() });
                    }
                    at = self.task_parents.get(ancestor).map(String::as_str);
                }
                Some((task.clone(), above))
            }
            _ => None,
        };
        if let Some((task, above)) = link {
            self.task_children
                .entry(above.clone())
                .or_default()
                .insert(task.clone());
            self.task_parents.insert(task, above);
        }
        if let Some(parent) = &parent {
            if let Some(node) = self.agents.get_mut(parent) {
                node.children.insert(agent.clone());
            }
        }
        self.agents.insert(
            agent,
            Node {
                parent,
                task,
                children: BTreeSet::new(),
                own: Cost::default(),
            },
        );
        Ok(Applied::Recorded)
    }

    fn step(&mut self, step: Step) -> Result<Applied, CostError> {
        if !mutated(Mutation::RetriesCounted) {
            if let Some(seen) = self.steps.get(&step.key) {
                if seen == &step {
                    return Ok(Applied::Duplicate);
                }
                return Err(CostError::StepChanged { key: step.key });
            }
        }
        if !self.agents.contains_key(&step.agent) {
            return Err(CostError::UnknownAgent { agent: step.agent });
        }
        let cost = Cost::of(&step);
        let task = self.task_of(&step.agent).map(str::to_owned);
        self.tasks.entry(task).or_default().add(&cost);
        if let Some(node) = self.agents.get_mut(&step.agent) {
            node.own.add(&cost);
        }
        self.turns.entry(step.turn.clone()).or_default().add(&cost);
        self.steps.insert(step.key.clone(), step);
        Ok(Applied::Recorded)
    }

    fn commit(&mut self, sha: String, agent: String, turn: String) -> Result<Applied, CostError> {
        if !self.agents.contains_key(&agent) {
            return Err(CostError::UnknownAgent { agent });
        }
        if let Some(seen) = self.commits.get(&sha) {
            if seen.0 == agent && seen.1 == turn {
                return Ok(Applied::Duplicate);
            }
            return Err(CostError::CommitChanged { sha });
        }
        self.commits.insert(sha, (agent, turn));
        Ok(Applied::Recorded)
    }

    pub fn agents(&self) -> impl Iterator<Item = &str> + '_ {
        self.agents.keys().map(String::as_str)
    }

    #[must_use]
    pub fn parent(&self, agent: &str) -> Option<&str> {
        self.agents.get(agent)?.parent.as_deref()
    }

    pub fn children(&self, agent: &str) -> impl Iterator<Item = &str> + '_ {
        self.agents
            .get(agent)
            .into_iter()
            .flat_map(|node| node.children.iter().map(String::as_str))
    }

    #[must_use]
    pub fn task_of(&self, agent: &str) -> Option<&str> {
        let mut at = self.agents.get(agent)?;
        loop {
            if let Some(task) = &at.task {
                return Some(task);
            }
            if mutated(Mutation::OrphanSubagents) {
                return None;
            }
            at = self.agents.get(at.parent.as_deref()?)?;
        }
    }

    #[must_use]
    pub fn task_parent(&self, task: &str) -> Option<&str> {
        self.task_parents.get(task).map(String::as_str)
    }

    pub fn task_children(&self, task: &str) -> impl Iterator<Item = &str> + '_ {
        self.task_children
            .get(task)
            .into_iter()
            .flat_map(|children| children.iter().map(String::as_str))
    }

    #[must_use]
    pub fn agent_own(&self, agent: &str) -> Option<Cost> {
        Some(self.agents.get(agent)?.own)
    }

    #[must_use]
    pub fn agent_total(&self, agent: &str) -> Option<Cost> {
        let node = self.agents.get(agent)?;
        let mut cost = node.own;
        for (index, child) in node.children.iter().enumerate() {
            if index == 0 && mutated(Mutation::RollupSkipsChild) {
                continue;
            }
            cost.add(&self.agent_total(child)?);
        }
        Some(cost)
    }

    #[must_use]
    pub fn by_agent(&self) -> BTreeMap<&str, Cost> {
        self.agents
            .iter()
            .map(|(agent, node)| (agent.as_str(), node.own))
            .collect()
    }

    #[must_use]
    pub const fn by_turn(&self) -> &BTreeMap<String, Cost> {
        &self.turns
    }

    #[must_use]
    pub fn turn(&self, turn: &str) -> Cost {
        self.turns.get(turn).copied().unwrap_or_default()
    }

    #[must_use]
    pub const fn by_task(&self) -> &BTreeMap<Option<String>, Cost> {
        &self.tasks
    }

    #[must_use]
    pub fn task_own(&self, task: &str) -> Cost {
        self.tasks
            .get(&Some(task.to_owned()))
            .copied()
            .unwrap_or_default()
    }

    #[must_use]
    pub fn task_total(&self, task: &str) -> Cost {
        let mut cost = self.task_own(task);
        for child in self.task_children(task) {
            cost.add(&self.task_total(child));
        }
        cost
    }

    #[must_use]
    pub fn total(&self) -> Cost {
        let mut cost = Cost::default();
        for node in self.agents.values() {
            cost.add(&node.own);
        }
        cost
    }

    #[must_use]
    pub fn commit_cost(&self, sha: &str) -> Option<CommitCost> {
        let (agent, turn) = self.commits.get(sha)?;
        Some(CommitCost {
            sha: sha.to_owned(),
            agent: agent.clone(),
            turn: turn.clone(),
            task: self.task_of(agent).map(str::to_owned),
            turn_cost: self.turn(turn),
        })
    }

    #[must_use]
    pub fn commits(&self) -> Vec<CommitCost> {
        self.commits
            .keys()
            .filter_map(|sha| self.commit_cost(sha))
            .collect()
    }
}
