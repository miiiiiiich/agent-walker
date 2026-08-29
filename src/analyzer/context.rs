//! CONTEXT panel data: cache reuse per call, input-equivalent cost by context
//! size, and the two behaviours that pay full price for a whole prefix —
//! starting a session and resuming one after the cache expired.
use std::collections::HashMap;

use time::{Date, Duration, OffsetDateTime, UtcOffset};

use crate::cost::pricing_for;
use crate::model::{
    Collection, ContextBand, ContextReason, ContextSummary, Provider, SourceKind, UsageEvent,
};

/// Context-size bands shared by every tab so the Total tab can add provider
/// summaries element-wise. Upper bounds are exclusive.
const BANDS: [(&str, u64); 4] = [
    ("<100K", 100_000),
    ("100-200K", 200_000),
    ("200-500K", 500_000),
    ("500K+", u64::MAX),
];

/// Fallback input-equivalent multipliers when the model has no `LiteLLM`
/// price: Anthropic's and `OpenAI`'s published cache pricing.
const DEFAULT_READ_MULTIPLIER: f64 = 0.1;
const DEFAULT_WRITE_5M_MULTIPLIER: f64 = 1.25;
const DEFAULT_WRITE_1H_MULTIPLIER: f64 = 2.0;

/// How long a provider keeps a cached prefix alive after its last use. A
/// low-reuse call after a longer silence paid for the whole prefix again.
fn retention(provider: Provider) -> Duration {
    match provider {
        // GPT-5.6+: eligible for 30 minutes after the last write or reuse.
        Provider::Codex => Duration::minutes(30),
        // Claude Code uses the 1h ttl (the 5m default would misfile calls
        // resumed within the hour as expired); other providers get the same
        // conservative hour.
        _ => Duration::hours(1),
    }
}

struct Multipliers {
    read: f64,
    write_5m: f64,
    write_1h: f64,
}

