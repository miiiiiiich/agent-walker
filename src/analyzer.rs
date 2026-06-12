use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use time::{Date, Duration, OffsetDateTime, UtcOffset};

use crate::model::{
    AgentStat, Collection, DailySessions, DailyStat, DurationBucket, DurationSummary,
    ModelDailyStat, ModelStat, ProjectStat, SessionSpan, SourceKind, Summary, TokenUsage, ToolStat,
};

/// Summarize a collection over the trailing window. All timestamps are
/// normalized to `local_offset` before day/hour bucketing so that daily and
/// hourly stats follow the user's clock, not UTC.
#[allow(
    clippy::too_many_lines,
    reason = "Flat assembly of the Summary struct; splitting adds indirection without logic."
)]
pub fn summarize(
    collection: Collection,
    now: OffsetDateTime,
    period_days: u16,
    local_offset: UtcOffset,
) -> Summary {
    let safe_period_days = period_days.max(1);
    let period_end = now.to_offset(local_offset).date();
    let period_start = period_end - Duration::days(i64::from(safe_period_days) - 1);

    let aggregates = build_aggregates(
        &collection,
        period_start,
        period_end,
        safe_period_days,
        local_offset,
    );

    let daily = aggregates
        .daily_usage
        .into_iter()
        .map(|(date, usage)| DailyStat { date, usage })
        .collect::<Vec<_>>();
    let daily_sessions = aggregates
        .daily_session_ids
        .into_iter()
        .map(|(date, sessions)| DailySessions {
            date,
            sessions: sessions.len(),
        })
        .collect::<Vec<_>>();
    let most_active_day = daily
        .iter()
        .max_by_key(|day| day.usage.token_volume())
        .filter(|day| day.usage.token_volume() > 0)
        .cloned();
    let busiest_hour = aggregates
        .hourly_usage
        .iter()
        .enumerate()
        .max_by_key(|(_, usage)| **usage)
        .and_then(|(hour, usage)| {
            if *usage == 0 {
                None
            } else {
                u8::try_from(hour).ok().map(|hour| (hour, *usage))
            }
        });

    let mut models = aggregates
        .model_map
        .into_iter()
        .map(|(name, accumulator)| accumulator.into_stat(name))
        .collect::<Vec<_>>();
    models.sort_by(|left, right| {
        right
            .usage
            .token_volume()
            .cmp(&left.usage.token_volume())
            .then_with(|| left.name.cmp(&right.name))
    });
    let favorite_model = models.first().map(|stat| stat.name.clone());
    let model_daily = aggregates
        .model_daily_usage
        .into_iter()
        .map(|((date, model), usage)| ModelDailyStat { date, model, usage })
        .collect::<Vec<_>>();

    let mut agents = aggregates
        .agent_map
        .into_iter()
        .map(|(name, accumulator)| accumulator.into_stat(name))
        .collect::<Vec<_>>();
    agents.sort_by(|left, right| {
        right
            .usage
            .token_volume()
            .cmp(&left.usage.token_volume())
            .then_with(|| right.calls.cmp(&left.calls))
            .then_with(|| left.name.cmp(&right.name))
    });

    let mut tools = aggregates
        .tool_map
        .into_iter()
        .map(|(name, calls)| ToolStat { name, calls })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| {
        right
            .calls
            .cmp(&left.calls)
            .then_with(|| left.name.cmp(&right.name))
    });

    let mut projects = aggregates
        .project_map
        .into_iter()
        .map(|(name, accumulator)| accumulator.into_stat(name))
        .collect::<Vec<_>>();
    strip_common_project_prefix(&mut projects);
    projects.sort_by(|left, right| {
        right
            .usage
            .token_volume()
            .cmp(&left.usage.token_volume())
            .then_with(|| left.name.cmp(&right.name))
    });

    let longest_session = longest_session_span(&collection, period_start, period_end, local_offset);
    let completion_duration =
        completion_duration_summary(&collection, period_start, period_end, local_offset);
    let (longest_streak_days, current_streak_days) =
        streaks(&aggregates.active_dates, period_start, period_end);

    Summary {
        provider: collection.provider,
        generated_at: now,
        period_days: safe_period_days,
        period_start,
        period_end,
        root: collection.root,
        scan_stats: collection.stats,
        total_usage: aggregates.total_usage,
        daily,
        daily_sessions,
        model_daily,
        models,
        agents,
        tools,
        projects,
        sessions: aggregates.period_sessions.len(),
        active_days: aggregates.active_dates.len(),
        previous_total_volume: aggregates.previous_total_volume,
        previous_sessions: aggregates.previous_sessions.len(),
        longest_streak_days,
        current_streak_days,
        most_active_day,
        hourly_usage: aggregates.hourly_usage,
        busiest_hour,
        favorite_model,
        longest_session,
        completion_duration,
    }
}

