//! Dashboard aggregates, separate from detailed event rows.
use std::collections::BTreeMap;

use serde::Serialize;
use time::{Date, OffsetDateTime, UtcOffset};

use crate::cost::CostTally;
use crate::format::short_model_name;
use crate::model::Summary;

use super::histories::{CreditsDto, LimitsDto, ModesDto};
use super::panels::{ContextDto, ParallelDto, TurnLengthDto, WorkingTimeDto};
use super::{TokensDto, ymd};

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::format::json) struct SummaryDto {
    codename: CodenameDto,
    tokens: TokensDto,
    cost: CostDto,
    sessions: usize,
    active_days: usize,
    streak: StreakDto,
    daily: Vec<DailyDto>,
    hourly_profile: Vec<HourDto>,
    models: Vec<ModelDto>,
    projects: Vec<ProjectDto>,
    tools: Vec<ToolDto>,
    subagents: Vec<SubagentDto>,
    skills: Vec<SkillDto>,
    turn_length: TurnLengthDto,
    working_time: Option<WorkingTimeDto>,
    context: Option<ContextDto>,
    parallel: ParallelDto,
    modes: Option<ModesDto>,
    limits: Option<LimitsDto>,
    credits: Option<CreditsDto>,
    signal: SignalDto,
}

impl SummaryDto {
    #[allow(
        clippy::too_many_lines,
        reason = "Flat mapping of the output contract's named fields."
    )]
    pub(in crate::format::json) fn new(summary: &Summary, offset: UtcOffset) -> Self {
        let codename = crate::codename::for_summary(summary);
        let daily_sessions = summary
            .daily_sessions
            .iter()
            .map(|day| (day.date, day.sessions))
            .collect::<BTreeMap<_, _>>();
        Self {
            codename: CodenameDto {
                rank: codename.rank.letters().unwrap_or("unranked").to_owned(),
                ops: codename.ops,
                animal: codename.animal,
            },
            tokens: TokensDto::from(&summary.total_usage),
            cost: CostDto::new(summary),
            sessions: summary.sessions,
            active_days: summary.active_days,
            streak: StreakDto {
                current_days: summary.current_streak_days,
                longest_days: summary.longest_streak_days,
            },
            daily: summary
                .daily
                .iter()
                .map(|day| DailyDto {
                    date: day.date,
                    tokens: day.usage.token_volume(),
                    sessions: daily_sessions.get(&day.date).copied().unwrap_or(0),
                })
                .collect(),
            hourly_profile: (0_u8..24)
                .zip(summary.hourly_usage)
                .map(|(hour, tokens)| HourDto { hour, tokens })
                .collect(),
            models: summary
                .models
                .iter()
                .map(|model| ModelDto {
                    id: model.name.clone(),
                    label: short_model_name(&model.name),
                    tokens: model.usage.token_volume(),
                    share: ratio(
                        model.usage.token_volume(),
                        summary.total_usage.token_volume(),
                    ),
                    calls: model.events,
                })
                .collect(),
            projects: summary
                .projects
                .iter()
                .map(|project| ProjectDto {
                    id: project.path.clone(),
                    label: project.name.clone(),
                    tokens: project.usage.token_volume(),
                })
                .collect(),
            tools: summary
                .tools
                .iter()
                .map(|tool| ToolDto {
                    name: tool.name.clone(),
                    calls: tool.calls,
                })
                .collect(),
            subagents: summary
                .agents
                .iter()
                .map(|agent| SubagentDto {
                    name: agent.name.clone(),
                    tokens: agent.usage.token_volume(),
                    calls: agent.calls,
                })
                .collect(),
            skills: summary
                .skills
                .iter()
                .map(|skill| SkillDto {
                    name: skill.name.clone(),
                    tokens: skill.usage.token_volume(),
                })
                .collect(),
            turn_length: TurnLengthDto::new(
                summary.completion_duration.as_ref(),
                summary.interrupted,
            ),
            working_time: summary.active_time.as_ref().map(WorkingTimeDto::new),
            context: summary.context.as_ref().map(ContextDto::new),
            parallel: ParallelDto::new(&summary.orchestration),
            modes: (!summary.modes.is_empty()).then(|| ModesDto::new(&summary.modes)),
            limits: summary.limits.as_ref().map(LimitsDto::new),
            credits: summary.credits.as_ref().map(CreditsDto::new),
            signal: SignalDto {
                favorite_model: summary.favorite_model.clone(),
                most_active_day: summary.most_active_day.as_ref().map(|day| DayTokensDto {
                    date: day.date,
                    tokens: day.usage.token_volume(),
                }),
                busiest_hour: summary
                    .busiest_hour
                    .map(|(hour, tokens)| HourDto { hour, tokens }),
                longest_session: summary.longest_session.as_ref().map(|span| SessionSpanDto {
                    started_at: span.started_at.to_offset(offset),
                    ended_at: span.ended_at.to_offset(offset),
                    duration_ms: u64::try_from(
                        (span.ended_at - span.started_at)
                            .whole_milliseconds()
                            .max(0),
                    )
                    .unwrap_or(u64::MAX),
                }),
            },
        }
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Ratios are approximate; integer counters stay exact."
)]
fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        (numerator as f64 / denominator as f64).clamp(0.0, 1.0)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct CodenameDto {
    rank: String,
    ops: &'static str,
    animal: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct CostDto {
    estimated_usd: Option<f64>,
    reported_usd: f64,
    unpriced_tokens: u64,
}

impl CostDto {
    fn new(summary: &Summary) -> Self {
        let mut estimated = CostTally::default();
        let mut reported_usd = 0.0;
        for model in &summary.models {
            estimated.add(&model.name, &model.unreported_usage, None);
            reported_usd += model.reported_cost_usd.unwrap_or(0.0);
        }
        Self {
            estimated_usd: estimated.complete_usd(),
            reported_usd,
            unpriced_tokens: estimated.unpriced_volume,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct StreakDto {
    current_days: usize,
    longest_days: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct DailyDto {
    #[serde(with = "ymd")]
    date: Date,
    tokens: u64,
    sessions: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct HourDto {
    hour: u8,
    tokens: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ModelDto {
    id: String,
    label: String,
    tokens: u64,
    share: f64,
    calls: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ProjectDto {
    id: String,
    label: String,
    tokens: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ToolDto {
    name: String,
    calls: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct SubagentDto {
    name: String,
    tokens: u64,
    calls: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct SkillDto {
    name: String,
    tokens: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct SignalDto {
    favorite_model: Option<String>,
    most_active_day: Option<DayTokensDto>,
    busiest_hour: Option<HourDto>,
    longest_session: Option<SessionSpanDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct DayTokensDto {
    #[serde(with = "ymd")]
    date: Date,
    tokens: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct SessionSpanDto {
    #[serde(with = "time::serde::rfc3339")]
    started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    ended_at: OffsetDateTime,
    duration_ms: u64,
}
