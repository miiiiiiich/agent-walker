//! The per-file event bundle collectors produce and the cache serializes.
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime, UtcOffset};

use crate::model::{
    CreditSample, DurationEvent, EffortEvent, ModeEvent, PermissionEvent, RateLimitSample,
    SessionTouch, ToolEvent, UsageEvent,
};

/// Events extracted from a single log file. The unit of caching: parsed once,
/// reused as long as (mtime, size) of the source file are unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileEvents {
    pub usage_events: Vec<KeyedUsageEvent>,
    pub tool_events: Vec<KeyedToolEvent>,
    pub session_touches: Vec<SessionTouch>,
    pub duration_events: Vec<KeyedDurationEvent>,
    pub rate_limit_samples: Vec<KeyedRateLimitSample>,
    pub credit_samples: Vec<KeyedCreditSample>,
    pub effort_events: Vec<KeyedEffortEvent>,
    pub mode_events: Vec<KeyedModeEvent>,
    pub permission_events: Vec<KeyedPermissionEvent>,
    pub lines_seen: usize,
    pub parse_errors: usize,
}

/// Usage event with an optional cross-file deduplication key
/// (e.g. Claude message id appearing in both a session file and a fork).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedUsageEvent {
    pub key: Option<String>,
    pub event: UsageEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedToolEvent {
    pub key: Option<String>,
    pub event: ToolEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedRateLimitSample {
    pub key: Option<String>,
    pub event: RateLimitSample,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedCreditSample {
    pub key: Option<String>,
    pub event: CreditSample,
}

/// Duration event with an optional cross-file dedup key. Most collectors
/// leave it `None` (their durations never appear twice); Grok keys turn
/// durations by `prompt_id` because fork copies replay the parent's turns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedDurationEvent {
    pub key: Option<String>,
    pub event: DurationEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedEffortEvent {
    pub key: Option<String>,
    pub event: EffortEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedPermissionEvent {
    pub key: Option<String>,
    pub event: PermissionEvent,
}

/// Mode event keyed by message id. Duplicate lines for the same message can
/// disagree (a streaming fragment without the thinking block yet), so
/// duplicates merge with OR semantics instead of being dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedModeEvent {
    pub key: Option<String>,
    pub event: ModeEvent,
}

impl FileEvents {
    /// Compress raw session touches: per (session, local date) only the first
    /// and last touch matter for sessions / active-day / span aggregation.
    /// Keeps memory and cache size bounded for 100k-line session files.
    ///
    /// Bucketing uses the local-offset date to match the analyzer, which buckets
    /// concurrency / longest-session / daily-sessions by local day. The result
    /// therefore depends on `local_offset`; cached `FileEvents` embed this
    /// interpretation, and the cache records the offset it was built with
    /// (`CacheFile::offset_seconds`) so a machine-TZ change rebuilds
    /// automatically — no `--no-cache` needed.
    pub fn compress_touches(&mut self, local_offset: UtcOffset) {
        if self.session_touches.len() <= 2 {
            return;
        }
        let mut bounds: HashMap<(String, Date), (OffsetDateTime, OffsetDateTime)> = HashMap::new();
        for touch in self.session_touches.drain(..) {
            let key = (
                touch.session_id,
                touch.timestamp.to_offset(local_offset).date(),
            );
            bounds
                .entry(key)
                .and_modify(|(start, end)| {
                    *start = (*start).min(touch.timestamp);
                    *end = (*end).max(touch.timestamp);
                })
                .or_insert((touch.timestamp, touch.timestamp));
        }
        for ((session_id, _), (start, end)) in bounds {
            self.session_touches.push(SessionTouch {
                timestamp: start,
                session_id: session_id.clone(),
            });
            if end != start {
                self.session_touches.push(SessionTouch {
                    timestamp: end,
                    session_id,
                });
            }
        }
    }
}
