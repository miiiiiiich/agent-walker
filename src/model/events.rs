use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::{Provider, SourceKind, TokenUsage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEvent {
    pub timestamp: Option<OffsetDateTime>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub source_kind: SourceKind,
    pub attribution_agent: Option<String>,
    pub attribution_skill: Option<String>,
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
    pub human_wait_ms: u64,
    pub model_ms: Option<u64>,
    pub status: Option<String>,
}

impl DurationEvent {
    pub fn active_ms(&self) -> u64 {
        self.duration_ms.saturating_sub(self.human_wait_ms)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitSample {
    pub timestamp: OffsetDateTime,
    pub used_percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditSample {
    pub timestamp: OffsetDateTime,
    pub nano_aiu: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffortEvent {
    pub timestamp: Option<OffsetDateTime>,
    pub effort: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionEvent {
    pub timestamp: Option<OffsetDateTime>,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptEvent {
    pub timestamp: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaceEvent {
    pub timestamp: Option<OffsetDateTime>,
    pub gap_ms: u64,
}

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
    pub credit_samples: Vec<CreditSample>,
    pub effort_events: Vec<EffortEvent>,
    pub mode_events: Vec<ModeEvent>,
    pub permission_events: Vec<PermissionEvent>,
    pub interrupt_events: Vec<InterruptEvent>,
    pub pace_events: Vec<PaceEvent>,
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
            credit_samples: Vec::new(),
            effort_events: Vec::new(),
            mode_events: Vec::new(),
            permission_events: Vec::new(),
            interrupt_events: Vec::new(),
            pace_events: Vec::new(),
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
                .credit_samples
                .extend(collection.credit_samples.iter().cloned());
            combined
                .effort_events
                .extend(collection.effort_events.iter().cloned());
            combined
                .mode_events
                .extend(collection.mode_events.iter().cloned());
            combined
                .permission_events
                .extend(collection.permission_events.iter().cloned());
            combined
                .interrupt_events
                .extend(collection.interrupt_events.iter().cloned());
            combined
                .pace_events
                .extend(collection.pace_events.iter().cloned());
            combined.stats.add_assign(&collection.stats);
        }
        combined.stats.usage_events = combined.usage_events.len();
        combined.stats.tool_events = combined.tool_events.len();
        combined.stats.duration_events = combined.duration_events.len();
        combined
    }
}