fn multipliers(model: Option<&str>) -> Multipliers {
    // The fallback covers an unresolved model or a table without an input
    // price. A resolved model keeps its table ratios as they are — a zero
    // cache-write rate (OpenAI) is a real price, and overriding it would put
    // this panel at odds with COST.
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

/// One call's token accounting: raw context / cached / uncached plus the
/// input-equivalent (price-weighted) volumes.
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
    let long_writes = usage
        .cache_creation_ephemeral_1h_input_tokens
        .min(usage.cache_creation_input_tokens);
    let short_writes = usage.cache_creation_input_tokens - long_writes;
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

/// Summarize cache reuse over the fixed window. Only main-chain calls take
/// part: sidechain rows share the parent's session id and would interleave
/// parallel call chains. Predecessors are found before the window filter so
/// the first in-window call of a running session is not a false cold start.
/// The combined collection gets `None` — the Total tab sums the providers.
pub(super) fn context_summary(
    collection: &Collection,
    window_start: Date,
    period_end: Date,
    local_offset: UtcOffset,
) -> Option<ContextSummary> {
    // Combined is summed from the providers by the caller. Copilot's usage
    // events are per-shutdown deltas of cumulative counters, not calls —
    // one event can cover a whole session — so call-level analysis would
    // file a session as a single huge cold start.
    if matches!(collection.provider, Provider::Combined | Provider::Copilot) {
        return None;
    }
    let retention = retention(collection.provider);
    let in_window = |timestamp: OffsetDateTime| {
        let date = timestamp.to_offset(local_offset).date();
        date >= window_start && date <= period_end
    };

    let mut by_session: HashMap<Option<&str>, Vec<&UsageEvent>> = HashMap::new();
    for event in &collection.usage_events {
        if event.source_kind != SourceKind::Main || event.timestamp.is_none() {
            continue;
        }
        if account_call(event).context == 0 {
            continue;
        }
        by_session
            .entry(event.session_id.as_deref())
            .or_default()
            .push(event);
    }

    let mut summary = ContextSummary {
        bands: BANDS
            .iter()
            .map(|(label, _)| ContextBand {
                label: (*label).to_owned(),
                ..ContextBand::default()
            })
            .collect(),
        ..ContextSummary::default()
    };
    let mut expired = ContextReason::default();
    let mut cold_start = ContextReason::default();
    let mut has_sessions = false;

    for (session, mut events) in by_session {
        events.sort_by_key(|event| event.timestamp);
        let sessionful = session.is_some();
        has_sessions |= sessionful;
        let mut previous: Option<OffsetDateTime> = None;
        for event in events {
            let timestamp = event.timestamp.expect("filtered to dated events");
            let first = previous.is_none();
            let gap = previous.map(|prev| timestamp - prev);
            previous = Some(timestamp);
            if !in_window(timestamp) {
                continue;
            }
            let call = account_call(event);
            summary.calls += 1;
            summary.context_tokens = summary.context_tokens.saturating_add(call.context);
            summary.cached_tokens = summary.cached_tokens.saturating_add(call.cached);
            summary.effective_tokens = summary
                .effective_tokens
                .saturating_add(call.uncached_effective)
                .saturating_add(call.cached_effective);
            let band = &mut summary.bands[band_index(call.context)];
            band.calls += 1;
            band.cached_effective = band.cached_effective.saturating_add(call.cached_effective);

            if !sessionful {
                continue;
            }
            // Cold start = every session's first call, whatever the cache
            // did (a sibling session may have warmed the prefix; the
            // uncached part is still the price of starting). Expired =
            // low reuse (uncached ≥ half the context) after a silence
            // longer than the provider keeps a prefix.
            let low_reuse = call.uncached.saturating_mul(2) >= call.context;
            let reason = if first {
                Some(&mut cold_start)
            } else if low_reuse && gap.is_some_and(|gap| gap >= retention) {
                Some(&mut expired)
            } else {
                None
            };
            if let Some(reason) = reason {
                reason.calls += 1;
                reason.effective = reason.effective.saturating_add(call.uncached_effective);
            }
        }
    }

    if summary.calls == 0 {
        return None;
    }
    if has_sessions {
        summary.expired = Some(expired);
        summary.cold_start = Some(cold_start);
    }
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

    /// A session's first call is a cold start; a low-reuse call after the
    /// retention window is expired; a low-reuse call inside the window is
    /// neither; high-reuse calls only feed the bands and cache totals.
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
        // Reason rows carry the input-equivalent uncached cost: input × 1 plus
        // 5m cache writes × 1.25 (unpriced model → fallback multipliers).
        let cold = summary.cold_start.expect("cold start");
        assert_eq!((cold.calls, cold.effective), (1, 1_000 + 73_750));
        let expired = summary.expired.expect("expired");
        assert_eq!((expired.calls, expired.effective), (1, 87_500));
        // Every call sits below 100K context → one band populated.
        assert_eq!(summary.bands[0].calls, 4);
        assert!(summary.bands[1..].iter().all(|band| band.calls == 0));
        // Unpriced model → fallback multipliers: 60K cached reads ≈ 6K effective
        // in the high-reuse call, 5K → 500 in the last.
        assert_eq!(summary.bands[0].cached_effective, 6_000 + 500);
    }

    /// A session's first call counts as a cold start even when a sibling
    /// session had warmed the prefix — only its uncached part is charged.
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

    /// Codex keeps a prefix for 30 minutes: a 40-minute gap is expired there
    /// but not on Claude, whose retention is an hour.
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

    /// The predecessor is found before the window filter: the first in-window
    /// call of a session that started earlier is not a cold start. Sidechain
    /// rows and the combined provider contribute nothing.
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
        assert_eq!(summary.cold_start.expect("cold start").calls, 0);
        assert_eq!(summary.expired.expect("expired").calls, 1);

        assert!(
            context_summary(
                &collection(Provider::Copilot, events.clone()),
                date!(2026 - 06 - 01),
                date!(2026 - 06 - 30),
                UtcOffset::UTC,
            )
            .is_none()
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

    /// Cache writes split by ttl: the 1h share is weighted 2×, the rest
    /// 1.25× (fallback multipliers); an over-reported 1h share is clamped.
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
        // 6,000 × 1.25 + 4,000 × 2 = 15,500
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
        assert_eq!(summary.effective_tokens, 20_000);
    }

    /// A priced model takes its multipliers from the pricing table instead
    /// of the fallback constants.
    #[test]
    fn priced_model_uses_table_multipliers() {
        crate::cost::tests::install_test_pricing();
        // Test table: claude-opus-4-8 input $5, cache_read $0.5 (0.1×),
        // write 5m $6.25 (1.25×), write 1h $10 (2×) — same as the fallback,
        // so pin a model whose ratios differ: gpt-5.5 has no write price.
        let mut e = event("s1", datetime!(2026-06-08 10:00 UTC), 0, 10_000, 100_000);
        e.model = Some("gpt-5.5".to_owned());
        let summary = context_summary(
            &collection(Provider::Codex, vec![e]),
            date!(2026 - 06 - 01),
            date!(2026 - 06 - 30),
            UtcOffset::UTC,
        )
        .expect("summary");
        // cache_read $0.5 / $5 = 0.1× → 10,000; the table prices writes at
        // $0 for this model, so they weigh nothing — the same call COST makes.
        assert_eq!(summary.effective_tokens, 10_000);
    }

    /// A poisoned row with saturated counters neither panics nor wraps.
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
        assert_eq!(summary.bands[3].calls, 1);
    }

    /// Session-less providers (Cursor) get bands but no reason rows.
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
