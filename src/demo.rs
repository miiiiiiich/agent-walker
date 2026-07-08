//! Synthetic report data used when `AGENT_WALKER_DEMO=1`.
//!
//! Everything is generated from a fixed-seed xorshift, so the demo looks the
//! same on every machine and run (apart from the date axis, which tracks the
//! current day).

use std::path::PathBuf;

use time::{Duration, OffsetDateTime, Time, Weekday};

use crate::analyzer::summarize;
use crate::app::Config;
use crate::model::{
    AppSummary, Collection, DurationEvent, EffortEvent, ModeEvent, Provider, RateLimitSample,
    SessionTouch, SourceKind, TokenUsage, ToolEvent, UsageEvent,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform value in `lo..hi`.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo).max(1)
    }

    /// True with probability `percent`/100.
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

const PROJECTS: [&str; 6] = [
    "acme/web-app",
    "acme/api",
    "acme/mobile",
    "oss/agent-walker",
    "side/blog",
    "lab/experiments",
];

const CLAUDE_TOOLS: [(&str, u64); 8] = [
    ("Bash", 30),
    ("Read", 26),
    ("Edit", 22),
    ("Write", 7),
    ("Grep", 9),
    ("Glob", 3),
    ("Task", 6),
    ("WebSearch", 2),
];

const SUBAGENTS: [&str; 3] = ["Explore", "general-purpose", "code-reviewer"];

/// Demo skill labels for the SKILLS section (Claude attribution).
const SKILLS: [&str; 6] = [
    "sk:review",
    "sk:release",
    "orc:inbox",
    "deep-research",
    "loop",
    "ref:api",
];

fn pick_skill(rng: &mut Rng, progress: f64) -> Option<String> {
    // Attribution fields only exist in recent logs; mirror that by tagging
    // mostly late-period events, at a modest rate like real data.
    if progress < 0.4 || !rng.chance(35) {
        return None;
    }
    let index = match rng.range(0, 100) {
        0..=29 => 0,
        30..=54 => 1,
        55..=74 => 2,
        75..=86 => 3,
        87..=94 => 4,
        _ => 5,
    };
    Some(SKILLS[index].to_owned())
}

const CODEX_TOOLS: [(&str, u64); 4] = [
    ("shell", 55),
    ("apply_patch", 30),
    ("update_plan", 10),
    ("web_search", 5),
];

/// Evening-heavy hour profile (weight per hour of day).
const HOUR_WEIGHTS: [u64; 24] = [
    4, 2, 1, 0, 0, 0, 0, 1, 2, 4, 6, 7, 8, 8, 9, 10, 12, 10, 8, 7, 8, 9, 8, 6,
];

fn pick_hour(rng: &mut Rng) -> u8 {
    let total: u64 = HOUR_WEIGHTS.iter().sum();
    let mut roll = rng.range(0, total);
    for (hour, weight) in HOUR_WEIGHTS.iter().enumerate() {
        if roll < *weight {
            return u8::try_from(hour).unwrap_or(0);
        }
        roll -= weight;
    }
    20
}

fn pick_project(rng: &mut Rng) -> String {
    // Heavily skewed toward the first projects, like real work.
    let index = match rng.range(0, 100) {
        0..=39 => 0,
        40..=64 => 1,
        65..=79 => 2,
        80..=89 => 3,
        90..=95 => 4,
        _ => 5,
    };
    PROJECTS[index].to_owned()
}

/// Claude model mix shifts over the period: an older Opus era hands over to
/// the newer one, with Haiku doing background work throughout.
fn pick_claude_model(rng: &mut Rng, progress: f64) -> &'static str {
    if rng.chance(6) {
        return "claude-haiku-4-5";
    }
    if progress > 0.85 && rng.chance(25) {
        return "claude-fable-5";
    }
    if progress < 0.45 {
        if rng.chance(92) {
            "claude-opus-4-7"
        } else {
            "claude-sonnet-4-6"
        }
    } else if rng.chance(94) {
        "claude-opus-4-8"
    } else {
        "claude-opus-4-7"
    }
}

/// Split a daily token volume into a cache-heavy usage block.
fn usage_block(rng: &mut Rng, volume: u64) -> TokenUsage {
    let cache_read = volume / 100 * 92;
    let cache_creation = volume / 100 * 5;
    let output = volume / 100 * 2;
    let input = volume.saturating_sub(cache_read + cache_creation + output);
    let five_minute = cache_creation / 100 * rng.range(60, 95);
    TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: cache_creation,
        cache_read_input_tokens: cache_read,
        cache_creation_ephemeral_5m_input_tokens: five_minute,
        cache_creation_ephemeral_1h_input_tokens: cache_creation.saturating_sub(five_minute),
        ..TokenUsage::default()
    }
}

