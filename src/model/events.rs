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
            combined.absorb(collection);
        }
        combined
    }

    /// No rows at all — a provider that was probed but had nothing to add.
    pub fn is_empty(&self) -> bool {
        self.usage_events.is_empty()
            && self.tool_events.is_empty()
            && self.session_touches.is_empty()
            && self.duration_events.is_empty()
            && self.rate_limit_samples.is_empty()
            && self.credit_samples.is_empty()
            && self.effort_events.is_empty()
            && self.mode_events.is_empty()
            && self.permission_events.is_empty()
            && self.interrupt_events.is_empty()
            && self.pace_events.is_empty()
    }

    /// Append another provider's events; the combined collection is a plain
    /// concatenation, so a provider that arrives late can be folded in.
    pub fn absorb(&mut self, collection: &Self) {
        self.usage_events
            .extend(collection.usage_events.iter().cloned());
        self.tool_events
            .extend(collection.tool_events.iter().cloned());
        self.session_touches
            .extend(collection.session_touches.iter().cloned());
        self.duration_events
            .extend(collection.duration_events.iter().cloned());
        self.rate_limit_samples
            .extend(collection.rate_limit_samples.iter().cloned());
        self.credit_samples
            .extend(collection.credit_samples.iter().cloned());
        self.effort_events
            .extend(collection.effort_events.iter().cloned());
        self.mode_events
            .extend(collection.mode_events.iter().cloned());
        self.permission_events
            .extend(collection.permission_events.iter().cloned());
        self.interrupt_events
            .extend(collection.interrupt_events.iter().cloned());
        self.pace_events
            .extend(collection.pace_events.iter().cloned());
        self.stats.add_assign(&collection.stats);
        self.stats.usage_events = self.usage_events.len();
        self.stats.tool_events = self.tool_events.len();
        self.stats.duration_events = self.duration_events.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive destructuring: adding a field to `Collection` without
    /// teaching `absorb` and `is_empty` about it fails to compile here.
    #[test]
    fn absorb_and_is_empty_cover_every_field() {
        let now = time::macros::datetime!(2026-09-17 12:00 UTC);
        let a = crate::demo::copilot_collection_for_tests(now, 30);
        assert!(!a.is_empty());
        assert!(Collection::new(Provider::Combined, PathBuf::new()).is_empty());

        let mut c = Collection::combined(PathBuf::new(), std::slice::from_ref(&a));
        c.absorb(&a);
        let Collection {
            provider: _,
            root: _,
            stats,
            usage_events,
            tool_events,
            session_touches,
            duration_events,
            rate_limit_samples,
            credit_samples,
            effort_events,
            mode_events,
            permission_events,
            interrupt_events,
            pace_events,
        } = &c;
        assert_eq!(usage_events.len(), a.usage_events.len() * 2);
        assert_eq!(tool_events.len(), a.tool_events.len() * 2);
        assert_eq!(session_touches.len(), a.session_touches.len() * 2);
        assert_eq!(duration_events.len(), a.duration_events.len() * 2);
        assert_eq!(rate_limit_samples.len(), a.rate_limit_samples.len() * 2);
        assert_eq!(credit_samples.len(), a.credit_samples.len() * 2);
        assert_eq!(effort_events.len(), a.effort_events.len() * 2);
        assert_eq!(mode_events.len(), a.mode_events.len() * 2);
        assert_eq!(permission_events.len(), a.permission_events.len() * 2);
        assert_eq!(interrupt_events.len(), a.interrupt_events.len() * 2);
        assert_eq!(pace_events.len(), a.pace_events.len() * 2);
        assert_eq!(stats.files_seen, a.stats.files_seen * 2);
        assert_eq!(stats.usage_events, usage_events.len());
    }
}
