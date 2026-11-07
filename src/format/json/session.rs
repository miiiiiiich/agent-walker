use serde::Serialize;
use time::{OffsetDateTime, UtcOffset};

use crate::analyzer::{LONG_TURN_MS, SessionReport};
use crate::timestamp::to_local;

use super::SCHEMA_VERSION;
use super::panels::PaceDto;
use super::summary::{SessionNames, SummaryDto, ratio};

/// What `session` shows, under the same names as in the full export.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::format::json) struct SessionDocDto {
    schema_version: u32,
    session: SessionSpanDto,
    total: TotalDto,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct SessionSpanDto {
    id: String,
    #[serde(with = "time::serde::rfc3339")]
    first_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    last_at: OffsetDateTime,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct TotalDto {
    cost: CostDto,
    working_time: Option<WorkingTimeDto>,
    turn_length: TurnLengthDto,
    context: Option<ContextDto>,
    tools: Vec<NameCallsDto>,
    subagents: Vec<NameCallsDto>,
    skills: Vec<NameDto>,
    models: Vec<ModelDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct CostDto {
    estimated_usd: Option<f64>,
    unpriced_tokens: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct WorkingTimeDto {
    active_ms: u64,
    human_wait_ms: u64,
    model_share: Option<f64>,
    measured_ms: u64,
    pace: Option<PaceDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ContextDto {
    cached_share: f64,
    expired: Option<CallsDto>,
    cold_start: Option<CallsDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct CallsDto {
    calls: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct TurnLengthDto {
    turns: usize,
    p50_ms: Option<u64>,
    p90_ms: Option<u64>,
    max_ms: Option<u64>,
    long_from_ms: u64,
    long_turns: usize,
    /// One per turn, in the order they ran.
    lengths_ms: Vec<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct NameCallsDto {
    name: String,
    calls: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct NameDto {
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ModelDto {
    label: String,
    share: f64,
}

impl SessionDocDto {
    pub(in crate::format::json) fn new(report: &SessionReport, offset: UtcOffset) -> Self {
        let SessionReport {
            id,
            first_at,
            last_at,
            summary,
            turn_lengths_ms,
            models,
        } = report;
        let full = SummaryDto::new(summary, offset);
        let SessionNames {
            tools,
            subagents,
            skills,
        } = full.names();
        let duration = summary.completion_duration.as_ref();
        Self {
            schema_version: SCHEMA_VERSION,
            session: SessionSpanDto {
                id: id.clone(),
                first_at: to_local(*first_at, offset).unwrap_or(*first_at),
                last_at: to_local(*last_at, offset).unwrap_or(*last_at),
            },
            total: TotalDto {
                cost: CostDto {
                    estimated_usd: full.cost.estimated_usd,
                    unpriced_tokens: full.cost.unpriced_tokens,
                },
                working_time: full.working_time.map(|working| WorkingTimeDto {
                    active_ms: working.active_ms,
                    human_wait_ms: working.human_wait_ms,
                    model_share: working.model_share,
                    measured_ms: working.measured_ms,
                    pace: working.pace,
                }),
                turn_length: TurnLengthDto {
                    turns: duration.map_or(0, |d| d.count),
                    p50_ms: duration.map(|d| d.p50_ms),
                    p90_ms: duration.map(|d| d.p90_ms),
                    max_ms: duration.map(|d| d.max_ms),
                    long_from_ms: LONG_TURN_MS,
                    long_turns: duration.map_or(0, crate::model::DurationSummary::long_turns),
                    lengths_ms: turn_lengths_ms.clone(),
                },
                context: full.context.map(|context| ContextDto {
                    cached_share: context.cached_share,
                    expired: context.expired.map(|reason| CallsDto {
                        calls: reason.calls,
                    }),
                    cold_start: context.cold_start.map(|reason| CallsDto {
                        calls: reason.calls,
                    }),
                }),
                tools: tools
                    .into_iter()
                    .map(|(name, calls)| NameCallsDto { name, calls })
                    .collect(),
                subagents: subagents
                    .into_iter()
                    .map(|(name, calls)| NameCallsDto { name, calls })
                    .collect(),
                skills: skills.into_iter().map(|name| NameDto { name }).collect(),
                models: models
                    .iter()
                    .map(|model| ModelDto {
                        label: model.label.clone(),
                        share: ratio(model.tokens, summary.total_usage.token_volume()),
                    })
                    .collect(),
            },
        }
    }
}
