use std::collections::HashMap;

use time::{Date, Duration, OffsetDateTime, UtcOffset};

use crate::cost::pricing_for;
use crate::model::{
    Collection, ContextBand, ContextReason, ContextSummary, Provider, SourceKind, UsageEvent,
};

/// Shared context-size bands let the Total tab add provider summaries element-wise.
const BANDS: [(&str, u64); 4] = [
    ("<100K", 100_000),
    ("100-200K", 200_000),
    ("200-500K", 500_000),
    ("500K+", u64::MAX),
];

const DEFAULT_READ_MULTIPLIER: f64 = 0.1;
const DEFAULT_WRITE_5M_MULTIPLIER: f64 = 1.25;
const DEFAULT_WRITE_1H_MULTIPLIER: f64 = 2.0;

/// Infer retention from the serving model; leave unknown families unclassified
/// rather than fabricating expiry rows.
fn retention(provider: Provider, model: Option<&str>) -> Option<Duration> {
    // Claude Code uses the 1h ttl (the 5m default would misfile calls
    // resumed within the hour as expired); OpenAI keeps a prefix 30 minutes
    // after the last write or reuse.
    const CLAUDE: Duration = Duration::hours(1);
    const OPENAI: Duration = Duration::minutes(30);
    match provider {
        Provider::Claude => Some(CLAUDE),
        Provider::Codex => Some(OPENAI),
        _ => {
            let family = model.unwrap_or_default().to_ascii_lowercase();
            if family.contains("claude") {
                Some(CLAUDE)
            } else if ["gpt", "codex", "o1", "o3", "o4"]
                .iter()
                .any(|prefix| family.starts_with(prefix))
            {
                Some(OPENAI)
            } else {
                None
            }
        }
    }
}

struct Multipliers {
    read: f64,
    write_5m: f64,
    write_1h: f64,
}

fn multipliers(model: Option<&str>) -> Multipliers {
    let fallback = Multipliers {
        read: DEFAULT_READ_MULTIPLIER,
        write_5m: DEFAULT_WRITE_5M_MULTIPLIER,
        write_1h: DEFAULT_WRITE_1H_MULTIPLIER,
    };
    let Some(pricing) = model.and_then(pricing_for) else {
        return fallback;
    };
    if pricing.input <= 0.0 {
        return fallback;
    }
    Multipliers {
        read: pricing.cache_read / pricing.input,
        write_5m: pricing.cache_write_5m / pricing.input,
        write_1h: pricing.cache_write_1h / pricing.input,
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Token counts are far below 2^52; the weighting is display-only."
)]
fn weighted(tokens: u64, multiplier: f64) -> u64 {
    (tokens as f64 * multiplier).round() as u64
}

fn band_index(context: u64) -> usize {
    BANDS
        .iter()
        .position(|(_, upper)| context < *upper)
        .unwrap_or(BANDS.len() - 1)
}

struct CallAccount {
    context: u64,
    cached: u64,
    uncached: u64,
    uncached_effective: u64,
    cached_effective: u64,
}

fn account_call(event: &UsageEvent) -> CallAccount {
    let usage = &event.usage;
    let m = multipliers(event.model.as_deref());
    // The 5m/1h split comes from untrusted logs; the same validation as
    // `usage_cost_usd`: a missing, broken, or overflowing split prices every
    // write at the 5m rate, so this panel and COST agree on the same row.
    let split = usage
        .cache_creation_ephemeral_5m_input_tokens
        .checked_add(usage.cache_creation_ephemeral_1h_input_tokens);
    let (short_writes, long_writes) = match split {
        Some(split) if split > 0 && split <= usage.cache_creation_input_tokens => (
            usage.cache_creation_ephemeral_5m_input_tokens
                + (usage.cache_creation_input_tokens - split),
            usage.cache_creation_ephemeral_1h_input_tokens,
        ),
        _ => (usage.cache_creation_input_tokens, 0),
    };
    // Counters come from untrusted logs: a poisoned row saturates instead
    // of wrapping (release) or panicking (debug).
    let uncached = usage
        .input_tokens
        .saturating_add(usage.cache_creation_input_tokens);
    CallAccount {
        context: uncached.saturating_add(usage.cache_read_input_tokens),
        cached: usage.cache_read_input_tokens,
        uncached,
        uncached_effective: usage
            .input_tokens
            .saturating_add(weighted(short_writes, m.write_5m))
            .saturating_add(weighted(long_writes, m.write_1h)),
        cached_effective: weighted(usage.cache_read_input_tokens, m.read),
    }
}

