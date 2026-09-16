//! Duration, working-time, context, and parallelism DTOs.
use serde::Serialize;
use time::Date;

use crate::model::{
    ActiveTimeSummary, ContextReason, ContextSummary, DurationSummary, Orchestration,
};

use super::ymd;

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::format::json) struct TurnLengthDto {
    turns: usize,
    p50_ms: Option<u64>,
    p90_ms: Option<u64>,
    p95_ms: Option<u64>,
    max_ms: Option<u64>,
    interrupted: usize,
    buckets: Vec<CountDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::format::json) struct CountDto {
    pub(in crate::format::json) label: String,
    pub(in crate::format::json) turns: usize,
}

impl TurnLengthDto {
    /// Interruptions are counted independently of completed turns, so a
    /// window with interrupts but no finished turn still reports them.
    pub(in crate::format::json) fn new(
        duration: Option<&DurationSummary>,
        interrupted: usize,
    ) -> Self {
        Self {
            turns: duration.map_or(0, |d| d.count),
            p50_ms: duration.map(|d| d.p50_ms),
            p90_ms: duration.map(|d| d.p90_ms),
            p95_ms: duration.map(|d| d.p95_ms),
            max_ms: duration.map(|d| d.max_ms),
            interrupted,
            buckets: duration
                .into_iter()
                .flat_map(|d| d.buckets.iter())
                .map(|bucket| CountDto {
                    label: bucket.label.clone(),
                    turns: bucket.count,
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::format::json) struct WorkingTimeDto {
    active_ms: u64,
    human_wait_ms: u64,
    turns: usize,
    context_tokens_per_minute: Option<u64>,
    model_share: Option<f64>,
    measured_ms: u64,
    peak_day: Option<ActiveDayDto>,
    pace: Option<PaceDto>,
    daily: Vec<ActiveDayDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ActiveDayDto {
    #[serde(with = "ymd")]
    date: Date,
    active_ms: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(
    clippy::struct_field_names,
    reason = "The wire contract includes units on every duration."
)]
struct PaceDto {
    p50_ms: u64,
    p90_ms: u64,
    mean_ms: u64,
}

impl WorkingTimeDto {
    pub(in crate::format::json) fn new(time: &ActiveTimeSummary) -> Self {
        Self {
            active_ms: time.active_ms,
            human_wait_ms: time.human_wait_ms,
            turns: time.turns,
            context_tokens_per_minute: time.context_per_minute(),
            model_share: time.model_share(),
            measured_ms: time.measured_ms,
            peak_day: time
                .peak_day()
                .map(|(date, active_ms)| ActiveDayDto { date, active_ms }),
            pace: time.pace_percentiles().zip(time.pace_mean_ms()).map(
                |((p50_ms, p90_ms), mean_ms)| PaceDto {
                    p50_ms,
                    p90_ms,
                    mean_ms,
                },
            ),
            daily: time
                .daily_active_ms
                .iter()
                .map(|&(date, active_ms)| ActiveDayDto { date, active_ms })
                .collect(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::format::json) struct ContextDto {
    calls: usize,
    context_tokens: u64,
    cached_tokens: u64,
    cached_share: f64,
    effective_tokens: u64,
    bands: Vec<BandDto>,
    expired: Option<ReasonDto>,
    cold_start: Option<ReasonDto>,
    uncached: ReasonDto,
    other_effective_tokens: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct BandDto {
    label: String,
    calls: usize,
    cached_effective_tokens: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ReasonDto {
    calls: usize,
    effective_tokens: u64,
}

impl From<&ContextReason> for ReasonDto {
    fn from(reason: &ContextReason) -> Self {
        Self {
            calls: reason.calls,
            effective_tokens: reason.effective,
        }
    }
}

impl ContextDto {
    pub(in crate::format::json) fn new(context: &ContextSummary) -> Self {
        Self {
            calls: context.calls,
            context_tokens: context.context_tokens,
            cached_tokens: context.cached_tokens,
            cached_share: context.cached_share().clamp(0.0, 1.0),
            effective_tokens: context.effective_tokens,
            bands: context
                .bands
                .iter()
                .map(|band| BandDto {
                    label: band.label.clone(),
                    calls: band.calls,
                    cached_effective_tokens: band.cached_effective,
                })
                .collect(),
            expired: context.expired.as_ref().map(ReasonDto::from),
            cold_start: context.cold_start.as_ref().map(ReasonDto::from),
            uncached: ReasonDto::from(&context.uncached),
            other_effective_tokens: context.unclassified_effective,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::format::json) struct ParallelDto {
    avg_concurrency: f64,
    peak_concurrency: usize,
    time_by_level: Vec<LevelDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct LevelDto {
    level: &'static str,
    active_ms: u64,
}

impl ParallelDto {
    pub(in crate::format::json) fn new(parallel: &Orchestration) -> Self {
        Self {
            avg_concurrency: parallel.avg_concurrency,
            peak_concurrency: parallel.peak_concurrency,
            time_by_level: ["1", "2", "3", "4-6", "7-9", "10+"]
                .into_iter()
                .zip(parallel.time_by_level)
                .map(|(level, seconds)| LevelDto {
                    level,
                    active_ms: seconds.saturating_mul(1_000),
                })
                .collect(),
        }
    }
}
