pub use nanocode_types_core::pricing::{
    CostStatus, EstimatedUsdCost, ReportedCost, ServiceTier, UsdAmount, usd_number,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_field_names)]
pub struct TurnUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
    estimated_cost: Option<Box<EstimatedUsdCost>>,
    cost_status: CostStatus,
    #[serde(default, skip_serializing_if = "is_zero")]
    hedge_input_tokens: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentUsage {
    pub completed_turns: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<UsdAmount>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub estimated_cost_turns: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unpriced_turns: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub hedge_input_tokens: u64,
}

impl AgentUsage {
    pub fn record(&mut self, turn: &TurnUsage) {
        self.completed_turns = self.completed_turns.saturating_add(1);
        self.input_tokens = self.input_tokens.saturating_add(turn.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(turn.cached_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(turn.cache_write_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(turn.output_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(turn.reasoning_output_tokens);
        self.hedge_input_tokens = self
            .hedge_input_tokens
            .saturating_add(turn.hedge_input_tokens);
        let Some(cost) = turn.estimated_cost() else {
            self.unpriced_turns = self.unpriced_turns.saturating_add(1);
            return;
        };
        let total = self.cost_usd.unwrap_or_default().nano_usd();
        self.cost_usd = Some(UsdAmount::from_nano_usd(
            total.saturating_add(cost.amount().nano_usd()),
        ));
        if turn.cost_status != CostStatus::ReportedByProvider {
            self.estimated_cost_turns = self.estimated_cost_turns.saturating_add(1);
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.completed_turns == 0
    }

    pub fn add_spend(&mut self, spent: UsdAmount) {
        if spent.nano_usd() == 0 {
            return;
        }
        self.cost_usd = Some(self.cost_usd.unwrap_or_default().saturating_add(spent));
        self.estimated_cost_turns = self.estimated_cost_turns.saturating_add(1);
    }

    pub fn add(&mut self, other: &Self) {
        self.completed_turns = self.completed_turns.saturating_add(other.completed_turns);
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(other.cache_write_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(other.reasoning_output_tokens);
        self.cost_usd = match (self.cost_usd, other.cost_usd) {
            (Some(own), Some(added)) => Some(own.saturating_add(added)),
            (own, added) => own.or(added),
        };
        self.estimated_cost_turns = self
            .estimated_cost_turns
            .saturating_add(other.estimated_cost_turns);
        self.unpriced_turns = self.unpriced_turns.saturating_add(other.unpriced_turns);
        self.hedge_input_tokens = self
            .hedge_input_tokens
            .saturating_add(other.hedge_input_tokens);
    }
}

const fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReportedTurnUsage {
    pub input_tokens: u64,

    pub cached_input_tokens: u64,

    pub cache_write_input_tokens: u64,

    pub output_tokens: u64,

    pub reasoning_output_tokens: u64,

    pub total_tokens: u64,

    pub estimated_cost: Option<EstimatedUsdCost>,

    pub cost_status: CostStatus,
}

impl TurnUsage {
    #[must_use]
    pub fn from_reported(reported: ReportedTurnUsage) -> Self {
        Self {
            input_tokens: reported.input_tokens,
            cached_input_tokens: reported.cached_input_tokens,
            cache_write_input_tokens: reported.cache_write_input_tokens,
            output_tokens: reported.output_tokens,
            reasoning_output_tokens: reported.reasoning_output_tokens,
            total_tokens: reported.total_tokens,
            estimated_cost: reported.estimated_cost.map(Box::new),
            cost_status: reported.cost_status,
            hedge_input_tokens: 0,
        }
    }

    #[must_use]
    pub const fn with_hedge_input_tokens(mut self, tokens: u64) -> Self {
        self.hedge_input_tokens = tokens;
        self
    }

    #[must_use]
    pub const fn hedge_input_tokens(&self) -> u64 {
        self.hedge_input_tokens
    }

    #[must_use]
    pub const fn input_tokens(&self) -> u64 {
        self.input_tokens
    }

    #[must_use]
    pub const fn cached_input_tokens(&self) -> u64 {
        self.cached_input_tokens
    }

    #[must_use]
    pub const fn cache_write_input_tokens(&self) -> u64 {
        self.cache_write_input_tokens
    }

    #[must_use]
    pub const fn output_tokens(&self) -> u64 {
        self.output_tokens
    }

    #[must_use]
    pub const fn reasoning_output_tokens(&self) -> u64 {
        self.reasoning_output_tokens
    }

    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.total_tokens
    }

    #[must_use]
    pub fn estimated_cost(&self) -> Option<&EstimatedUsdCost> {
        self.estimated_cost.as_deref()
    }

    #[must_use]
    pub const fn cost_status(&self) -> CostStatus {
        self.cost_status
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageRecord {
    pub model: String,
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cost_nano_usd: Option<u64>,
}

impl UsageRecord {
    #[must_use]
    pub fn new(model: impl Into<String>, usage: &TurnUsage) -> Self {
        Self {
            model: model.into(),
            input: usage.input_tokens,
            cached_input: usage.cached_input_tokens,
            output: usage.output_tokens,
            reasoning: usage.reasoning_output_tokens,
            cost_nano_usd: usage.estimated_cost().map(|cost| cost.amount().nano_usd()),
        }
    }
}

impl From<&nanocode_types_core::events::RunTerminal> for UsageRecord {
    fn from(run: &nanocode_types_core::events::RunTerminal) -> Self {
        let usage = &run.metrics.usage;
        Self {
            model: run.model.clone(),
            input: usage.input_tokens,
            cached_input: usage.cached_input_tokens,
            output: usage.output_tokens,
            reasoning: usage.reasoning_output_tokens,
            cost_nano_usd: run
                .estimated_cost
                .as_ref()
                .map(|cost| cost.amount().nano_usd()),
        }
    }
}
