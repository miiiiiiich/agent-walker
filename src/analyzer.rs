mod active_time;
mod aggregates;
mod concurrency;
mod context;
mod duration;
mod projects;
mod streak;

mod credits;
mod limits;
mod modes;

use std::collections::BTreeMap;

use time::{Date, Duration, OffsetDateTime, UtcOffset};

use crate::model::{
    Collection, DailySessions, DailyStat, ModelDailyStat, SkillStat, Summary, TokenUsage, ToolStat,
};
use crate::timestamp::{local_date, to_local};

use self::aggregates::Aggregates;
pub(crate) use self::concurrency::session_day_bounds;

/// Normalize timestamps to the local offset so day/hour buckets follow the user's clock.
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
    let period_end = to_local(now, local_offset).unwrap_or(now).date();
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
    let mut model_daily_estimated = aggregates.model_daily_estimated;
    let model_daily = aggregates
        .model_daily_usage
        .into_iter()
        .map(|((date, model), usage)| {
            let estimated = model_daily_estimated
                .remove(&(date, model.clone()))
                .unwrap_or_default();
            ModelDailyStat {
                date,
                model,
                usage,
                estimated,
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
    let interrupted =
        duration::interrupted_count(collection, period_start, period_end, local_offset);
    let orchestration =
        concurrency::orchestration(collection, period_start, period_end, local_offset);
    let (longest_streak_days, current_streak_days) =
        streak::streaks(&aggregates.active_dates, period_start, period_end);

    // Use the requested window for codename volume so its rate stays comparable across spans.
    let mut recent_window_volume = 0_u64;
    let mut recent_active_days = std::collections::HashSet::new();
    let mut skill_map: BTreeMap<String, TokenUsage> = BTreeMap::new();
    for event in &collection.usage_events {
        let Some(date) = event.timestamp.and_then(|ts| local_date(ts, local_offset)) else {
            continue;
        };
        if date < period_start || date > period_end {
            continue;
        }
        let volume = event.usage.token_volume();
        if volume == 0 {
            continue;
        }
        recent_window_volume = recent_window_volume.saturating_add(volume);
        recent_active_days.insert(date);
        if let Some(skill) = &event.attribution_skill {
            skill_map
                .entry(skill.clone())
                .or_default()
                .add_assign(&event.usage);
        }
    }
    let mut skills = skill_map
        .into_iter()
        .map(|(name, usage)| SkillStat { name, usage })
        .collect::<Vec<_>>();
    skills.sort_by(|left, right| {
        right
            .usage
            .token_volume()
            .cmp(&left.usage.token_volume())
            .then_with(|| left.name.cmp(&right.name))
    });
    let limits = limits::limits_history(
        collection,
        period_start,
        period_end,
        local_offset,
        &recent_active_days,
    );
    let credits = credits::credits_history(collection, period_start, period_end, local_offset);
    let mode_usage = modes::modes_summary(collection, period_start, period_end, local_offset);
    let context = context::context_summary(collection, period_start, period_end, local_offset);
    let active_time = active_time::active_time_summary(
        collection,
        period_start,
        period_end,
        safe_period_days,
        local_offset,
    );
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
        skills,
        limits,
        credits,
        modes: mode_usage,
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
        interrupted,
        context,
        active_time,
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
        Collection, InterruptEvent, Provider, SessionTouch, SourceKind, TokenUsage, UsageEvent,
    };

    /// Collectors keep only plausible timestamps, but a parse cache written by
    /// another build can still hand over one that overflows in the local
    /// offset: every section skips it instead of panicking.
    #[test]
    fn timestamps_that_overflow_locally_are_skipped() {
        let edge = datetime!(9999-12-31 23:00 UTC);
        let now = datetime!(2026-06-08 12:00 UTC);
        let collection = Collection {
            usage_events: vec![UsageEvent {
                timestamp: Some(edge),
                session_id: Some("s1".to_owned()),
                model: Some("claude-opus-4-8".to_owned()),
                source_kind: SourceKind::Main,
                attribution_agent: None,
                attribution_skill: None,
                project: None,
                usage: TokenUsage {
                    input_tokens: 10,
                    ..TokenUsage::default()
                },
                granularity: crate::model::UsageGranularity::Call,
            }],
            tool_events: vec![crate::model::ToolEvent {
                timestamp: Some(edge),
                session_id: Some("s1".to_owned()),
                tool_name: "Read".to_owned(),
                subagent_type: None,
                source_kind: SourceKind::Main,
            }],
            session_touches: vec![SessionTouch {
                timestamp: edge,
                session_id: "s1".to_owned(),
            }],
            duration_events: vec![crate::model::DurationEvent {
                timestamp: Some(edge),
                session_id: Some("s1".to_owned()),
                duration_ms: 1_000,
                human_wait_ms: 0,
                model_ms: None,
                status: None,
            }],
            rate_limit_samples: vec![crate::model::RateLimitSample {
                timestamp: edge,
                used_percent: 50.0,
            }],
            credit_samples: vec![crate::model::CreditSample {
                timestamp: edge,
                nano_aiu: 1,
            }],
            interrupt_events: vec![InterruptEvent {
                timestamp: Some(edge),
            }],
            pace_events: vec![crate::model::PaceEvent {
                timestamp: Some(edge),
                gap_ms: 1,
            }],
            mode_events: vec![crate::model::ModeEvent {
                timestamp: Some(edge),
                has_thinking: true,
                fast: false,
            }],
            ..Collection::new(Provider::Codex, "/tmp".into())
        };

        let tokyo = UtcOffset::from_hms(9, 0, 0).expect("offset");
        let summary = summarize(&collection, now, 7, tokyo);

        assert_eq!(summary.total_usage.token_volume(), 0);
        assert_eq!(summary.sessions, 0);
        assert_eq!(summary.interrupted, 0);
    }

    #[test]
    fn interruptions_are_counted_independently_of_durations() {
        let now = datetime!(2026-06-08 12:00 UTC);
        let collection = Collection {
            interrupt_events: vec![
                InterruptEvent {
                    timestamp: Some(datetime!(2026-06-07 10:00 UTC)),
                },
                InterruptEvent {
                    timestamp: Some(datetime!(2026-01-01 10:00 UTC)),
                },
                InterruptEvent { timestamp: None },
            ],
            ..Collection::new(Provider::Claude, "/tmp".into())
        };

        let summary = summarize(&collection, now, 7, UtcOffset::UTC);

        assert!(summary.completion_duration.is_none());
        assert_eq!(summary.interrupted, 1);
    }

    #[test]
    fn aggregates_models_agents_tools_and_streaks() {
        let now = datetime!(2026-06-08 12:00 UTC);
        let collection = Collection {
            usage_events: vec![
                UsageEvent {
                    timestamp: Some(datetime!(2026-06-07 10:00 UTC)),
                    session_id: Some("s1".to_owned()),
                    model: Some("claude-opus-4-8".to_owned()),
                    source_kind: SourceKind::Main,
                    attribution_agent: None,
                    attribution_skill: None,
                    project: Some("orchestra".to_owned()),
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 10,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 80,
                        ..TokenUsage::default()
                    },
                    granularity: crate::model::UsageGranularity::Call,
                },
                UsageEvent {
                    timestamp: Some(datetime!(2026-06-08 10:00 UTC)),
                    session_id: Some("s1".to_owned()),
                    model: Some("claude-haiku-4-5".to_owned()),
                    source_kind: SourceKind::Subagent,
                    attribution_agent: Some("Explore".to_owned()),
                    attribution_skill: None,
                    project: Some("orchestra".to_owned()),
                    usage: TokenUsage {
                        input_tokens: 5,
                        output_tokens: 5,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 40,
                        ..TokenUsage::default()
                    },
                    granularity: crate::model::UsageGranularity::Call,
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
            ..Collection::new(Provider::Claude, "/tmp/claude".into())
        };

        let summary = summarize(&collection, now, 7, UtcOffset::UTC);

        assert_eq!(summary.total_usage.token_volume(), 150);
        assert_eq!(summary.models.len(), 2);
        assert_eq!(summary.agents[0].name, "Explore");
        assert_eq!(summary.agents[0].calls, 1);
        assert_eq!(summary.tools[0].name, "Agent");
        assert_eq!(summary.current_streak_days, 2);
        assert_eq!(
            summary
                .longest_session
                .expect("session span should exist")
                .duration_secs(),
            14_400
        );
    }

    #[test]
    fn every_section_follows_the_requested_window() {
        let now = datetime!(2026-06-30 12:00 UTC);
        let event = |at: OffsetDateTime, tokens: u64| UsageEvent {
            timestamp: Some(at),
            session_id: Some("s".to_owned()),
            model: Some("claude-opus-4-8".to_owned()),
            source_kind: SourceKind::Main,
            attribution_agent: None,
            attribution_skill: None,
            project: None,
            usage: TokenUsage {
                input_tokens: tokens,
                ..TokenUsage::default()
            },
            granularity: crate::model::UsageGranularity::Call,
        };
        let events = vec![
            event(datetime!(2026-06-30 10:00 UTC), 1_000_000_000), // every window
            event(datetime!(2026-06-29 10:00 UTC), 1_000_000_000), // every window
            event(datetime!(2026-06-25 10:00 UTC), 1_000_000_000), // 7d and wider
            event(datetime!(2026-05-21 10:00 UTC), 9_000_000_000), // only the 90d span
        ];
        let collection = |events: Vec<UsageEvent>| Collection {
            usage_events: events,
            ..Collection::new(Provider::Claude, "/tmp".into())
        };

        let week = summarize(&collection(events.clone()), now, 7, UtcOffset::UTC);
        let quarter = summarize(&collection(events), now, 90, UtcOffset::UTC);

        assert_eq!(week.recent_window_volume, 3_000_000_000);
        assert_eq!(quarter.recent_window_volume, 12_000_000_000);
        assert_eq!(week.total_usage.token_volume(), 3_000_000_000);
        assert_eq!(quarter.total_usage.token_volume(), 12_000_000_000);
        assert_eq!(week.period_days, 7);
        assert_eq!(quarter.period_days, 90);
        assert_eq!(week.recent_window_active_days, 3);
        assert_eq!(quarter.recent_window_active_days, 4);
        assert_eq!(
            crate::codename::for_summary(&week).rank,
            crate::codename::Rank::S
        );
        assert_eq!(
            crate::codename::for_summary(&quarter).rank,
            crate::codename::Rank::B
        );
    }

    fn haiku_call(cache_read: u64) -> UsageEvent {
        UsageEvent {
            timestamp: Some(datetime!(2026-06-07 10:00 UTC)),
            session_id: None,
            model: Some("claude-haiku-5-5".to_owned()),
            source_kind: SourceKind::Main,
            attribution_agent: None,
            attribution_skill: None,
            project: None,
            usage: TokenUsage {
                input_tokens: 1_000,
                cache_read_input_tokens: cache_read,
                ..TokenUsage::default()
            },
            granularity: crate::model::UsageGranularity::Call,
        }
    }

    /// Two calls under the long-context threshold stay at base rates even
    /// though their sum is over it, and an aggregate row never crosses it,
    /// whichever provider it comes from — the Total tab included.
    #[test]
    fn long_context_rates_are_judged_per_call() {
        crate::cost::tests::install_test_pricing();
        let now = datetime!(2026-06-08 12:00 UTC);
        let claude = Collection {
            usage_events: vec![haiku_call(59_000), haiku_call(59_000), haiku_call(119_000)],
            ..Collection::new(Provider::Claude, "/tmp".into())
        };
        let summary = summarize(&claude, now, 7, UtcOffset::UTC);
        let short = (1_000.0 * 0.1 + 59_000.0 * 0.01) / 1e6;
        let long = (1_000.0 * 0.5 + 119_000.0 * 0.05) / 1e6;
        let expected = 2.0 * short + long;
        let model = &summary.models[0];
        assert!((model.estimated.priced_usd - expected).abs() < 1e-12);
        assert!(model.estimated.is_complete());
        let day = &summary.model_daily[0];
        assert!((day.estimated.priced_usd - expected).abs() < 1e-12);

        let mut snapshot = haiku_call(119_000);
        snapshot.granularity = crate::model::UsageGranularity::Aggregate;
        let base = (1_000.0 * 0.1 + 119_000.0 * 0.01) / 1e6;
        let copilot = Collection {
            usage_events: vec![haiku_call(119_000), snapshot.clone()],
            ..Collection::new(Provider::Copilot, "/tmp".into())
        };
        let summary = summarize(&copilot, now, 7, UtcOffset::UTC);
        assert!((summary.models[0].estimated.priced_usd - (long + base)).abs() < 1e-12);

        let combined = Collection {
            usage_events: vec![haiku_call(119_000), snapshot],
            ..Collection::new(Provider::Combined, "/tmp".into())
        };
        let summary = summarize(&combined, now, 7, UtcOffset::UTC);
        assert!((summary.models[0].estimated.priced_usd - (long + base)).abs() < 1e-12);
        assert!((summary.model_daily[0].estimated.priced_usd - (long + base)).abs() < 1e-12);
    }
}

#[cfg(test)]
mod v09_tests {
    use time::macros::{date, datetime};

    use super::*;
    use crate::model::LimitDay;
    use crate::model::{
        Collection, CreditSample, DurationEvent, EffortEvent, ModeEvent, PermissionEvent, Provider,
        RateLimitSample, SourceKind, UsageEvent,
    };

    fn skill_event(at: OffsetDateTime, skill: Option<&str>, tokens: u64) -> UsageEvent {
        UsageEvent {
            timestamp: Some(at),
            session_id: Some("s".to_owned()),
            model: Some("claude-fable-5".to_owned()),
            source_kind: SourceKind::Main,
            attribution_agent: None,
            attribution_skill: skill.map(ToOwned::to_owned),
            project: None,
            usage: TokenUsage {
                input_tokens: tokens,
                ..TokenUsage::default()
            },
            granularity: crate::model::UsageGranularity::Call,
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn v09_sections_follow_the_window_with_tristate_days() {
        let now = datetime!(2026-06-30 12:00 UTC);
        let collection = Collection {
            usage_events: vec![
                skill_event(
                    datetime!(2026-06-25 10:00 UTC),
                    Some("sk:review"),
                    1_000_000,
                ),
                skill_event(
                    datetime!(2026-05-20 10:00 UTC),
                    Some("sk:review"),
                    2_000_000,
                ),
                skill_event(datetime!(2026-06-22 09:00 UTC), None, 500),
            ],
            duration_events: vec![
                DurationEvent {
                    timestamp: Some(datetime!(2026-06-25 10:00 UTC)),
                    session_id: None,
                    duration_ms: 600_000,
                    human_wait_ms: 0,
                    model_ms: Some(300_000),
                    status: Some("turn".to_owned()),
                },
                DurationEvent {
                    timestamp: Some(datetime!(2026-05-20 10:00 UTC)),
                    session_id: None,
                    duration_ms: 600_000,
                    human_wait_ms: 0,
                    model_ms: Some(300_000),
                    status: Some("turn".to_owned()),
                },
            ],
            credit_samples: vec![
                CreditSample {
                    timestamp: datetime!(2026-06-25 10:00 UTC),
                    nano_aiu: 2_000_000_000,
                },
                CreditSample {
                    timestamp: datetime!(2026-05-20 10:00 UTC),
                    nano_aiu: 5_000_000_000,
                },
            ],
            rate_limit_samples: vec![
                RateLimitSample {
                    timestamp: datetime!(2026-06-22 08:00 UTC),
                    used_percent: 40.0,
                },
                RateLimitSample {
                    timestamp: datetime!(2026-06-22 09:01 UTC),
                    used_percent: 100.0,
                },
                RateLimitSample {
                    timestamp: datetime!(2026-05-20 09:00 UTC),
                    used_percent: 90.0,
                },
            ],
            effort_events: vec![
                EffortEvent {
                    timestamp: Some(datetime!(2026-06-25 10:00 UTC)),
                    effort: "xhigh".to_owned(),
                },
                EffortEvent {
                    timestamp: Some(datetime!(2026-06-25 11:00 UTC)),
                    effort: "xhigh".to_owned(),
                },
                EffortEvent {
                    timestamp: Some(datetime!(2026-06-25 12:00 UTC)),
                    effort: "low".to_owned(),
                },
            ],
            mode_events: vec![
                ModeEvent {
                    timestamp: Some(datetime!(2026-06-25 10:00 UTC)),
                    has_thinking: true,
                    fast: false,
                },
                ModeEvent {
                    timestamp: Some(datetime!(2026-06-25 11:00 UTC)),
                    has_thinking: false,
                    fast: false,
                },
                ModeEvent {
                    timestamp: Some(datetime!(2026-05-20 10:00 UTC)),
                    has_thinking: true,
                    fast: true,
                },
            ],
            permission_events: vec![
                PermissionEvent {
                    timestamp: Some(datetime!(2026-06-25 10:00 UTC)),
                    mode: "dontAsk".to_owned(),
                },
                PermissionEvent {
                    timestamp: Some(datetime!(2026-06-25 11:00 UTC)),
                    mode: "dontAsk".to_owned(),
                },
                PermissionEvent {
                    timestamp: Some(datetime!(2026-06-25 12:00 UTC)),
                    mode: "auto".to_owned(),
                },
                PermissionEvent {
                    timestamp: Some(datetime!(2026-05-20 10:00 UTC)),
                    mode: "default".to_owned(),
                },
            ],
            ..Collection::new(Provider::Claude, "/tmp".into())
        };

        let summary = summarize(&collection, now, 30, UtcOffset::UTC);

        assert_eq!(summary.skills.len(), 1);
        assert_eq!(summary.skills[0].name, "sk:review");
        assert_eq!(summary.skills[0].usage.token_volume(), 1_000_000);

        let limits = summary.limits.expect("limits history should exist");
        assert_eq!(limits.days.len(), 30);
        assert_eq!(limits.peak, Some((date!(2026 - 06 - 22), 100.0)));
        let day = |d: Date| {
            limits
                .days
                .iter()
                .find(|(date, _)| *date == d)
                .map(|(_, day)| *day)
                .expect("day should be in window")
        };
        assert_eq!(day(date!(2026 - 06 - 22)), LimitDay::Measured(100.0));
        assert_eq!(day(date!(2026 - 06 - 25)), LimitDay::NoSample);
        assert_eq!(day(date!(2026 - 06 - 03)), LimitDay::NoUse);

        assert_eq!(summary.modes.assistant_turns, 2);
        assert_eq!(summary.modes.thinking_turns, 1);
        assert_eq!(summary.modes.fast_turns, 0);
        assert_eq!(
            summary.modes.permissions,
            vec![("dontAsk".to_owned(), 2), ("auto".to_owned(), 1)]
        );
        assert_eq!(
            summary.modes.efforts,
            vec![("xhigh".to_owned(), 2), ("low".to_owned(), 1)]
        );

        let time = summary.active_time.as_ref().expect("active time");
        assert_eq!(summary.credits.as_ref().expect("credits").days.len(), 30);
        assert_eq!(summary.context.as_ref().expect("context").calls, 2);
        assert_eq!((time.turns, time.window_days), (1, 30));
        assert_eq!(time.active_per_day_ms(), 600_000 / 30);

        let wide = summarize(&collection, now, 90, UtcOffset::UTC);
        let wide_time = wide.active_time.as_ref().expect("active time");
        assert_eq!(wide.skills[0].usage.token_volume(), 3_000_000);
        assert_eq!(wide.limits.expect("limits").days.len(), 90);
        assert_eq!(wide.modes.assistant_turns, 3);
        assert_eq!(wide.credits.expect("credits").days.len(), 90);
        assert_eq!(wide.context.expect("context").calls, 3);
        assert_eq!((wide_time.turns, wide_time.window_days), (2, 90));
        assert_eq!(wide_time.active_per_day_ms(), 1_200_000 / 90);
    }
}