/// Aggregate usage events feed totals but not call-level rows: dividing session
/// or multi-call volume by aggregate record counts would misstate per-call figures.
fn call_level(provider: Provider) -> bool {
    !matches!(provider, Provider::Copilot | Provider::Grok)
}

fn empty_summary() -> ContextSummary {
    ContextSummary {
        bands: BANDS
            .iter()
            .map(|(label, _)| ContextBand {
                label: (*label).to_owned(),
                ..ContextBand::default()
            })
            .collect(),
        ..ContextSummary::default()
    }
}

fn add_totals(summary: &mut ContextSummary, call: &CallAccount) {
    summary.context_tokens = summary.context_tokens.saturating_add(call.context);
    summary.cached_tokens = summary.cached_tokens.saturating_add(call.cached);
}

fn add_reason(reason: &mut ContextReason, call: &CallAccount) {
    reason.calls += 1;
    reason.effective = reason.effective.saturating_add(call.uncached_effective);
}

/// Exclude sidechains from call rows because shared session IDs would interleave parallel chains.
/// Find predecessors before filtering the window; missing history can make a resumed session appear cold.
pub(super) fn context_summary(
    collection: &Collection,
    window_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
) -> Option<ContextSummary> {
    if collection.provider == Provider::Combined {
        return None;
    }
    let call_level = call_level(collection.provider);
    let in_window = |timestamp: OffsetDateTime| {
        let date = timestamp.to_offset(local_offset).date();
        date >= window_start && date <= period_end
    };

    let mut summary = empty_summary();
    let mut by_session: HashMap<Option<&str>, Vec<&UsageEvent>> = HashMap::new();
    for event in &collection.usage_events {
        let Some(timestamp) = event.timestamp else {
            continue;
        };
        let call = account_call(event);
        if call.context == 0 {
            continue;
        }
        if call_level && event.source_kind == SourceKind::Main {
            by_session
                .entry(event.session_id.as_deref())
                .or_default()
                .push(event);
        } else if in_window(timestamp) {
            add_totals(&mut summary, &call);
            summary.unclassified_effective = summary
                .unclassified_effective
                .saturating_add(call.uncached_effective)
                .saturating_add(call.cached_effective);
        }
    }

    let mut expired = ContextReason::default();
    let mut cold_start = ContextReason::default();
    // "Has sessions" and "expiry is classifiable" are different facts: a
    // sessionful provider whose models have no known retention can count
    // cold starts but must not show an `expired 0` that means "unknown".
    let mut sessionful_in_window = false;
    let mut expiry_classifiable = false;

    for (session, mut events) in by_session {
        events.sort_by_key(|event| event.timestamp);
        let sessionful = session.is_some();
        // Cold start is per session; the expiry gap is per serving model —
        // a session that alternates models keeps one prefix per model, and a
        // call in between on another model neither refreshes nor expires it.
        let mut first_seen = false;
        let mut previous_by_model: HashMap<Option<&str>, OffsetDateTime> = HashMap::new();
        for event in events {
            let timestamp = event.timestamp.expect("filtered to dated events");
            let first = !first_seen;
            first_seen = true;
            let gap = previous_by_model
                .insert(event.model.as_deref(), timestamp)
                .map(|prev| timestamp - prev);
            if !in_window(timestamp) {
                continue;
            }
            let call = account_call(event);
            summary.calls += 1;
            add_totals(&mut summary, &call);
            let band = &mut summary.bands[band_index(call.context)];
            band.calls += 1;
            band.cached_effective = band.cached_effective.saturating_add(call.cached_effective);

            let low_reuse = call.uncached.saturating_mul(2) >= call.context;
            let retention = retention(collection.provider, event.model.as_deref());
            let expired_gap = retention.is_some_and(|keep| gap.is_some_and(|gap| gap >= keep));
            sessionful_in_window |= sessionful;
            expiry_classifiable |= sessionful && retention.is_some();
            let reason = if sessionful && first {
                &mut cold_start
            } else if sessionful && low_reuse && expired_gap {
                &mut expired
            } else {
                &mut summary.uncached
            };
            add_reason(reason, &call);
        }
    }

    if summary.context_tokens == 0 {
        return None;
    }
    if sessionful_in_window {
        summary.cold_start = Some(cold_start);
    }
    if expiry_classifiable {
        summary.expired = Some(expired);
    }
    summary.finalize();
    Some(summary)
}

#[cfg(test)]
mod tests {
    use time::macros::{date, datetime};

    use super::*;
    use crate::model::TokenUsage;

