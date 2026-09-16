//! Dated, normalized collector records. Undated and out-of-window rows are dropped.
use serde::Serialize;
use time::{Date, OffsetDateTime};

use crate::model::{Collection, Provider, SourceKind};

use super::{EventWindow, TokensDto, ymd};

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::format::json) struct EventsDto {
    usage_events: Vec<UsageDto>,
    turns: Vec<TurnDto>,
    sessions: Vec<SessionDto>,
    tools: Vec<ToolDto>,
    rate_limits: Vec<RateLimitDto>,
    credits: Vec<CreditDto>,
    efforts: Vec<EffortDto>,
    modes: Vec<ModeDto>,
    permissions: Vec<PermissionDto>,
    interrupts: Vec<InterruptDto>,
    pace: Vec<PaceDto>,
}

impl EventsDto {
    #[allow(
        clippy::too_many_lines,
        reason = "Each collector list maps independently to its wire rows."
    )]
    pub(in crate::format::json) fn new(collection: &Collection, window: EventWindow) -> Self {
        Self {
            usage_events: window.rows(
                &collection.usage_events,
                |e| e.timestamp,
                |e, at| UsageDto {
                    at,
                    session_id: e.session_id.clone(),
                    model: e.model.clone(),
                    source: source_name(e.source_kind),
                    project: e.project.clone(),
                    skill: e.attribution_skill.clone(),
                    subagent: e.attribution_agent.clone(),
                    tokens: TokensDto::from(&e.usage),
                    reported_cost_usd: e.reported_cost_usd,
                },
            ),
            turns: window.rows(
                &collection.duration_events,
                |e| e.timestamp,
                |e, at| TurnDto {
                    at,
                    at_marks: if collection.provider == Provider::OpenCode {
                        "start"
                    } else {
                        "end"
                    },
                    session_id: e.session_id.clone(),
                    duration_ms: e.duration_ms,
                    human_wait_ms: e.human_wait_ms,
                    model_ms: e.model_ms,
                    status: e.status.clone(),
                },
            ),
            sessions: sessions(collection, window),
            tools: window.rows(
                &collection.tool_events,
                |e| e.timestamp,
                |e, at| ToolDto {
                    at,
                    session_id: e.session_id.clone(),
                    name: e.tool_name.clone(),
                    subagent_type: e.subagent_type.clone(),
                    source: source_name(e.source_kind),
                },
            ),
            rate_limits: window.rows(
                &collection.rate_limit_samples,
                |e| Some(e.timestamp),
                |e, at| RateLimitDto {
                    at,
                    used_percent: e.used_percent,
                },
            ),
            credits: window.rows(
                &collection.credit_samples,
                |e| Some(e.timestamp),
                |e, at| CreditDto {
                    at,
                    nano_aiu: e.nano_aiu,
                },
            ),
            efforts: window.rows(
                &collection.effort_events,
                |e| e.timestamp,
                |e, at| EffortDto {
                    at,
                    effort: e.effort.clone(),
                },
            ),
            modes: window.rows(
                &collection.mode_events,
                |e| e.timestamp,
                |e, at| ModeDto {
                    at,
                    thinking: e.has_thinking,
                    fast: e.fast,
                },
            ),
            permissions: window.rows(
                &collection.permission_events,
                |e| e.timestamp,
                |e, at| PermissionDto {
                    at,
                    mode: e.mode.clone(),
                },
            ),
            interrupts: window.rows(
                &collection.interrupt_events,
                |e| e.timestamp,
                |_, at| InterruptDto { at },
            ),
            pace: window.rows(
                &collection.pace_events,
                |e| e.timestamp,
                |e, at| PaceDto {
                    at,
                    gap_ms: e.gap_ms,
                },
            ),
        }
    }
}

fn source_name(source: SourceKind) -> &'static str {
    match source {
        SourceKind::Main => "main",
        SourceKind::Subagent => "subagent",
    }
}

fn sessions(collection: &Collection, window: EventWindow) -> Vec<SessionDto> {
    let bounds =
        crate::analyzer::session_day_bounds(collection, window.start, window.end, window.offset);
    let mut rows = bounds
        .into_iter()
        .map(|((session_id, date), (first_at, last_at))| SessionDto {
            session_id: session_id.to_owned(),
            date,
            first_at: first_at.to_offset(window.offset),
            last_at: last_at.to_offset(window.offset),
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        left.first_at
            .cmp(&right.first_at)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    rows
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct UsageDto {
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
    session_id: Option<String>,
    model: Option<String>,
    source: &'static str,
    project: Option<String>,
    skill: Option<String>,
    subagent: Option<String>,
    tokens: TokensDto,
    reported_cost_usd: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct TurnDto {
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
    at_marks: &'static str,
    session_id: Option<String>,
    duration_ms: u64,
    human_wait_ms: u64,
    model_ms: Option<u64>,
    status: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct SessionDto {
    session_id: String,
    #[serde(with = "ymd")]
    date: Date,
    #[serde(with = "time::serde::rfc3339")]
    first_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    last_at: OffsetDateTime,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ToolDto {
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
    session_id: Option<String>,
    name: String,
    subagent_type: Option<String>,
    source: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct RateLimitDto {
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
    used_percent: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct CreditDto {
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
    nano_aiu: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct EffortDto {
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
    effort: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ModeDto {
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
    thinking: bool,
    fast: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct PermissionDto {
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
    mode: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct InterruptDto {
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct PaceDto {
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
    gap_ms: u64,
}
