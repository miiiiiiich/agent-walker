//! What the analyzer produces and the UI consumes: per-day stats, panel
//! histories, and the Summary/AppSummary the dashboard renders. Not part of
//! the parse cache — changes here affect display, not stored data.
use std::path::PathBuf;

use time::{Date, OffsetDateTime};

use super::{Provider, ScanStats, TokenUsage};

#[derive(Debug, Clone)]
pub struct DailyStat {
    pub date: Date,
    pub usage: TokenUsage,
}

#[derive(Debug, Clone)]
pub struct DailySessions {
    pub date: Date,
    pub sessions: usize,
}

#[derive(Debug, Clone)]
pub struct ModelDailyStat {
    pub date: Date,
    pub model: String,
    pub usage: TokenUsage,
    /// The subset of `usage` from events that carried NO provider-reported cost,
    /// i.e. the tokens that must be priced from `LiteLLM`. When a model name is
    /// shared on the same day by a reporting provider (Cursor) and a
    /// non-reporting one (Claude Code), this keeps the two cost paths additive.
    pub unreported_usage: TokenUsage,
    /// Summed provider-reported cost for this model-day, if any event carried
    /// one (see `UsageEvent::reported_cost_usd`). Added to the `LiteLLM` price of
    /// `unreported_usage`.
    pub reported_cost_usd: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct ModelStat {
    pub name: String,
    pub usage: TokenUsage,
    /// Subset of `usage` priced from `LiteLLM` (events with no reported cost) —
    /// see `ModelDailyStat::unreported_usage`.
    pub unreported_usage: TokenUsage,
    pub events: usize,
    /// Summed provider-reported cost for this model over the period, if any
    /// event carried one. Added to the `LiteLLM` price of `unreported_usage`.
    pub reported_cost_usd: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct AgentStat {
    pub name: String,
    pub usage: TokenUsage,
    pub calls: usize,
}

/// Per-skill token volume over the fixed 30-day window (Claude
/// `attributionSkill`). TUI-only — must never reach the share card.
#[derive(Debug, Clone)]
pub struct SkillStat {
    pub name: String,
    pub usage: TokenUsage,
}

/// One day of the LIMITS history. `NoUse` = no provider activity that day;
/// `NoSample` = activity but the CLI recorded no rate-limit snapshot (older
/// versions); `Measured` = the day's peak `used_percent`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LimitDay {
    NoUse,
    NoSample,
    Measured(f64),
}

/// Daily-peak history of the plan's 5h window over the fixed 30-day window,
/// oldest day first.
#[derive(Debug, Clone)]
pub struct LimitsHistory {
    pub days: Vec<(Date, LimitDay)>,
    pub peak: Option<(Date, f64)>,
}

/// Daily AI-credit spend over the fixed 30-day window (Copilot). Historical
/// by design — spend that already happened, not a remaining-quota meter.
#[derive(Debug, Clone)]
pub struct CreditsHistory {
    /// One entry per window day; 0.0 = no recorded spend.
    pub days: Vec<(Date, f64)>,
    pub total: f64,
    pub peak: Option<(Date, f64)>,
}

/// Mode usage over the fixed 30-day window: how the user lets the model
/// think. Claude: thinking-block fire rate (+ fast mode when used) and the
/// reasoning-effort distribution (top-level `effort`, CLI v2.1.212+);
/// Codex: reasoning-effort distribution.
#[derive(Debug, Clone, Default)]
pub struct ModesSummary {
    pub assistant_turns: usize,
    pub thinking_turns: usize,
    pub fast_turns: usize,
    /// (effort label, turns), sorted by turns descending.
    pub efforts: Vec<(String, usize)>,
    /// (permission-mode label, turns), sorted by turns descending — Claude's
    /// `permissionMode`, Codex's `approval_policy`.
    pub permissions: Vec<(String, usize)>,
}

impl ModesSummary {
    pub fn is_empty(&self) -> bool {
        self.assistant_turns == 0 && self.efforts.is_empty() && self.permissions.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct ToolStat {
    pub name: String,
    pub calls: usize,
}

#[derive(Debug, Clone)]
pub struct ProjectStat {
    pub name: String,
    pub usage: TokenUsage,
}

#[derive(Debug, Clone)]
pub struct SessionSpan {
    pub started_at: OffsetDateTime,
    pub ended_at: OffsetDateTime,
}

impl SessionSpan {
    pub fn duration_secs(&self) -> i64 {
        (self.ended_at - self.started_at).whole_seconds().max(0)
    }
}

/// Completed-turn duration statistics. `Some` on a `Summary` guarantees at
/// least one completed turn — interruptions live on `Summary::interrupted`,
/// not here.
#[derive(Debug, Clone)]
pub struct DurationSummary {
    pub count: usize,
    pub p50_ms: u64,
    pub p90_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
    pub buckets: Vec<DurationBucket>,
}

#[derive(Debug, Clone)]
pub struct DurationBucket {
    pub label: String,
    pub count: usize,
}

/// TIME panel data over the fixed 30-day window: how long the agent was
/// working (turn lengths with the human's answer time removed) and how much
/// context it read per minute of that. `Some` whenever a turn completed in
/// the window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveTimeSummary {
    /// Completed turns in the window.
    pub turns: usize,
    /// Turn lengths minus `human_wait_ms`, summed.
    pub active_ms: u64,
    /// Time inside those turns spent waiting on the human (`AskUserQuestion`).
    pub human_wait_ms: u64,
    /// Input-side tokens (uncached input + cache writes + cache reads) over
    /// the same window — what every call re-reads, so the per-minute rate
    /// tracks how long a context the agent was dragging.
    pub context_tokens: u64,
    /// Window length in days, for the per-day average.
    pub window_days: u16,
    /// Gaps before each human prompt (previous turn's last activity → the
    /// prompt), sorted ascending, under the 30-minute cutoff. Kept raw so
    /// the Total tab can take percentiles across providers.
    pub pace_gaps_ms: Vec<u64>,
    /// Working time per local day (turn end date), ascending by date, only
    /// days with any. Feeds the peak-day row and, later, a daily chart.
    pub daily_active_ms: Vec<(Date, u64)>,
}

impl ActiveTimeSummary {
    pub fn active_per_day_ms(&self) -> u64 {
        self.active_ms / u64::from(self.window_days.max(1))
    }

