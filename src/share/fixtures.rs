use crate::model::{
    DurationBucket, DurationSummary, ModelStat, Orchestration, Provider, ScanStats, Summary,
    TokenUsage,
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
        generated_at: time::OffsetDateTime::UNIX_EPOCH,
        period_days: 30,
        period_start: date!(2026 - 05 - 14),
        period_end: date!(2026 - 06 - 12),
        root: std::path::PathBuf::new(),
        scan_stats: ScanStats::default(),
        total_usage: usage.clone(),
        daily: Vec::new(),
        daily_sessions: Vec::new(),
        model_daily: Vec::new(),
        models: vec![ModelStat {
            name: "claude-opus-4-8".to_owned(),
            usage: usage.clone(),
            events: 10,
            active_days: 5,
        }],
        agents: Vec::new(),
        tools: Vec::new(),
        projects: vec![
            crate::model::ProjectStat {
                name: "agent-walker".to_owned(),
                usage: usage.clone(),
                events: 50,
            },
            crate::model::ProjectStat {
                name: "orchestra".to_owned(),
                usage: usage.clone(),
                events: 30,
            },
        ],
        sessions: 42,
        active_days: 30,
        previous_total_volume: 15_000_000,
        previous_sessions: 30,
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
        orchestration: Orchestration {
            parallel_rate: 0.62,
            avg_concurrency: 2.5,
            peak_concurrency: 4,
            span_count: 18,
            time_by_level: [144_000, 108_000, 54_000, 36_000, 18_000, 6_000],
        },
    }
}
