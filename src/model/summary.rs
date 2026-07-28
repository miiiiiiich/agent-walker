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
/// think. Claude: thinking-block fire rate (+ fast mode when used);
/// Codex: reasoning-effort distribution.
#[derive(Debug, Clone, Default)]
pub struct ModesSummary {
    pub assistant_turns: usize,
    pub thinking_turns: usize,
    pub fast_turns: usize,
    /// (effort label, turns), sorted by turns descending.
    pub efforts: Vec<(String, usize)>,
}

impl ModesSummary {
    pub fn is_empty(&self) -> bool {
        self.assistant_turns == 0 && self.efforts.is_empty()
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