    /// The day the agent worked longest.
    pub fn peak_day(&self) -> Option<(Date, u64)> {
        self.daily_active_ms
            .iter()
            .copied()
            .max_by_key(|(_, active_ms)| *active_ms)
    }

    /// Your pace, on average — the mean gap before a prompt.
    pub fn pace_mean_ms(&self) -> Option<u64> {
        if self.pace_gaps_ms.is_empty() {
            return None;
        }
        let total: u128 = self.pace_gaps_ms.iter().map(|&gap| u128::from(gap)).sum();
        Some(u64::try_from(total / self.pace_gaps_ms.len() as u128).unwrap_or(u64::MAX))
    }

    /// Your pace: the median and p90 gap before a prompt; `None` without
    /// any recorded gap.
    pub fn pace_percentiles(&self) -> Option<(u64, u64)> {
        if self.pace_gaps_ms.is_empty() {
            return None;
        }
        let at = |percentile: usize| {
            let rank = self
                .pace_gaps_ms
                .len()
                .saturating_mul(percentile)
                .div_ceil(100);
            let index = rank.saturating_sub(1).min(self.pace_gaps_ms.len() - 1);
            self.pace_gaps_ms[index]
        };
        Some((at(50), at(90)))
    }

    /// Context tokens per active minute; `None` under a minute of activity.
    /// Divides in milliseconds — a 90-second window is 1.5 minutes, not 1.
    pub fn context_per_minute(&self) -> Option<u64> {
        if self.active_ms < 60_000 {
            return None;
        }
        let rate = u128::from(self.context_tokens) * 60_000 / u128::from(self.active_ms);
        Some(u64::try_from(rate).unwrap_or(u64::MAX))
    }