fn build_aggregates(
    collection: &Collection,
    period_start: Date,
    period_end: Date,
    period_days: u16,
    local_offset: UtcOffset,
) -> Aggregates {
    let previous_start = period_start - Duration::days(i64::from(period_days));
    let mut aggregates = Aggregates {
        daily_usage: init_daily_usage(period_start, period_days),
        ..Aggregates::default()
    };
    for event in &collection.usage_events {
        aggregates.add_usage_event(
            event,
            period_start,
            period_end,
            previous_start,
            local_offset,
        );
    }
    for event in &collection.tool_events {
        aggregates.add_tool_event(event, period_start, period_end, local_offset);
    }
    for touch in &collection.session_touches {
        aggregates.add_session_touch(
            touch,
            period_start,
            period_end,
            previous_start,
            local_offset,
        );
    }
    aggregates
}

/// Drop dash-separated prefix segments shared by every project name
/// ("alice/work/api" / "alice/blog" -> "work/api" / "blog") so the
/// distinctive tail survives narrow columns.
fn strip_common_project_prefix(projects: &mut [ProjectStat]) {
    if projects.len() < 2 {
        return;
    }
    loop {
        let Some(first_segment) = projects[0].name.split('-').next().map(ToOwned::to_owned) else {
            return;
        };
        let prefix = format!("{first_segment}-");
        let all_share = projects
            .iter()
            .all(|project| project.name.starts_with(&prefix) && project.name.len() > prefix.len());
        if !all_share {
            return;
        }
        for project in projects.iter_mut() {
            project.name = project.name[prefix.len()..].to_owned();
        }
    }
}

fn init_daily_usage(period_start: Date, period_days: u16) -> BTreeMap<Date, TokenUsage> {
    (0..period_days)
        .map(|offset| {
            (
                period_start + Duration::days(i64::from(offset)),
                TokenUsage::default(),
            )
        })
        .collect()
}

fn longest_session_span(
    collection: &Collection,
    period_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
) -> Option<SessionSpan> {
    // Span per (session, local day): resumed sessions reuse their id across
    // days, so a raw per-session min/max would report multi-day "sessions".
    let mut bounds: HashMap<(&str, Date), (OffsetDateTime, OffsetDateTime)> = HashMap::new();
    for touch in &collection.session_touches {
        let date = touch.timestamp.to_offset(local_offset).date();
        if date < period_start || date > period_end {
            continue;
        }
        bounds
            .entry((touch.session_id.as_str(), date))
            .and_modify(|(start, end)| {
                *start = (*start).min(touch.timestamp);
                *end = (*end).max(touch.timestamp);
            })
            .or_insert((touch.timestamp, touch.timestamp));
    }

    bounds
        .into_iter()
        .map(|((session_id, _), (started_at, ended_at))| SessionSpan {
            session_id: session_id.to_owned(),
            started_at,
            ended_at,
        })
        .max_by_key(SessionSpan::duration_secs)
}