/// Turn durations: mostly minutes, with a believable 20m+ autonomy tail.
fn turn_duration_ms(rng: &mut Rng) -> u64 {
    match rng.range(0, 100) {
        0..=34 => rng.range(20_000, 120_000),
        35..=69 => rng.range(120_000, 360_000),
        70..=89 => rng.range(360_000, 1_080_000),
        90..=95 => rng.range(1_200_000, 2_400_000),
        96..=98 => rng.range(2_400_000, 3_600_000),
        _ => rng.range(3_600_000, 5_400_000),
    }
}

/// Daily volume envelope: ramps up over the period, dips on weekends, and
/// keeps the first stretch quiet so the grass shows texture.
#[allow(
    clippy::cast_precision_loss,
    reason = "Synthetic noise factor is below 200; no precision at stake."
)]
fn daily_volume(rng: &mut Rng, progress: f64, weekday: Weekday) -> u64 {
    if progress < 0.18 {
        return 0;
    }
    if progress < 0.3 && rng.chance(55) {
        return 0;
    }
    let ramp = 4_000_000.0 + 56_000_000.0 * progress * progress;
    let noise = rng.range(50, 160) as f64 / 100.0;
    let weekend = matches!(weekday, Weekday::Saturday | Weekday::Sunday);
    let weekend_factor = if weekend { 0.35 } else { 1.0 };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "Synthetic volumes are small positive numbers."
    )]
    {
        (ramp * noise * weekend_factor) as u64
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Day indices are tiny; precision is irrelevant for fake data."
)]
fn claude_collection(now: OffsetDateTime, days: u16, rng: &mut Rng) -> Collection {
    let mut collection = Collection::new(Provider::Claude, PathBuf::from("demo data"));
    let total_days = i64::from(days.max(1));

    for day_index in 0..total_days {
        let date = (now - Duration::days(total_days - 1 - day_index)).date();
        let progress = day_index as f64 / total_days as f64;
        let volume = daily_volume(rng, progress, date.weekday());
        if volume == 0 {
            continue;
        }

        let sessions = rng.range(1, 4);
        for session_index in 0..sessions {
            let session_id = format!("demo-claude-{day_index}-{session_index}");
            let session_volume = volume / sessions;
            let chunks = rng.range(2, 5);
            let mut first_time: Option<OffsetDateTime> = None;
            let mut last_time: Option<OffsetDateTime> = None;

            for _ in 0..chunks {
                let hour = pick_hour(rng);
                let time = Time::from_hms(hour, u8::try_from(rng.range(0, 60)).unwrap_or(0), 0)
                    .unwrap_or(Time::MIDNIGHT);
                let timestamp = date.with_time(time).assume_offset(now.offset());
                first_time = Some(first_time.map_or(timestamp, |t| t.min(timestamp)));
                last_time = Some(last_time.map_or(timestamp, |t| t.max(timestamp)));

                collection.usage_events.push(UsageEvent {
                    timestamp: Some(timestamp),
                    session_id: Some(session_id.clone()),
                    model: Some(pick_claude_model(rng, progress).to_owned()),
                    source_kind: SourceKind::Main,
                    attribution_agent: None,
                    attribution_skill: pick_skill(rng, progress),
                    project: Some(pick_project(rng)),
                    usage: usage_block(rng, session_volume / chunks),
                    reported_cost_usd: None,
                });
                collection.mode_events.push(ModeEvent {
                    timestamp: Some(timestamp),
                    has_thinking: rng.chance(52),
                    fast: false,
                });

                for (tool, weight) in CLAUDE_TOOLS {
                    let calls = rng.range(0, weight / 2 + 2);
                    for _ in 0..calls {
                        let subagent = (tool == "Task").then(|| {
                            SUBAGENTS[usize::try_from(rng.range(0, 3)).unwrap_or(0)].to_owned()
                        });
                        collection.tool_events.push(ToolEvent {
                            timestamp: Some(timestamp),
                            session_id: Some(session_id.clone()),
                            tool_name: tool.to_owned(),
                            subagent_type: subagent,
                            source_kind: SourceKind::Main,
                        });
                    }
                }

                let turns = rng.range(2, 7);
                for _ in 0..turns {
                    collection.duration_events.push(DurationEvent {
                        timestamp: Some(timestamp),
                        session_id: Some(session_id.clone()),
                        duration_ms: turn_duration_ms(rng),
                        status: Some("turn".to_owned()),
                    });
                }
            }

            if let (Some(first), Some(last)) = (first_time, last_time) {
                collection.session_touches.push(SessionTouch {
                    timestamp: first,
                    session_id: session_id.clone(),
                });
                collection.session_touches.push(SessionTouch {
                    timestamp: last
                        + Duration::minutes(i64::try_from(rng.range(20, 150)).unwrap_or(30)),
                    session_id,
                });
            }
        }
    }

    collection.stats.files_seen = 412;
    collection.stats.lines_seen = 184_309;
    collection.stats.usage_events = collection.usage_events.len();
    collection.stats.tool_events = collection.tool_events.len();
    collection.stats.duration_events = collection.duration_events.len();
    collection
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Day indices are tiny; precision is irrelevant for fake data."
)]
fn codex_collection(now: OffsetDateTime, days: u16, rng: &mut Rng) -> Collection {
    let mut collection = Collection::new(Provider::Codex, PathBuf::from("demo data"));
    let total_days = i64::from(days.max(1));

    for day_index in 0..total_days {
        let date = (now - Duration::days(total_days - 1 - day_index)).date();
        let progress = day_index as f64 / total_days as f64;
        let volume = daily_volume(rng, progress, date.weekday()) / 4;
        if volume == 0 || rng.chance(35) {
            continue;
        }

        let session_id = format!("demo-codex-{day_index}");
        let hour = pick_hour(rng);
        let time = Time::from_hms(hour, u8::try_from(rng.range(0, 60)).unwrap_or(0), 0)
            .unwrap_or(Time::MIDNIGHT);
        let timestamp = date.with_time(time).assume_offset(now.offset());

        collection.usage_events.push(UsageEvent {
            timestamp: Some(timestamp),
            session_id: Some(session_id.clone()),
            model: Some("gpt-5.5".to_owned()),
            source_kind: SourceKind::Main,
            attribution_agent: None,
            attribution_skill: None,
            project: Some(pick_project(rng)),
            usage: usage_block(rng, volume),
            reported_cost_usd: None,
        });
        collection.effort_events.push(EffortEvent {
            timestamp: Some(timestamp),
            effort: if rng.chance(88) { "xhigh" } else { "low" }.to_owned(),
        });
        // Daily-peak 5h-window utilization; one mid-period day hits the limit
        // so the red bar and the peak note render in the demo.
        let used_percent = if day_index == total_days / 2 {
            100.0
        } else {
            rng.range(2, 65) as f64
        };
        collection.rate_limit_samples.push(RateLimitSample {
            timestamp,
            used_percent,
        });
        collection.session_touches.push(SessionTouch {
            timestamp,
            session_id: session_id.clone(),
        });
        collection.session_touches.push(SessionTouch {
            timestamp: timestamp
                + Duration::minutes(i64::try_from(rng.range(15, 90)).unwrap_or(30)),
            session_id: session_id.clone(),
        });

        for (tool, weight) in CODEX_TOOLS {
            let calls = rng.range(0, weight / 6 + 2);
            for _ in 0..calls {
                collection.tool_events.push(ToolEvent {
                    timestamp: Some(timestamp),
                    session_id: Some(session_id.clone()),
                    tool_name: tool.to_owned(),
                    subagent_type: None,
                    source_kind: SourceKind::Main,
                });
            }
        }

        let tasks = rng.range(1, 4);
        for _ in 0..tasks {
            collection.duration_events.push(DurationEvent {
                timestamp: Some(timestamp),
                session_id: Some(session_id.clone()),
                duration_ms: turn_duration_ms(rng),
                status: Some("task_complete".to_owned()),
            });
        }
    }

    collection.stats.files_seen = 96;
    collection.stats.lines_seen = 41_877;
    collection.stats.usage_events = collection.usage_events.len();
    collection.stats.tool_events = collection.tool_events.len();
    collection.stats.duration_events = collection.duration_events.len();
    collection
}

/// Build a complete synthetic report through the real analyzer, so the demo
/// exercises exactly the rendering paths real data does.
pub fn demo_report(config: &Config) -> AppSummary {
    let now = OffsetDateTime::now_utc().to_offset(config.local_offset);
    let mut rng = Rng(0x5EED_CAFE_F00D_0001);

    let collections = vec![
        claude_collection(now, config.days, &mut rng),
        codex_collection(now, config.days, &mut rng),
        Collection::new(Provider::Agy, PathBuf::from("demo data")),
    ];

    let providers = collections
        .iter()
        .map(|collection| summarize(collection, now, config.days, config.local_offset))
        .collect::<Vec<_>>();
    let combined = summarize(
        &Collection::combined(PathBuf::from("demo data"), &collections),
        now,
        config.days,
        config.local_offset,
    );

    AppSummary {
        generated_at: now,
        period_days: config.days.max(1),
        load_duration_ms: 0,
        combined,
        providers,
    }
}
