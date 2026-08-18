mod aggregates;
mod concurrency;
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
    let mut model_daily_unreported = aggregates.model_daily_unreported;
    let model_daily = aggregates
        .model_daily_usage
        .into_iter()
        .map(|((date, model), usage)| {
            let key = (date, model.clone());
            let reported_cost_usd = model_daily_reported.get(&key).copied();
            let unreported_usage = model_daily_unreported.remove(&key).unwrap_or_default();
            ModelDailyStat {
                date,
                model,
                usage,
                unreported_usage,
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
    let interrupted =
        duration::interrupted_count(collection, period_start, period_end, local_offset);
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
    // The v0.9 sections (SKILLS / LIMITS / MODES) share this fixed 30-day
    // window: attribution fields exist only in recent logs, and the mode /
    // limit views are "how you've been using it lately" by design.
    let mut skill_map: BTreeMap<String, TokenUsage> = BTreeMap::new();
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
        codename_window_start,
        period_end,
        local_offset,
        &recent_active_days,
    );
    let credits =
        credits::credits_history(collection, codename_window_start, period_end, local_offset);
    let mode_usage =
        modes::modes_summary(collection, codename_window_start, period_end, local_offset);
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

    /// Interruptions are independent of completed-turn stats: a window with
    /// interruptions but no completed turn reports the count and no duration
    /// summary; undated or out-of-window interrupts are excluded.
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

        // No completed turn → no duration stats; the interruption count is
        // its own metric and stays visible regardless.
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
                    reported_cost_usd: None,
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
            ..Collection::new(Provider::Claude, "/tmp/claude".into())
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
            attribution_skill: None,
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
            usage_events: events,
            ..Collection::new(Provider::Claude, "/tmp".into())
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

    #[test]
    fn reported_and_unreported_costs_split_per_model() {
        // Same model name on the same day: one event reports a cost (Cursor),
        // one doesn't (Claude Code). The model's reported cost and its
        // LiteLLM-priced (unreported) token subset must stay separable.
        let now = datetime!(2026-06-08 12:00 UTC);
        let usage = |input: u64| TokenUsage {
            input_tokens: input,
            ..TokenUsage::default()
        };
        let event = |reported: Option<f64>, input: u64| UsageEvent {
            timestamp: Some(datetime!(2026-06-07 10:00 UTC)),
            session_id: None,
            model: Some("shared-model".to_owned()),
            source_kind: SourceKind::Main,
            attribution_agent: None,
            attribution_skill: None,
            project: None,
            usage: usage(input),
            reported_cost_usd: reported,
        };
        let collection = Collection {
            usage_events: vec![event(Some(0.5), 100), event(None, 300)],
            ..Collection::new(Provider::Combined, "/tmp".into())
        };

        let summary = summarize(&collection, now, 7, UtcOffset::UTC);
        let model = summary
            .models
            .iter()
            .find(|stat| stat.name == "shared-model")
            .expect("model present");
        assert_eq!(model.usage.input_tokens, 400);
        assert_eq!(model.reported_cost_usd, Some(0.5));
        // Only the unreported 300 tokens should be priced from LiteLLM.
        assert_eq!(model.unreported_usage.input_tokens, 300);

        let day = summary
            .model_daily
            .iter()
            .find(|stat| stat.model == "shared-model")
            .expect("model-day present");
        assert_eq!(day.reported_cost_usd, Some(0.5));
        assert_eq!(day.unreported_usage.input_tokens, 300);
    }
}

#[cfg(test)]
mod v09_tests {
    use time::macros::{date, datetime};

    use super::*;
    use crate::model::LimitDay;
    use crate::model::{
        Collection, EffortEvent, ModeEvent, PermissionEvent, Provider, RateLimitSample, SourceKind,
        UsageEvent,
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
            reported_cost_usd: None,
        }
    }

    /// SKILLS / LIMITS / MODES all cut on the fixed 30-day codename window
    /// (display `--days` = 90 here), and LIMITS days are tri-state.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn v09_sections_use_fixed_window_and_tristate_days() {
        let now = datetime!(2026-06-30 12:00 UTC);
        let collection = Collection {
            usage_events: vec![
                // In the 30d window (06-01..06-30) with a skill.
                skill_event(
                    datetime!(2026-06-25 10:00 UTC),
                    Some("sk:review"),
                    1_000_000,
                ),
                // In the 90d display window but OUTSIDE the fixed 30d window:
                // must not appear in SKILLS.
                skill_event(
                    datetime!(2026-05-20 10:00 UTC),
                    Some("sk:review"),
                    2_000_000,
                ),
                // Active day without any rate-limit sample -> NoSample.
                skill_event(datetime!(2026-06-22 09:00 UTC), None, 500),
            ],
            rate_limit_samples: vec![
                RateLimitSample {
                    timestamp: datetime!(2026-06-22 08:00 UTC),
                    used_percent: 40.0,
                },
                // Same day, later, higher: the day keeps its PEAK.
                RateLimitSample {
                    timestamp: datetime!(2026-06-22 09:01 UTC),
                    used_percent: 100.0,
                },
                // Outside the 30d window: ignored.
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
                // Outside the 30d window: ignored.
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
                // Outside the 30d window: ignored.
                PermissionEvent {
                    timestamp: Some(datetime!(2026-05-20 10:00 UTC)),
                    mode: "default".to_owned(),
                },
            ],
            ..Collection::new(Provider::Combined, "/tmp".into())
        };

        let summary = summarize(&collection, now, 90, UtcOffset::UTC);

        // SKILLS: only the in-window 1M event counts.
        assert_eq!(summary.skills.len(), 1);
        assert_eq!(summary.skills[0].name, "sk:review");
        assert_eq!(summary.skills[0].usage.token_volume(), 1_000_000);

        // LIMITS: 30 tri-state days, daily PEAK, window-filtered.
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

        // MODES: window-filtered turns and effort distribution.
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
    }
}