    /// The Total tab's record: a sum over the providers that measured any
    /// working time. Providers with tokens but no turn durations (Cursor,
    /// Antigravity) stay out — their tokens would inflate the rate without
    /// adding a minute to divide by.
    pub fn merged<'a>(parts: impl IntoIterator<Item = &'a ActiveTimeSummary>) -> Option<Self> {
        let mut out: Option<Self> = None;
        for part in parts.into_iter().filter(|part| part.turns > 0) {
            let acc = out.get_or_insert_with(|| Self {
                window_days: part.window_days,
                ..Self::default()
            });
            acc.turns += part.turns;
            acc.active_ms = acc.active_ms.saturating_add(part.active_ms);
            acc.human_wait_ms = acc.human_wait_ms.saturating_add(part.human_wait_ms);
            acc.context_tokens = acc.context_tokens.saturating_add(part.context_tokens);
            acc.pace_gaps_ms.extend_from_slice(&part.pace_gaps_ms);
            for (date, active_ms) in &part.daily_active_ms {
                match acc.daily_active_ms.iter_mut().find(|(d, _)| d == date) {
                    Some((_, total)) => *total = total.saturating_add(*active_ms),
                    None => acc.daily_active_ms.push((*date, *active_ms)),
                }
            }
        }
        if let Some(acc) = out.as_mut() {
            acc.pace_gaps_ms.sort_unstable();
            acc.daily_active_ms.sort_unstable_by_key(|(date, _)| *date);
        }
        out
    }
}

#[cfg(test)]
mod active_time_tests {
    use super::ActiveTimeSummary;

    /// 90K tokens over 90 seconds is 60K/min, not 90K/min; under a minute
    /// there is no rate; a saturated numerator still divides safely.
    #[test]
    fn context_per_minute_divides_in_milliseconds() {
        let rate = |context_tokens, active_ms| {
            ActiveTimeSummary {
                turns: 1,
                active_ms,
                context_tokens,
                window_days: 30,
                ..ActiveTimeSummary::default()
            }
            .context_per_minute()
        };
        assert_eq!(rate(90_000, 90_000), Some(60_000));
        assert_eq!(rate(90_000, 59_999), None);
        assert_eq!(rate(u64::MAX, 60_000), Some(u64::MAX));
    }

    /// Merging skips token-only providers so they can't inflate the rate.
    #[test]
    fn merged_skips_providers_without_turns() {
        let measured = ActiveTimeSummary {
            turns: 10,
            active_ms: 600_000,
            human_wait_ms: 60_000,
            context_tokens: 1_000_000,
            window_days: 30,
            pace_gaps_ms: vec![5_000, 45_000, 120_000],
            daily_active_ms: vec![(time::macros::date!(2026 - 09 - 01), 600_000)],
        };
        let token_only = ActiveTimeSummary {
            context_tokens: 9_000_000,
            window_days: 30,
            ..ActiveTimeSummary::default()
        };
        let merged = ActiveTimeSummary::merged([&measured, &token_only]).expect("measured part");
        assert_eq!(merged, measured);
        assert!(ActiveTimeSummary::merged([&token_only]).is_none());

        // Two providers on the same day add up; the peak is the summed day.
        let other = ActiveTimeSummary {
            turns: 1,
            active_ms: 60_000,
            window_days: 30,
            daily_active_ms: vec![
                (time::macros::date!(2026 - 09 - 01), 60_000),
                (time::macros::date!(2026 - 09 - 02), 500_000),
            ],
            ..ActiveTimeSummary::default()
        };
        let merged = ActiveTimeSummary::merged([&measured, &other]).expect("merged");
        assert_eq!(
            merged.daily_active_ms,
            vec![
                (time::macros::date!(2026 - 09 - 01), 660_000),
                (time::macros::date!(2026 - 09 - 02), 500_000),
            ]
        );
        assert_eq!(
            merged.peak_day(),
            Some((time::macros::date!(2026 - 09 - 01), 660_000))
        );
    }

