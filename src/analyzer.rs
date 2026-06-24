mod aggregates;
mod concurrency;
mod duration;
mod projects;
mod streak;

use std::collections::BTreeMap;

use time::{Date, Duration, OffsetDateTime, UtcOffset};

use crate::model::{
    Collection, DailySessions, DailyStat, ModelDailyStat, Summary, TokenUsage, ToolStat,
};

use self::aggregates::Aggregates;

/// Summarize a collection over the trailing window. All timestamps are
/// normalized to `local_offset` before day/hour bucketing so that daily and
/// hourly stats follow the user's clock, not UTC.
#[allow(
    clippy::too_many_lines,
    reason = "Flat assembly of the Summary struct; splitting adds indirection without logic."
)]
pub fn summarize(
    collection: &Collection,
    now: OffsetDateTime,
    period_days: u16,
    local_offset: UtcOffset,
) -> Summary {
    let safe_period_days = period_days.max(1);
    let period_end = now.to_offset(local_offset).date();
    let period_start = period_end - Duration::days(i64::from(safe_period_days) - 1);

    let aggregates = build_aggregates(
        collection,
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
    let model_daily_reported = aggregates.model_daily_reported;
    let model_daily = aggregates
        .model_daily_usage
        .into_iter()
        .map(|((date, model), usage)| {
            let reported_cost_usd = model_daily_reported.get(&(date, model.clone())).copied();
            ModelDailyStat {
                date,
                model,
                usage,
                reported_cost_usd,
            }
        })
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
    projects::strip_common_project_prefix(&mut projects);
    projects.sort_by(|left, right| {
        right
            .usage
            .token_volume()
            .cmp(&left.usage.token_volume())
            .then_with(|| left.name.cmp(&right.name))
    });

    let longest_session =
        duration::longest_session_span(collection, period_start, period_end, local_offset);
    let completion_duration =
        duration::completion_duration_summary(collection, period_start, period_end, local_offset);
    let orchestration =
        concurrency::orchestration(collection, period_start, period_end, local_offset);
    let (longest_streak_days, current_streak_days) =
        streak::streaks(&aggregates.active_dates, period_start, period_end);

    // Codename throughput is a fixed-window rate independent of the display
    // `--days`: sum token volume over the most recent `CODENAME_WINDOW_DAYS`
    // straight from the events (the collector loads at least that span), so a
    // 7- or 90-day view yields the same level.
    let codename_window_start =
        period_end - Duration::days(crate::codename::CODENAME_WINDOW_DAYS - 1);
    let mut recent_window_volume = 0_u64;
    let mut recent_active_days = std::collections::HashSet::new();
    for event in &collection.usage_events {
        let Some(date) = event.timestamp.map(|ts| ts.to_offset(local_offset).date()) else {
            continue;
        };
        if date < codename_window_start || date > period_end {
            continue;
        }
        let volume = event.usage.token_volume();
        if volume == 0 {
            continue;
        }
        recent_window_volume = recent_window_volume.saturating_add(volume);
        recent_active_days.insert(date);
    }
    let recent_window_active_days = recent_active_days.len();

    Summary {
        provider: collection.provider,
        period_days: safe_period_days,
        period_start,
        period_end,
        root: collection.root.clone(),
        scan_stats: collection.stats.clone(),
        total_usage: aggregates.total_usage,
        recent_window_volume,
        recent_window_active_days,
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
        longest_streak_days,
        current_streak_days,
        most_active_day,
        hourly_usage: aggregates.hourly_usage,
        busiest_hour,
        favorite_model,
        longest_session,
        completion_duration,
        orchestration,
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

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::model::{
        Collection, Provider, ScanStats, SessionTouch, SourceKind, TokenUsage, UsageEvent,
    };

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
                    reported_cost_usd: None,
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
                    reported_cost_usd: None,
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

        let summary = summarize(&collection, now, 7, UtcOffset::UTC);

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

    #[test]
    fn recent_window_volume_is_independent_of_display_days() {
        // Four usage events at 0, 5, 20, and 40 days before period_end.
        let now = datetime!(2026-06-30 12:00 UTC);
        let event = |at: OffsetDateTime, tokens: u64| UsageEvent {
            timestamp: Some(at),
            session_id: Some("s".to_owned()),
            model: Some("claude-opus-4-8".to_owned()),
            source_kind: SourceKind::Main,
            attribution_agent: None,
            project: None,
            usage: TokenUsage {
                input_tokens: tokens,
                ..TokenUsage::default()
            },
            reported_cost_usd: None,
        };
        let events = vec![
            event(datetime!(2026-06-30 10:00 UTC), 1_000_000), // 30d + every window
            event(datetime!(2026-06-25 10:00 UTC), 2_000_000), // 30d + 7d window
            event(datetime!(2026-06-10 10:00 UTC), 3_000_000), // 30d, not 7d window
            event(datetime!(2026-05-21 10:00 UTC), 4_000_000), // only the 90d display
        ];
        let collection = |events: Vec<UsageEvent>| Collection {
            provider: Provider::Claude,
            root: "/tmp".into(),
            usage_events: events,
            tool_events: Vec::new(),
            session_touches: Vec::new(),
            duration_events: Vec::new(),
            stats: ScanStats::default(),
        };

        let week = summarize(&collection(events.clone()), now, 7, UtcOffset::UTC);
        let quarter = summarize(&collection(events), now, 90, UtcOffset::UTC);

        // The codename window is fixed at the last 30 days (06-01..06-30 = 6M),
        // regardless of how many days the display covers.
        assert_eq!(week.recent_window_volume, 6_000_000);
        assert_eq!(quarter.recent_window_volume, 6_000_000);
        // The display totals, by contrast, DO follow --days (3M vs 10M).
        assert_eq!(week.total_usage.token_volume(), 3_000_000);
        assert_eq!(quarter.total_usage.token_volume(), 10_000_000);
    }
}