fn completion_duration_summary(
    collection: &Collection,
    period_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
) -> Option<DurationSummary> {
    let mut values = collection
        .duration_events
        .iter()
        .filter(|event| {
            event.timestamp.is_none_or(|timestamp| {
                let date = timestamp.to_offset(local_offset).date();
                date >= period_start && date <= period_end
            })
        })
        .map(|event| event.duration_ms)
        .filter(|duration_ms| *duration_ms > 0)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(DurationSummary {
        count: values.len(),
        p50_ms: percentile_ms(&values, 50),
        p90_ms: percentile_ms(&values, 90),
        p95_ms: percentile_ms(&values, 95),
        max_ms: *values.last().unwrap_or(&0),
        buckets: duration_buckets(&values),
    })
}

fn percentile_ms(sorted_values: &[u64], percentile: usize) -> u64 {
    let rank = sorted_values.len().saturating_mul(percentile).div_ceil(100);
    let index = rank.saturating_sub(1).min(sorted_values.len() - 1);
    sorted_values[index]
}

fn duration_buckets(sorted_values: &[u64]) -> Vec<DurationBucket> {
    const SECOND: u64 = 1_000;
    const MINUTE: u64 = 60 * SECOND;
    // Weighted toward the autonomy range: in 90 days of real data ~96% of
    // turns finish under 20m, so the short side gets three coarse buckets
    // and the 20m+ tail (the "can it run unattended" signal) gets four.
    const BUCKETS: [(&str, u64, u64); 7] = [
        ("<2m", 0, 2 * MINUTE),
        ("2-10m", 2 * MINUTE, 10 * MINUTE),
        ("10-20m", 10 * MINUTE, 20 * MINUTE),
        ("20-30m", 20 * MINUTE, 30 * MINUTE),
        ("30-45m", 30 * MINUTE, 45 * MINUTE),
        ("45m-1h", 45 * MINUTE, 60 * MINUTE),
        ("1h+", 60 * MINUTE, u64::MAX),
    ];

    BUCKETS
        .iter()
        .map(|(label, start, end)| DurationBucket {
            label: (*label).to_owned(),
            count: sorted_values
                .iter()
                .filter(|value| **value >= *start && **value < *end)
                .count(),
        })
        .collect()
}

fn streaks(active_dates: &BTreeSet<Date>, period_start: Date, period_end: Date) -> (usize, usize) {
    let mut longest = 0;
    let mut current_run = 0;
    let mut date = period_start;
    while date <= period_end {
        if active_dates.contains(&date) {
            current_run += 1;
            longest = longest.max(current_run);
        } else {
            current_run = 0;
        }
        date += Duration::days(1);
    }

    let mut current = 0;
    let mut cursor = period_end;
    while cursor >= period_start && active_dates.contains(&cursor) {
        current += 1;
        cursor -= Duration::days(1);
    }

    (longest, current)
}

#[derive(Default)]
struct Aggregates {
    total_usage: TokenUsage,
    daily_usage: BTreeMap<Date, TokenUsage>,
    model_daily_usage: BTreeMap<(Date, String), TokenUsage>,
    model_map: HashMap<String, ModelAccumulator>,
    agent_map: HashMap<String, AgentAccumulator>,
    tool_map: HashMap<String, usize>,
    project_map: HashMap<String, ProjectAccumulator>,
    period_sessions: HashSet<String>,
    daily_session_ids: BTreeMap<Date, HashSet<String>>,
    active_dates: BTreeSet<Date>,
    hourly_usage: [u64; 24],
    previous_total_volume: u64,
    previous_sessions: HashSet<String>,
}