    /// p50 / p90 use the same rank rule as the completion percentiles.
    #[test]
    fn pace_percentiles_follow_the_rank_rule() {
        let summary = ActiveTimeSummary {
            turns: 1,
            pace_gaps_ms: (1..=10).map(|n| n * 1_000).collect(),
            window_days: 30,
            ..ActiveTimeSummary::default()
        };
        assert_eq!(summary.pace_percentiles(), Some((5_000, 9_000)));
        assert_eq!(summary.pace_mean_ms(), Some(5_500));
        assert!(ActiveTimeSummary::default().pace_percentiles().is_none());
        assert!(ActiveTimeSummary::default().pace_mean_ms().is_none());
    }
}

#[derive(Debug, Clone, Default)]
pub struct Orchestration {
    /// Time-weighted mean of simultaneous sessions over active wall-time. Shown
    /// in the PARALLEL AGENTS panel (display-only — the codename ranks on token
    /// throughput alone).
    pub avg_concurrency: f64,
    /// Maximum number of sessions observed running simultaneously.
    pub peak_concurrency: usize,
    /// Active seconds spent at concurrency level 1, 2, 3, 4–6, 7–9, 10+
    /// (6 buckets). Drives the PARALLEL AGENTS distribution bar.
    pub time_by_level: [u64; 6],
}

/// CONTEXT panel data over the fixed 30-day window. The token totals (and so
/// the cached share) cover every dated usage event; the call-level rows —
/// bands, cold starts, expiries, ordinary uncached input — cover main-chain
/// calls of providers whose events are calls, and everything else lands in
/// `unclassified_effective`. `Some` whenever any event carried context;
/// `calls` may be 0 for aggregate-only providers. The share card quotes the
/// cached share only on a 30-day report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextSummary {
    pub calls: usize,
    /// Σ (input + cache writes + cache reads) over the window.
    pub context_tokens: u64,
    /// Σ cache reads.
    pub cached_tokens: u64,
    /// Σ input-equivalent tokens: input × 1, cache writes × their price
    /// multiplier, cache reads × theirs — the "what it actually cost" volume.
    pub effective_tokens: u64,
    /// Fixed context-size bands; a band with zero calls is not rendered.
    pub bands: Vec<ContextBand>,
    /// Low-reuse calls that resumed a session after the cache retention
    /// window. `None` when the provider has no session notion.
    pub expired: Option<ContextReason>,
    /// The first call of each session — the price of starting fresh.
    pub cold_start: Option<ContextReason>,
    /// Every other uncached input: the suffix a running session appends
    /// each call, plus calls with no chain to classify. Completes the
    /// partition so the rows account for the whole effective volume.
    pub uncached: ContextReason,
    /// Input-equivalent volume from events that are not calls (sidechain
    /// rows, Copilot / Grok aggregates): counted in the totals and the cached
    /// share, shown as its own row with no per-call figure, never mixed
    /// into a row that has a call denominator.
    pub unclassified_effective: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextBand {
    pub label: String,
    pub calls: usize,
    /// Input-equivalent tokens spent on cache reads by calls in this band.
    pub cached_effective: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextReason {
    pub calls: usize,
    /// Input-equivalent tokens of the uncached part of these calls — the
    /// same scale as the band rows, so per-call figures compare directly.
    pub effective: u64,
}

impl ContextSummary {
    /// Make the rows a partition of the displayed total: `effective_tokens`
    /// is derived from the rows, so shares add up by construction. If that
    /// sum overflows (a poisoned counter saturated a component) the rows
    /// cannot add up — the whole breakdown is dropped (no bands, no reason
    /// rows) and only the headline survives. Runs after provider analysis
    /// and again after `merged()`, which can overflow on its own.
    pub fn finalize(&mut self) {
        let total = self
            .bands
            .iter()
            .map(|band| band.cached_effective)
            .chain(self.expired.iter().map(|r| r.effective))
            .chain(self.cold_start.iter().map(|r| r.effective))
            .chain([self.uncached.effective, self.unclassified_effective])
            .try_fold(0_u64, u64::checked_add);
        if let Some(total) = total {
            self.effective_tokens = total;
        } else {
            self.effective_tokens = u64::MAX;
            self.bands.iter_mut().for_each(|band| {
                band.calls = 0;
                band.cached_effective = 0;
            });
            self.expired = None;
            self.cold_start = None;
            self.uncached = ContextReason::default();
            self.unclassified_effective = 0;
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "Display-only ratio of u64 token counts."
    )]
    pub fn cached_share(&self) -> f64 {
        if self.context_tokens == 0 {
            0.0
        } else {
            self.cached_tokens as f64 / self.context_tokens as f64
        }
    }

    /// Element-wise sum — the Total tab adds provider summaries because the
    /// expiry threshold is per provider and the bands are shared.
    pub fn merged<'a>(
        parts: impl IntoIterator<Item = &'a ContextSummary>,
    ) -> Option<ContextSummary> {
        let mut out: Option<ContextSummary> = None;
        for part in parts {
            let acc = out.get_or_insert_with(|| ContextSummary {
                bands: part
                    .bands
                    .iter()
                    .map(|band| ContextBand {
                        label: band.label.clone(),
                        ..ContextBand::default()
                    })
                    .collect(),
                ..ContextSummary::default()
            });
            acc.calls += part.calls;
            acc.context_tokens = acc.context_tokens.saturating_add(part.context_tokens);
            acc.cached_tokens = acc.cached_tokens.saturating_add(part.cached_tokens);
            acc.effective_tokens = acc.effective_tokens.saturating_add(part.effective_tokens);
            for (dst, src) in acc.bands.iter_mut().zip(&part.bands) {
                debug_assert_eq!(
                    dst.label, src.label,
                    "context bands are one shared constant"
                );
                dst.calls += src.calls;
                dst.cached_effective = dst.cached_effective.saturating_add(src.cached_effective);
            }
            acc.unclassified_effective = acc
                .unclassified_effective
                .saturating_add(part.unclassified_effective);
            acc.uncached.calls += part.uncached.calls;
            acc.uncached.effective = acc
                .uncached
                .effective
                .saturating_add(part.uncached.effective);
            for (dst, src) in [
                (&mut acc.expired, &part.expired),
                (&mut acc.cold_start, &part.cold_start),
            ] {
                if let Some(src) = src {
                    let dst = dst.get_or_insert_with(ContextReason::default);
                    dst.calls += src.calls;
                    dst.effective = dst.effective.saturating_add(src.effective);
                }
            }
        }
        // A part with totals but no calls (Copilot) still counts toward the
        // Total share; only an all-empty merge is None. The sum can overflow
        // where no part did, so the partition is re-validated.
        out.filter(|summary| summary.calls > 0 || summary.context_tokens > 0)
            .map(|mut summary| {
                summary.finalize();
                summary
            })
    }
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub provider: Provider,
    pub period_days: u16,
    pub period_start: Date,
    pub period_end: Date,
    pub root: PathBuf,
    pub scan_stats: ScanStats,
    pub total_usage: TokenUsage,
    /// Token volume over the most recent fixed codename window (last 30 days,
    /// inclusive of `period_end`), independent of the display `--days`. The
    /// codename level divides this by the window length so it never drifts with
    /// the chosen window.
    pub recent_window_volume: u64,
    /// Distinct active days within the same fixed 30-day window. Used as the
    /// codename's data-sufficiency floor so a short `--days` view can't demote a
    /// real user to the no-data rank.
    pub recent_window_active_days: usize,
    pub daily: Vec<DailyStat>,
    pub daily_sessions: Vec<DailySessions>,
    pub model_daily: Vec<ModelDailyStat>,
    pub models: Vec<ModelStat>,
    pub agents: Vec<AgentStat>,
    /// Fixed 30-day window (same as the codename window), NOT the display
    /// `--days` — attribution fields exist only in recent logs, so an
    /// all-time cut would silently under-count.
    pub skills: Vec<SkillStat>,
    pub limits: Option<LimitsHistory>,
    pub credits: Option<CreditsHistory>,
    pub modes: ModesSummary,
    pub tools: Vec<ToolStat>,
    pub projects: Vec<ProjectStat>,
    pub sessions: usize,
    pub active_days: usize,
    /// Token volume over the window immediately before this one (same length),
    /// for period-over-period deltas.
    pub previous_total_volume: u64,
    pub longest_streak_days: usize,
    pub current_streak_days: usize,
    pub most_active_day: Option<DailyStat>,
    pub hourly_usage: [u64; 24],
    pub busiest_hour: Option<(u8, u64)>,
    pub favorite_model: Option<String>,
    pub longest_session: Option<SessionSpan>,
    pub completion_duration: Option<DurationSummary>,
    /// User-initiated interruptions (Claude esc markers, Codex
    /// `turn_aborted`) dated inside the window. Independent of
    /// `completion_duration`: a window can hold interruptions and no
    /// completed turn.
    pub interrupted: usize,
    /// Cache reuse over the fixed 30-day window: event totals (the cached
    /// share) plus optional call-level rows; `None` when no event carried
    /// context. The Total tab holds the sum of the provider summaries.
    pub context: Option<ContextSummary>,
    /// Working time and context-per-minute over the fixed 30-day window;
    /// `None` when no turn completed there.
    pub active_time: Option<ActiveTimeSummary>,
    pub orchestration: Orchestration,
}

#[derive(Debug, Clone)]
pub struct AppSummary {
    pub generated_at: OffsetDateTime,
    pub period_days: u16,
    pub load_duration_ms: u64,
    pub combined: Summary,
    pub providers: Vec<Summary>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(calls: usize, effective: u64, expired: Option<u64>) -> ContextSummary {
        ContextSummary {
            calls,
            context_tokens: effective * 10,
            cached_tokens: effective * 9,
            effective_tokens: effective,
            bands: vec![
                ContextBand {
                    label: "<100K".into(),
                    calls,
                    cached_effective: effective / 2,
                },
                ContextBand {
                    label: "100-200K".into(),
                    calls: 0,
                    cached_effective: 0,
                },
            ],
            expired: expired.map(|effective| ContextReason {
                calls: 1,
                effective,
            }),
            cold_start: None,
            uncached: ContextReason {
                calls,
                effective: effective / 4,
            },
            unclassified_effective: effective / 10,
        }
    }

