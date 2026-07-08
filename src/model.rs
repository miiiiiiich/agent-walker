use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Combined,
    Claude,
    Codex,
    Agy,
    OpenCode,
    Cursor,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Combined => "Total",
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Agy => "Agy",
            Self::OpenCode => "OpenCode",
            Self::Cursor => "Cursor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceKind {
    Main,
    Subagent,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_ephemeral_1h_input_tokens: u64,
    pub cache_creation_ephemeral_5m_input_tokens: u64,
    pub server_tool_use: BTreeMap<String, u64>,
}

impl TokenUsage {
    // All arithmetic saturates: token counts come from untrusted log files,
    // and a poisoned value must degrade to a pinned number, never wrap or
    // panic the whole dashboard.
    pub fn token_volume(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }

    pub fn add_assign(&mut self, other: &Self) {
        let add = |target: &mut u64, value: u64| *target = target.saturating_add(value);
        add(&mut self.input_tokens, other.input_tokens);
        add(&mut self.output_tokens, other.output_tokens);
        add(
            &mut self.reasoning_output_tokens,
            other.reasoning_output_tokens,
        );
        add(
            &mut self.cache_creation_input_tokens,
            other.cache_creation_input_tokens,
        );
        add(
            &mut self.cache_read_input_tokens,
            other.cache_read_input_tokens,
        );
        add(
            &mut self.cache_creation_ephemeral_1h_input_tokens,
            other.cache_creation_ephemeral_1h_input_tokens,
        );
        add(
            &mut self.cache_creation_ephemeral_5m_input_tokens,
            other.cache_creation_ephemeral_5m_input_tokens,
        );
        for (key, value) in &other.server_tool_use {
            let entry = self.server_tool_use.entry(key.clone()).or_default();
            *entry = entry.saturating_add(*value);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEvent {
    pub timestamp: Option<OffsetDateTime>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub source_kind: SourceKind,
    pub attribution_agent: Option<String>,
    /// Skill active when this message was produced (Claude `attributionSkill`).
    /// Feeds the SKILLS section only — never the share card.
    pub attribution_skill: Option<String>,
    /// Repository / working-directory label derived from the log location
    /// (Claude: project directory name; Codex: `session_meta` cwd).
    pub project: Option<String>,
    pub usage: TokenUsage,
    /// Provider-reported cost in USD for this event, when the source gives an
    /// authoritative figure that the `LiteLLM` model→price path can't (Cursor's
    /// own models aren't in the pricing table). `None` means "price it from
    /// `LiteLLM` like every other provider".
    pub reported_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolEvent {
    pub timestamp: Option<OffsetDateTime>,
    pub session_id: Option<String>,
    pub tool_name: String,
    pub subagent_type: Option<String>,
    pub source_kind: SourceKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTouch {
    pub timestamp: OffsetDateTime,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurationEvent {
    pub timestamp: Option<OffsetDateTime>,
    pub session_id: Option<String>,
    pub duration_ms: u64,
    pub status: Option<String>,
}

/// One `rate_limits` snapshot from a Codex rollout: the plan's primary
/// (5-hour) window utilization at that moment. History-only material — the
/// dashboard shows past utilization, never a "current" meter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitSample {
    pub timestamp: OffsetDateTime,
    pub used_percent: f64,
}

/// One Codex turn's reasoning-effort setting (`turn_context.payload.effort`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffortEvent {
    pub timestamp: Option<OffsetDateTime>,
    pub effort: String,
}

/// Per-assistant-message mode flags for Claude: whether extended thinking
/// fired (a `thinking` content block exists — block presence only, text is
/// never read) and whether fast mode served it (`usage.speed == "fast"`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeEvent {
    pub timestamp: Option<OffsetDateTime>,
    pub has_thinking: bool,
    pub fast: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanStats {
    pub files_seen: usize,
    pub lines_seen: usize,
    pub usage_events: usize,
    pub tool_events: usize,
    pub duration_events: usize,
    pub parse_errors: usize,
    pub unreadable_files: usize,
    pub unreadable_dirs: usize,
}

impl ScanStats {
    pub fn add_assign(&mut self, other: &Self) {
        self.files_seen += other.files_seen;
        self.lines_seen += other.lines_seen;
        self.usage_events += other.usage_events;
        self.tool_events += other.tool_events;
        self.duration_events += other.duration_events;
        self.parse_errors += other.parse_errors;
        self.unreadable_files += other.unreadable_files;
        self.unreadable_dirs += other.unreadable_dirs;
    }
}

#[derive(Debug, Clone)]
pub struct Collection {
    pub provider: Provider,
    pub root: PathBuf,
    pub usage_events: Vec<UsageEvent>,
    pub tool_events: Vec<ToolEvent>,
    pub session_touches: Vec<SessionTouch>,
    pub duration_events: Vec<DurationEvent>,
    pub rate_limit_samples: Vec<RateLimitSample>,
    pub effort_events: Vec<EffortEvent>,
    pub mode_events: Vec<ModeEvent>,
    pub stats: ScanStats,
}

impl Collection {
    pub fn new(provider: Provider, root: PathBuf) -> Self {
        Self {
            provider,
            root,
            usage_events: Vec::new(),
            tool_events: Vec::new(),
            session_touches: Vec::new(),
            duration_events: Vec::new(),
            rate_limit_samples: Vec::new(),
            effort_events: Vec::new(),
            mode_events: Vec::new(),
            stats: ScanStats::default(),
        }
    }

    pub fn combined(root: PathBuf, collections: &[Self]) -> Self {
        let mut combined = Self::new(Provider::Combined, root);
        for collection in collections {
            combined
                .usage_events
                .extend(collection.usage_events.iter().cloned());
            combined
                .tool_events
                .extend(collection.tool_events.iter().cloned());
            combined
                .session_touches
                .extend(collection.session_touches.iter().cloned());
            combined
                .duration_events
                .extend(collection.duration_events.iter().cloned());
            combined
                .rate_limit_samples
                .extend(collection.rate_limit_samples.iter().cloned());
            combined
                .effort_events
                .extend(collection.effort_events.iter().cloned());
            combined
                .mode_events
                .extend(collection.mode_events.iter().cloned());
            combined.stats.add_assign(&collection.stats);
        }
        combined.stats.usage_events = combined.usage_events.len();
        combined.stats.tool_events = combined.tool_events.len();
        combined.stats.duration_events = combined.duration_events.len();
        combined
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_arithmetic_saturates_instead_of_wrapping() {
        let poisoned = TokenUsage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            ..TokenUsage::default()
        };
        assert_eq!(poisoned.token_volume(), u64::MAX);

        let mut total = TokenUsage {
            input_tokens: u64::MAX - 1,
            ..TokenUsage::default()
        };
        total.add_assign(&poisoned);
        assert_eq!(total.input_tokens, u64::MAX);
    }
}