impl Aggregates {
    fn add_usage_event(
        &mut self,
        event: &crate::model::UsageEvent,
        period_start: Date,
        period_end: Date,
        previous_start: Date,
        local_offset: UtcOffset,
    ) {
        let Some(timestamp) = event.timestamp else {
            return;
        };
        let timestamp = timestamp.to_offset(local_offset);
        let date = timestamp.date();
        if date >= previous_start && date < period_start {
            self.previous_total_volume = self
                .previous_total_volume
                .saturating_add(event.usage.token_volume());
            if let Some(session_id) = &event.session_id {
                self.previous_sessions.insert(session_id.clone());
            }
            return;
        }
        if date < period_start || date > period_end {
            return;
        }

        self.total_usage.add_assign(&event.usage);
        if event.usage.token_volume() > 0 {
            self.active_dates.insert(date);
        }
        if let Some(daily) = self.daily_usage.get_mut(&date) {
            daily.add_assign(&event.usage);
        }
        let hour = usize::from(timestamp.hour());
        self.hourly_usage[hour] =
            self.hourly_usage[hour].saturating_add(event.usage.token_volume());

        if let Some(session_id) = &event.session_id {
            self.period_sessions.insert(session_id.clone());
        }

        let model_name = event.model.clone().unwrap_or_else(|| "unknown".to_owned());
        self.model_map
            .entry(model_name.clone())
            .or_default()
            .add(&event.usage, date);
        self.model_daily_usage
            .entry((date, model_name))
            .or_default()
            .add_assign(&event.usage);

        if let Some(project) = &event.project {
            self.project_map
                .entry(project.clone())
                .or_default()
                .add(&event.usage);
        }

        if event.source_kind == SourceKind::Subagent || event.attribution_agent.is_some() {
            let agent_name = event
                .attribution_agent
                .clone()
                .unwrap_or_else(|| "subagent".to_owned());
            self.agent_map
                .entry(agent_name)
                .or_default()
                .add_usage(&event.usage, date);
        }
    }

    fn add_tool_event(
        &mut self,
        event: &crate::model::ToolEvent,
        period_start: Date,
        period_end: Date,
        local_offset: UtcOffset,
    ) {
        let Some(timestamp) = event.timestamp else {
            return;
        };
        let date = timestamp.to_offset(local_offset).date();
        if date < period_start || date > period_end {
            return;
        }
        *self.tool_map.entry(event.tool_name.clone()).or_default() += 1;
        if let Some(session_id) = &event.session_id {
            self.period_sessions.insert(session_id.clone());
        }
        if event.tool_name == "Agent"
            && let Some(subagent_type) = &event.subagent_type
        {
            self.agent_map
                .entry(subagent_type.clone())
                .or_default()
                .add_call(date);
        }
    }

    fn add_session_touch(
        &mut self,
        touch: &crate::model::SessionTouch,
        period_start: Date,
        period_end: Date,
        previous_start: Date,
        local_offset: UtcOffset,
    ) {
        let date = touch.timestamp.to_offset(local_offset).date();
        if date >= previous_start && date < period_start {
            self.previous_sessions.insert(touch.session_id.clone());
            return;
        }
        if date < period_start || date > period_end {
            return;
        }
        self.active_dates.insert(date);
        self.period_sessions.insert(touch.session_id.clone());
        self.daily_session_ids
            .entry(date)
            .or_default()
            .insert(touch.session_id.clone());
    }
}

#[derive(Default)]
struct ProjectAccumulator {
    usage: TokenUsage,
    events: usize,
}

impl ProjectAccumulator {
    fn add(&mut self, usage: &TokenUsage) {
        self.usage.add_assign(usage);
        self.events += 1;
    }

    fn into_stat(self, name: String) -> ProjectStat {
        ProjectStat {
            name,
            usage: self.usage,
            events: self.events,
        }
    }
}

#[derive(Default)]
struct ModelAccumulator {
    usage: TokenUsage,
    events: usize,
    active_days: BTreeSet<Date>,
}

impl ModelAccumulator {
    fn add(&mut self, usage: &TokenUsage, date: Date) {
        self.usage.add_assign(usage);
        self.events += 1;
        if usage.token_volume() > 0 {
            self.active_days.insert(date);
        }
    }