    /// The Total tab's summary is the element-wise sum: bands by position,
    /// reason rows present if any part has them, empty input → None.
    #[test]
    fn merged_context_adds_parts_element_wise() {
        let a = context(10, 1_000, Some(300));
        let b = context(5, 500, None);
        let total = ContextSummary::merged([&a, &b]).expect("merged");
        assert_eq!(total.calls, 15);
        // `merged()` re-derives the total from the rows: a = 500 + 300 + 250 +
        // 100, b = 250 + 0 + 125 + 50.
        assert_eq!(total.effective_tokens, 1_575);
        assert_eq!(total.cached_tokens, 13_500);
        assert_eq!(total.bands[0].calls, 15);
        assert_eq!(total.bands[0].cached_effective, 750);
        assert_eq!(total.bands[1].calls, 0);
        let expired = total
            .expired
            .as_ref()
            .expect("expired survives a None part");
        assert_eq!((expired.calls, expired.effective), (1, 300));
        assert!(total.cold_start.is_none());
        assert_eq!((total.uncached.calls, total.uncached.effective), (15, 375));
        assert_eq!(total.unclassified_effective, 150);
        assert!((total.cached_share() - 0.9).abs() < 1e-9);

        assert!(ContextSummary::merged([]).is_none());
        assert!(ContextSummary::merged([&context(0, 0, None)]).is_none());
        // Totals without calls (an aggregate-only provider) still merge.
        let totals_only = context(0, 400, None);
        assert!(ContextSummary::merged([&totals_only]).is_some());

        // A merge whose row sum overflows drops the whole breakdown: no band
        // calls, no reason rows, headline only.
        // (Built from a small fixture: the helper's `effective * 10` would
        // itself overflow on u64::MAX.)
        let mut huge = context(1, 1_000, Some(1));
        huge.bands[0].cached_effective = u64::MAX;
        huge.expired = Some(ContextReason {
            calls: 1,
            effective: u64::MAX,
        });
        let merged = ContextSummary::merged([&huge, &huge]).expect("merged");
        assert_eq!(merged.effective_tokens, u64::MAX);
        assert!(merged.bands.iter().all(|band| band.calls == 0));
        assert!(merged.expired.is_none() && merged.cold_start.is_none());
        assert_eq!(merged.unclassified_effective, 0);
    }
}
