use crate::model::{
    ContextBand, ContextReason, ContextSummary, DurationBucket, DurationSummary, ModelStat,
    Orchestration, Provider, ScanStats, Summary, TokenUsage,
};

pub(crate) fn sample_summary() -> Summary {
    use time::macros::date;

    let usage = TokenUsage {
        input_tokens: 1_000_000,
        output_tokens: 500_000,
        cache_read_input_tokens: 20_000_000,
        ..TokenUsage::default()
    };
    let mut hourly = [0_u64; 24];
    hourly[16] = 1_000_000;
    hourly[22] = 400_000;
    Summary {
        provider: Provider::Combined,
        period_days: 30,
        period_start: date!(2026 - 05 - 14),
        period_end: date!(2026 - 06 - 12),
        root: std::path::PathBuf::new(),
        scan_stats: ScanStats::default(),
        total_usage: usage.clone(),
        recent_window_volume: usage.token_volume(),
        recent_window_active_days: 25,
        daily: Vec::new(),
        daily_sessions: Vec::new(),
        model_daily: Vec::new(),
        models: vec![ModelStat {
            name: "claude-opus-4-8".to_owned(),
            usage: usage.clone(),
            unreported_usage: usage.clone(),
            events: 10,
            reported_cost_usd: None,
        }],
        agents: Vec::new(),
        skills: Vec::new(),
        limits: None,
        credits: None,
        modes: crate::model::ModesSummary::default(),
        tools: Vec::new(),
        projects: vec![
            crate::model::ProjectStat {
                name: "agent-walker".to_owned(),
                usage: usage.clone(),
            },
            crate::model::ProjectStat {
                name: "orchestra".to_owned(),
                usage: usage.clone(),
            },
        ],
        sessions: 42,
        active_days: 30,
        previous_total_volume: 15_000_000,
        longest_streak_days: 5,
        current_streak_days: 3,
        most_active_day: None,
        hourly_usage: hourly,
        busiest_hour: Some((16, 1_000_000)),
        favorite_model: None,
        longest_session: None,
        completion_duration: Some(DurationSummary {
            count: 100,
            p50_ms: 120_000,
            p90_ms: 600_000,
            p95_ms: 900_000,
            max_ms: 3_000_000,
            buckets: vec![
                DurationBucket {
                    label: "<2m".into(),
                    count: 40,
                },
                DurationBucket {
                    label: "2-10m".into(),
                    count: 30,
                },
                DurationBucket {
                    label: "10-20m".into(),
                    count: 15,
                },
                DurationBucket {
                    label: "20-30m".into(),
                    count: 8,
                },
                DurationBucket {
                    label: "30-60m".into(),
                    count: 6,
                },
                DurationBucket {
                    label: "1h+".into(),
                    count: 1,
                },
            ],
        }),
        interrupted: 0,
        context: Some(sample_context()),
        orchestration: Orchestration {
            avg_concurrency: 2.5,
            peak_concurrency: 4,
            time_by_level: [144_000, 108_000, 54_000, 36_000, 18_000, 6_000],
        },
    }
}

/// Cache-reuse fixture: three populated bands (500K+ empty), one expiry
/// row and one cold-start row.
fn sample_context() -> ContextSummary {
    ContextSummary {
        calls: 1_200,
        context_tokens: 300_000_000,
        cached_tokens: 285_000_000,
        effective_tokens: 45_000_000,
        bands: vec![
            ContextBand {
                label: "<100K".into(),
                calls: 400,
                cached_effective: 2_000_000,
            },
            ContextBand {
                label: "100-200K".into(),
                calls: 500,
                cached_effective: 9_000_000,
            },
            ContextBand {
                label: "200-500K".into(),
                calls: 300,
                cached_effective: 17_000_000,
            },
            ContextBand {
                label: "500K+".into(),
                calls: 0,
                cached_effective: 0,
            },
        ],
        expired: Some(ContextReason {
            calls: 20,
            effective: 9_000_000,
        }),
        cold_start: Some(ContextReason {
            calls: 60,
            effective: 3_000_000,
        }),
        uncached: ContextReason {
            calls: 1_100,
            effective: 5_000_000,
        },
    }
}
