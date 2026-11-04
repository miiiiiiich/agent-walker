use serde::Serialize;
use time::UtcOffset;

use crate::model::AppSummary;

use super::summary::{CodenameDto, SummaryDto};
use super::{SCHEMA_VERSION, WindowDto};

/// Only what the summary shows, under the same names as in the full export. A
/// caller drawing a codename and some numbers has no use for the project paths
/// and tool names the export also carries, and should not have to hold them.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::format::json) struct BriefDto<'a> {
    schema_version: u32,
    window: WindowDto,
    total: TotalDto,
    card_svg: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct TotalDto {
    codename: CodenameDto,
    tokens: TokensDto,
    cost: CostDto,
    sessions: usize,
    active_days: usize,
    working_time: Option<WorkingTimeDto>,
    context: Option<ContextDto>,
    parallel: ParallelDto,
    turn_length: TurnLengthDto,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct TokensDto {
    total: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct CostDto {
    estimated_usd: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct WorkingTimeDto {
    active_ms: u64,
    tokens_per_minute: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ContextDto {
    cached_share: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ParallelDto {
    avg_concurrency: f64,
    peak_concurrency: usize,
    four_plus_share: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct TurnLengthDto {
    turns: usize,
    p50_ms: Option<u64>,
    p90_ms: Option<u64>,
}

impl<'a> BriefDto<'a> {
    pub(in crate::format::json) fn new(
        report: &AppSummary,
        offset: UtcOffset,
        card_svg: &'a str,
    ) -> Self {
        let total = SummaryDto::new(&report.combined, offset);
        Self {
            schema_version: SCHEMA_VERSION,
            window: WindowDto::new(report),
            total: TotalDto {
                codename: total.codename,
                tokens: TokensDto {
                    total: total.tokens.total,
                },
                cost: CostDto {
                    estimated_usd: total.cost.estimated_usd,
                },
                sessions: total.sessions,
                active_days: total.active_days,
                working_time: total.working_time.map(|working| WorkingTimeDto {
                    active_ms: working.active_ms,
                    tokens_per_minute: working.tokens_per_minute,
                }),
                context: total.context.map(|context| ContextDto {
                    cached_share: context.cached_share,
                }),
                parallel: ParallelDto {
                    avg_concurrency: total.parallel.avg_concurrency,
                    peak_concurrency: total.parallel.peak_concurrency,
                    four_plus_share: total.parallel.four_plus_share,
                },
                turn_length: TurnLengthDto {
                    turns: total.turn_length.turns,
                    p50_ms: total.turn_length.p50_ms,
                    p90_ms: total.turn_length.p90_ms,
                },
            },
            card_svg,
        }
    }
}