    fn into_stat(self, name: String) -> ModelStat {
        ModelStat {
            name,
            usage: self.usage,
            events: self.events,
            active_days: self.active_days.len(),
        }
    }
}

#[derive(Default)]
struct AgentAccumulator {
    usage: TokenUsage,
    calls: usize,
    active_days: BTreeSet<Date>,
}

impl AgentAccumulator {
    fn add_usage(&mut self, usage: &TokenUsage, date: Date) {
        self.usage.add_assign(usage);
        if usage.token_volume() > 0 {
            self.active_days.insert(date);
        }
    }

    fn add_call(&mut self, date: Date) {
        self.calls += 1;
        self.active_days.insert(date);
    }

    fn into_stat(self, name: String) -> AgentStat {
        AgentStat {
            name,
            usage: self.usage,
            calls: self.calls,
            active_days: self.active_days.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::model::{Provider, ScanStats, SessionTouch, UsageEvent};

    #[test]
    fn aggregates_models_agents_tools_and_streaks() {
        let now = datetime!(2026-06-08 12:00 UTC);
        let collection = Collection {
            provider: Provider::Claude,
            root: "/tmp/claude".into(),
            usage_events: vec![
                UsageEvent {
                    timestamp: Some(datetime!(2026-06-07 10:00 UTC)),
                    session_id: Some("s1".to_owned()),
                    model: Some("claude-opus-4-8".to_owned()),
                    source_kind: SourceKind::Main,
                    attribution_agent: None,
                    project: Some("orchestra".to_owned()),
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 10,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 80,
                        ..TokenUsage::default()
                    },
                },
                UsageEvent {
                    timestamp: Some(datetime!(2026-06-08 10:00 UTC)),
                    session_id: Some("s1".to_owned()),
                    model: Some("claude-haiku-4-5".to_owned()),
                    source_kind: SourceKind::Subagent,
                    attribution_agent: Some("Explore".to_owned()),
                    project: Some("orchestra".to_owned()),
                    usage: TokenUsage {
                        input_tokens: 5,
                        output_tokens: 5,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 40,
                        ..TokenUsage::default()
                    },
                },
            ],
            tool_events: vec![crate::model::ToolEvent {
                timestamp: Some(datetime!(2026-06-08 10:00 UTC)),
                session_id: Some("s1".to_owned()),
                tool_name: "Agent".to_owned(),
                subagent_type: Some("Explore".to_owned()),
                source_kind: SourceKind::Main,
            }],
            session_touches: vec![
                SessionTouch {
                    timestamp: datetime!(2026-06-07 10:00 UTC),
                    session_id: "s1".to_owned(),
                },
                SessionTouch {
                    timestamp: datetime!(2026-05-01 00:00 UTC),
                    session_id: "old".to_owned(),
                },
                SessionTouch {
                    timestamp: datetime!(2026-05-20 00:00 UTC),
                    session_id: "old".to_owned(),
                },
                SessionTouch {
                    timestamp: datetime!(2026-06-08 10:00 UTC),
                    session_id: "s1".to_owned(),
                },
                SessionTouch {
                    timestamp: datetime!(2026-06-08 14:00 UTC),
                    session_id: "s1".to_owned(),
                },
            ],
            duration_events: Vec::new(),
            stats: ScanStats::default(),
        };

        let summary = summarize(collection, now, 7, UtcOffset::UTC);

        assert_eq!(summary.total_usage.token_volume(), 150);
        assert_eq!(summary.models.len(), 2);
        assert_eq!(summary.agents[0].name, "Explore");
        assert_eq!(summary.agents[0].calls, 1);
        assert_eq!(summary.tools[0].name, "Agent");
        assert_eq!(summary.current_streak_days, 2);
        // Session spans are bounded to a single local day: s1 spans
        // 10:00-14:00 on Jun 8 even though it also touched Jun 7.
        assert_eq!(
            summary
                .longest_session
                .expect("session span should exist")
                .duration_secs(),
            14_400
        );
    }
}
