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
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Combined => "Combined",
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::Agy => "Agy",
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

    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens
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
    /// Repository / working-directory label derived from the log location
    /// (Claude: project directory name; Codex: `session_meta` cwd).
    pub project: Option<String>,
    pub usage: TokenUsage,
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
}

#[derive(Debug, Clone)]
pub struct ModelStat {
    pub name: String,
    pub usage: TokenUsage,
    pub events: usize,
    pub active_days: usize,
}

#[derive(Debug, Clone)]
pub struct AgentStat {
    pub name: String,
    pub usage: TokenUsage,
    pub calls: usize,
    pub active_days: usize,
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
    pub events: usize,
}

#[derive(Debug, Clone)]
pub struct SessionSpan {
    pub session_id: String,
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

#[derive(Debug, Clone)]
pub struct Summary {
    pub provider: Provider,
    pub generated_at: OffsetDateTime,
    pub period_days: u16,
    pub period_start: Date,
    pub period_end: Date,
    pub root: PathBuf,
    pub scan_stats: ScanStats,
    pub total_usage: TokenUsage,
    pub daily: Vec<DailyStat>,
    pub daily_sessions: Vec<DailySessions>,
    pub model_daily: Vec<ModelDailyStat>,
    pub models: Vec<ModelStat>,
    pub agents: Vec<AgentStat>,
    pub tools: Vec<ToolStat>,
    pub projects: Vec<ProjectStat>,
    pub sessions: usize,
    pub active_days: usize,
    /// Token volume and session count over the window immediately before this
    /// one (same length), for period-over-period deltas.
    pub previous_total_volume: u64,
    pub previous_sessions: usize,
    pub longest_streak_days: usize,
    pub current_streak_days: usize,
    pub most_active_day: Option<DailyStat>,
    pub hourly_usage: [u64; 24],
    pub busiest_hour: Option<(u8, u64)>,
    pub favorite_model: Option<String>,
    pub longest_session: Option<SessionSpan>,
    pub completion_duration: Option<DurationSummary>,
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