    fn event(session: &str, at: OffsetDateTime, input: u64, write: u64, read: u64) -> UsageEvent {
        UsageEvent {
            timestamp: Some(at),
            session_id: Some(session.to_owned()),
            model: Some("model-nobody-priced".to_owned()),
            source_kind: SourceKind::Main,
            attribution_agent: None,
            attribution_skill: None,
            project: None,
            usage: TokenUsage {
                input_tokens: input,
                cache_creation_input_tokens: write,
                cache_read_input_tokens: read,
                ..TokenUsage::default()
            },
            reported_cost_usd: None,
        }
    }

    fn collection(provider: Provider, events: Vec<UsageEvent>) -> Collection {
        Collection {
            usage_events: events,
            ..Collection::new(provider, "/tmp".into())
        }
    }

    #[test]
    fn classifies_cold_start_and_expiry_per_session() {
        let t0 = datetime!(2026-06-08 10:00 UTC);
        let events = vec![
            event("s1", t0, 1_000, 59_000, 0), // cold start, 60K uncached
            event("s1", t0 + Duration::minutes(2), 500, 2_000, 60_000), // high reuse
            event("s1", t0 + Duration::hours(2), 0, 70_000, 0), // expired (gap 2h ≥ 1h)
            event(
                "s1",
                t0 + Duration::hours(2) + Duration::minutes(1),
                0,
                65_000,
                5_000,
            ), // low reuse, not expired
        ];
        let summary = context_summary(
            &collection(Provider::Claude, events),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("summary");

        assert_eq!(summary.calls, 4);
        assert_eq!(summary.cached_tokens, 65_000);
        let cold = summary.cold_start.expect("cold start");
        assert_eq!((cold.calls, cold.effective), (1, 1_000 + 73_750));
        let expired = summary.expired.expect("expired");
        assert_eq!((expired.calls, expired.effective), (1, 87_500));
        assert_eq!(summary.uncached.calls, 2);
        assert_eq!(summary.uncached.effective, 3_000 + 81_250);
        assert_eq!(summary.bands[0].calls, 4);
        assert!(summary.bands[1..].iter().all(|band| band.calls == 0));
        assert_eq!(summary.bands[0].cached_effective, 6_000 + 500);
    }

    #[test]
    fn warm_first_call_is_still_a_cold_start() {
        let t0 = datetime!(2026-06-08 10:00 UTC);
        let events = vec![event("s1", t0, 2_000, 0, 98_000)];
        let summary = context_summary(
            &collection(Provider::Claude, events),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("summary");
        let cold = summary.cold_start.expect("cold start");
        assert_eq!((cold.calls, cold.effective), (1, 2_000));
    }

    #[test]
    fn retention_threshold_is_per_provider() {
        let t0 = datetime!(2026-06-08 10:00 UTC);
        let events = || {
            vec![
                event("s1", t0, 50_000, 0, 0),
                event("s1", t0 + Duration::minutes(40), 50_000, 0, 0),
            ]
        };
        let codex = context_summary(
            &collection(Provider::Codex, events()),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("codex");
        let claude = context_summary(
            &collection(Provider::Claude, events()),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("claude");
        assert_eq!(codex.expired.expect("codex expired").calls, 1);
        assert_eq!(claude.expired.expect("claude expired").calls, 0);
    }

    #[test]
    fn window_and_source_gates() {
        let before = datetime!(2026-05-20 10:00 UTC);
        let mut side = event("s1", before + Duration::days(20), 50_000, 0, 0);
        side.source_kind = SourceKind::Subagent;
        let events = vec![
            event("s1", before, 50_000, 0, 0),
            event("s1", before + Duration::days(20), 50_000, 0, 0),
            side,
        ];
        let summary = context_summary(
            &collection(Provider::Claude, events.clone()),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("summary");
        assert_eq!(summary.calls, 1);
        assert_eq!(summary.context_tokens, 100_000);
        assert_eq!(summary.cold_start.expect("cold start").calls, 0);
        assert_eq!(summary.expired.expect("expired").calls, 1);

        let copilot = context_summary(
            &collection(Provider::Copilot, events.clone()),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("copilot totals");
        assert_eq!((copilot.calls, copilot.context_tokens), (0, 100_000));
        assert!(copilot.expired.is_none() && copilot.cold_start.is_none());
        assert_eq!((copilot.uncached.calls, copilot.uncached.effective), (0, 0));
        assert_eq!(copilot.unclassified_effective, 100_000);
        let grok = context_summary(
            &collection(Provider::Grok, events.clone()),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("grok totals");
        assert_eq!(
            (grok.calls, grok.uncached.calls, grok.context_tokens),
            (0, 0, 100_000)
        );
        assert!(
            context_summary(
                &collection(Provider::Combined, events),
                date!(2026 - 06 - 01),
                date!(2026 - 06 - 30),
                UtcOffset::UTC,
            )
            .is_none()
        );
    }

    #[test]
    fn long_ttl_writes_weigh_more() {
        let mut e = event("s1", datetime!(2026-06-08 10:00 UTC), 0, 10_000, 0);
        e.usage.cache_creation_ephemeral_1h_input_tokens = 4_000;
        let summary = context_summary(
            &collection(Provider::Claude, vec![e]),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("summary");
        assert_eq!(summary.effective_tokens, 15_500);

        let mut clamped = event("s1", datetime!(2026-06-08 10:00 UTC), 0, 10_000, 0);
        clamped.usage.cache_creation_ephemeral_1h_input_tokens = 50_000;
        let summary = context_summary(
            &collection(Provider::Claude, vec![clamped]),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("summary");
        assert_eq!(summary.effective_tokens, 12_500);
    }

    #[test]
    fn priced_model_uses_table_multipliers() {
        crate::cost::tests::install_test_pricing();
        // Use nonfallback cache ratios so this test distinguishes the pricing-table path.
        let mut e = event("s1", datetime!(2026-06-08 10:00 UTC), 0, 10_000, 100_000);
        e.model = Some("gpt-5.5".to_owned());
        let summary = context_summary(
            &collection(Provider::Codex, vec![e]),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("summary");
        assert_eq!(summary.effective_tokens, 10_000);
    }

    #[test]
    fn saturated_counters_do_not_overflow() {
        let e = event(
            "s1",
            datetime!(2026-06-08 10:00 UTC),
            u64::MAX,
            u64::MAX,
            u64::MAX,
        );
        let summary = context_summary(
            &collection(Provider::Claude, vec![e]),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("summary");
        assert_eq!(summary.context_tokens, u64::MAX);
        assert_eq!(summary.effective_tokens, u64::MAX);
        assert!(summary.bands.iter().all(|band| band.calls == 0));
        assert!(summary.cold_start.is_none() && summary.expired.is_none());
    }

    #[test]
    fn retention_follows_the_serving_model_elsewhere() {
        let t0 = datetime!(2026-06-08 10:00 UTC);
        let events = |model: &str| {
            let mut a = event("s1", t0, 50_000, 0, 0);
            let mut b = event("s1", t0 + Duration::minutes(40), 50_000, 0, 0);
            a.model = Some(model.to_owned());
            b.model = Some(model.to_owned());
            vec![a, b]
        };
        let run = |model: &str| {
            context_summary(
                &collection(Provider::OpenCode, events(model)),
                date!(2026 - 06 - 01),
                date!(2026 - 06 - 30),
                UtcOffset::UTC,
            )
            .expect("summary")
        };
        assert_eq!(run("gpt-5.5").expired.expect("expired").calls, 1);
        assert_eq!(run("claude-opus-4-8").expired.expect("expired").calls, 0);
        assert!(run("qwen3:8b").expired.is_none());
        assert_eq!(run("qwen3:8b").cold_start.expect("cold start").calls, 1);
        assert_eq!(run("qwen3:8b").uncached.calls, 1);
    }

    #[test]
    fn expiry_gap_is_per_serving_model() {
        let t0 = datetime!(2026-06-08 10:00 UTC);
        let mut gpt_a = event("s1", t0, 50_000, 0, 0);
        let mut qwen = event("s1", t0 + Duration::minutes(59), 50_000, 0, 0);
        let mut gpt_b = event("s1", t0 + Duration::minutes(60), 50_000, 0, 0);
        gpt_a.model = Some("gpt-5.5".to_owned());
        qwen.model = Some("qwen3:8b".to_owned());
        gpt_b.model = Some("gpt-5.5".to_owned());
        let summary = context_summary(
            &collection(Provider::OpenCode, vec![gpt_a, qwen, gpt_b]),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("summary");
        assert_eq!(summary.expired.expect("expired").calls, 1);
        assert_eq!(summary.cold_start.expect("cold start").calls, 1);
    }

    #[test]
    fn sessionless_events_skip_reasons() {
        let mut e = event("x", datetime!(2026-06-08 10:00 UTC), 50_000, 0, 0);
        e.session_id = None;
        let summary = context_summary(
            &collection(Provider::Cursor, vec![e]),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("summary");
        assert_eq!(summary.calls, 1);
        assert!(summary.expired.is_none() && summary.cold_start.is_none());
    }
}
