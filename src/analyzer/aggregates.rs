use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use time::{Date, UtcOffset};

use crate::model::{AgentStat, ModelStat, ProjectStat, SourceKind, TokenUsage};

#[derive(Default)]
pub(super) struct Aggregates {
    pub(super) total_usage: TokenUsage,
    pub(super) daily_usage: BTreeMap<Date, TokenUsage>,
    pub(super) model_daily_usage: BTreeMap<(Date, String), TokenUsage>,
    /// Provider-reported cost summed per (date, model). A key is present only
    /// when at least one event for it carried a reported cost; absence means
    /// "price from `LiteLLM`".
    pub(super) model_daily_reported: BTreeMap<(Date, String), f64>,
    /// Per (date, model) usage from events with NO reported cost — the portion
    /// still priced from `LiteLLM` even when the same model-day also has reported
    /// cost from another provider.
    pub(super) model_daily_unreported: BTreeMap<(Date, String), TokenUsage>,
    pub(super) model_map: HashMap<String, ModelAccumulator>,
    pub(super) agent_map: HashMap<String, AgentAccumulator>,
    pub(super) tool_map: HashMap<String, usize>,
    pub(super) project_map: HashMap<String, ProjectAccumulator>,
    pub(super) period_sessions: HashSet<String>,
    pub(super) daily_session_ids: BTreeMap<Date, HashSet<String>>,
    pub(super) active_dates: BTreeSet<Date>,
    pub(super) hourly_usage: [u64; 24],
    pub(super) previous_total_volume: u64,
}

impl Aggregates {
    pub(super) fn add_usage_event(
        &mut self,
        event: &crate::model::UsageEvent,
        period_start: Date,
        period_end: Date,
        previous_start: Date,
        local_offset: UtcOffset,
    ) {
        let Some(timestamp) = event.timestamp else {
            return;
        };
        let timestamp = timestamp.to_offset(local_offset);
        let date = timestamp.date();
        if date >= previous_start && date < period_start {
            self.previous_total_volume = self
                .previous_total_volume
                .saturating_add(event.usage.token_volume());
            return;
        }
        if date < period_start || date > period_end {
            return;
        }

        self.total_usage.add_assign(&event.usage);
        if event.usage.token_volume() > 0 {
            self.active_dates.insert(date);
        }
        if let Some(daily) = self.daily_usage.get_mut(&date) {
            daily.add_assign(&event.usage);
        }
        let hour = usize::from(timestamp.hour());
        self.hourly_usage[hour] =
            self.hourly_usage[hour].saturating_add(event.usage.token_volume());

        if let Some(session_id) = &event.session_id {
            self.period_sessions.insert(session_id.clone());
        }

        let model_name = event.model.clone().unwrap_or_else(|| "unknown".to_owned());
        self.model_map
            .entry(model_name.clone())
            .or_default()
            .add(&event.usage, event.reported_cost_usd);
        self.model_daily_usage
            .entry((date, model_name.clone()))
            .or_default()
            .add_assign(&event.usage);
        if let Some(cost) = event.reported_cost_usd {
            *self
                .model_daily_reported
                .entry((date, model_name))
                .or_insert(0.0) += cost;
        } else {
            self.model_daily_unreported
                .entry((date, model_name))
                .or_default()
                .add_assign(&event.usage);
        }

        if let Some(project) = &event.project {
            self.project_map
                .entry(project.clone())
                .or_default()
                .add(&event.usage);
        }

        if event.source_kind == SourceKind::Subagent || event.attribution_agent.is_some() {
            let agent_name = event
                .attribution_agent
                .clone()
                .unwrap_or_else(|| "subagent".to_owned());
            self.agent_map
                .entry(agent_name)
                .or_default()
                .add_usage(&event.usage);
        }
    }

    pub(super) fn add_tool_event(
        &mut self,
        event: &crate::model::ToolEvent,
        period_start: Date,
        period_end: Date,
        local_offset: UtcOffset,
    ) {
        let Some(timestamp) = event.timestamp else {
            return;
        };
        let date = timestamp.to_offset(local_offset).date();
        if date < period_start || date > period_end {
            return;
        }
        *self.tool_map.entry(event.tool_name.clone()).or_default() += 1;
        if let Some(session_id) = &event.session_id {
            self.period_sessions.insert(session_id.clone());
        }
        if event.tool_name == "Agent"
            && let Some(subagent_type) = &event.subagent_type
        {
            self.agent_map
                .entry(subagent_type.clone())
                .or_default()
                .add_call();
        }
    }

    pub(super) fn add_session_touch(
        &mut self,
        touch: &crate::model::SessionTouch,
        period_start: Date,
        period_end: Date,
        previous_start: Date,
        local_offset: UtcOffset,
    ) {
        let date = touch.timestamp.to_offset(local_offset).date();
        if date >= previous_start && date < period_start {
            return;
        }
        if date < period_start || date > period_end {
            return;
        }
        self.active_dates.insert(date);
        self.period_sessions.insert(touch.session_id.clone());
        self.daily_session_ids
            .entry(date)
            .or_default()
            .insert(touch.session_id.clone());
    }
}

#[derive(Default)]
pub(super) struct ProjectAccumulator {
    usage: TokenUsage,
}

impl ProjectAccumulator {
    fn add(&mut self, usage: &TokenUsage) {
        self.usage.add_assign(usage);
    }

    pub(super) fn into_stat(self, name: String) -> ProjectStat {
        ProjectStat {
            name,
            usage: self.usage,
        }
    }
}

#[derive(Default)]
pub(super) struct ModelAccumulator {
    usage: TokenUsage,
    /// Usage from events with no reported cost (priced via `LiteLLM`).
    unreported_usage: TokenUsage,
    events: usize,
    reported_cost: Option<f64>,
}

impl ModelAccumulator {
    fn add(&mut self, usage: &TokenUsage, reported_cost: Option<f64>) {
        self.usage.add_assign(usage);
        self.events += 1;
        if let Some(cost) = reported_cost {
            self.reported_cost = Some(self.reported_cost.unwrap_or(0.0) + cost);
        } else {
            self.unreported_usage.add_assign(usage);
        }
    }

    pub(super) fn into_stat(self, name: String) -> ModelStat {
        ModelStat {
            name,
            usage: self.usage,
            unreported_usage: self.unreported_usage,
            events: self.events,
            reported_cost_usd: self.reported_cost,
        }
    }
}

#[derive(Default)]
pub(super) struct AgentAccumulator {
    usage: TokenUsage,
    calls: usize,
}

impl AgentAccumulator {
    fn add_usage(&mut self, usage: &TokenUsage) {
        self.usage.add_assign(usage);
    }

    fn add_call(&mut self) {
        self.calls += 1;
    }

    pub(super) fn into_stat(self, name: String) -> AgentStat {
        AgentStat {
            name,
            usage: self.usage,
            calls: self.calls,
        }
    }
}
