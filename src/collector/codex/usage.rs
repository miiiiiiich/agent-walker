use std::collections::HashSet;

use serde_json::Value;
use time::OffsetDateTime;

use crate::collector::{FileEvents, KeyedRateLimitSample, KeyedUsageEvent};
use crate::model::{RateLimitSample, TokenUsage, UsageEvent};

use super::fork::Origin;
use super::{string_path, u64_field};

/// Per-line attribution shared by the two usage sources.
pub(super) struct UsageContext<'a> {
    pub(super) timestamp: Option<OffsetDateTime>,
    pub(super) session_id: Option<&'a String>,
    pub(super) model: Option<&'a String>,
    pub(super) project: Option<&'a str>,
    pub(super) origin: &'a Origin,
    pub(super) line_index: usize,
}

impl UsageContext<'_> {
    fn event(&self, usage: TokenUsage) -> UsageEvent {
        UsageEvent {
            timestamp: self.timestamp,
            session_id: self.session_id.cloned(),
            model: self.model.cloned().or_else(|| Some("codex".to_owned())),
            source_kind: self.origin.source_kind(),
            attribution_agent: self.origin.agent.clone(),
            attribution_skill: None,
            project: self.project.map(ToOwned::to_owned),
            usage,
            granularity: crate::model::UsageGranularity::Call,
        }
    }
}

/// The usage source wherever present (docs/codex.md). Every record's usage is
/// remembered, even one not counted here, so the `token_count` echoing it is
/// not counted twice.
pub(super) fn collect_usage_record(
    value: &Value,
    context: &UsageContext<'_>,
    own_thread_id: Option<&str>,
    recorded: &mut HashSet<String>,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("token_usage_record") {
        return;
    }
    let Some(usage_value) = value
        .get("payload")
        .and_then(|payload| payload.get("usage"))
        .and_then(fingerprintable)
    else {
        return;
    };
    recorded.insert(usage_fingerprint(usage_value));
    // A record owned by another thread is a copy of that thread's spending.
    if let (Some(owner), Some(own)) = (string_path(value, &["payload", "thread_id"]), own_thread_id)
        && owner != own
    {
        return;
    }
    let Some(usage) = parse_token_usage(usage_value) else {
        return;
    };
    let key = match string_path(value, &["payload", "response_id"]) {
        Some(response_id) => Some(format!("codex-usage:v3:{response_id}")),
        None => context.session_id.and_then(|sid| {
            positional_key("codex-record", sid, context.timestamp, context.line_index)
        }),
    };
    events.usage_events.push(KeyedUsageEvent {
        key,
        event: context.event(usage),
    });
}

pub(super) fn collect_usage_event(
    value: &Value,
    context: &UsageContext<'_>,
    recorded: &HashSet<String>,
    events: &mut FileEvents,
) {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return;
    }
    if string_path(value, &["payload", "type"]).as_deref() != Some("token_count") {
        return;
    }
    let session_id = context.session_id;
    collect_rate_limit_sample(
        value,
        context.timestamp,
        session_id,
        context.line_index,
        events,
    );
    let Some(last_usage) = value
        .get("payload")
        .and_then(|payload| payload.get("info"))
        .and_then(|info| info.get("last_token_usage"))
    else {
        return;
    };
    if recorded.contains(&usage_fingerprint(last_usage)) {
        return;
    }
    let Some(usage) = parse_token_usage(last_usage) else {
        return;
    };
    // Emit last_token_usage deltas; do not also add cumulative total_token_usage.
    // Key by session, last vector, and cumulative vector to collapse replay
    // and unchanged re-emissions (GH-36). Without cumulative usage, positional
    // keys deduplicate whole-file copies only.
    let key = match (session_id, total_usage(value)) {
        (Some(sid), Some(cum)) => Some(format!(
            "codex-usage:v2:{sid}:{last}:{cum}",
            last = usage_fingerprint(last_usage),
            cum = usage_fingerprint(cum),
        )),
        (Some(sid), None) => positional_key("codex", sid, context.timestamp, context.line_index),
        _ => None,
    };
    events.usage_events.push(KeyedUsageEvent {
        key,
        event: context.event(usage),
    });
}

/// Key by co-riding usage state and snapshot fields so fork replay collapses
/// (GH-36), while a moved window survives.
fn collect_rate_limit_sample(
    value: &Value,
    timestamp: Option<OffsetDateTime>,
    session_id: Option<&String>,
    line_index: usize,
    events: &mut FileEvents,
) {
    let Some(timestamp) = timestamp else {
        return;
    };
    let Some(primary) = value
        .get("payload")
        .and_then(|payload| payload.get("rate_limits"))
        .and_then(|limits| limits.get("primary"))
    else {
        return;
    };
    let Some(used_percent) = primary.get("used_percent").and_then(Value::as_f64) else {
        return;
    };
    let last = value
        .get("payload")
        .and_then(|payload| payload.get("info"))
        .and_then(|info| info.get("last_token_usage"))
        .and_then(fingerprintable);
    let key = match (session_id, total_usage(value), last) {
        (Some(sid), Some(cum), Some(last)) => Some(format!(
            "codex-limit:v2:{sid}:{last_fp}:{cum_fp}:{percent}:{window}:{resets}",
            last_fp = usage_fingerprint(last),
            cum_fp = usage_fingerprint(cum),
            percent = used_percent.clamp(0.0, 100.0).to_bits(),
            window = u64_field(primary, "window_minutes"),
            resets = u64_field(primary, "resets_at"),
        )),
        (Some(sid), _, _) => positional_key("codex-limit", sid, Some(timestamp), line_index),
        _ => None,
    };
    events.rate_limit_samples.push(KeyedRateLimitSample {
        key,
        event: RateLimitSample {
            timestamp,
            used_percent: used_percent.clamp(0.0, 100.0),
        },
    });
}

fn total_usage(value: &Value) -> Option<&Value> {
    value
        .get("payload")
        .and_then(|payload| payload.get("info"))
        .and_then(|info| info.get("total_token_usage"))
        .and_then(fingerprintable)
}

/// A usage object valid enough to key on: `total_tokens` parses as an
/// integer. Malformed payloads (`null`, `{}`, a string) would fingerprint as
/// all-zero and falsely collide distinct events — they must take the
/// positional fallback instead.
fn fingerprintable(usage: &Value) -> Option<&Value> {
    usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .is_some()
        .then_some(usage)
}

fn usage_fingerprint(value: &Value) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}",
        u64_field(value, "input_tokens"),
        u64_field(value, "cached_input_tokens"),
        u64_field(value, "cache_write_input_tokens"),
        u64_field(value, "output_tokens"),
        u64_field(value, "reasoning_output_tokens"),
        u64_field(value, "total_tokens"),
    )
}

/// Positional fallback key for events that predate semantic identifiers.
/// Stable for whole-file copies, but NOT for fork replays — those rewrite
/// timestamps and shift line positions, which is why keyed paths prefer
/// content-based keys and reach for this only when the content key can't be
/// built.
pub(super) fn positional_key(
    prefix: &str,
    session_id: &str,
    timestamp: Option<OffsetDateTime>,
    line_index: usize,
) -> Option<String> {
    timestamp.map(|ts| {
        format!(
            "{prefix}:{session_id}:{ts}:{line_index}",
            ts = ts.unix_timestamp_nanos(),
        )
    })
}

fn parse_token_usage(value: &Value) -> Option<TokenUsage> {
    let input_tokens = u64_field(value, "input_tokens");
    let cached_input_tokens = u64_field(value, "cached_input_tokens");
    let output_tokens = u64_field(value, "output_tokens");
    let reasoning_output_tokens = u64_field(value, "reasoning_output_tokens");
    // A `token_count` with only `total_tokens` (the post-compaction refill) is
    // no API call; counting it adds phantom calls.
    if input_tokens == 0 && output_tokens == 0 {
        return None;
    }

    // input_tokens includes cached and cache-write input; subtract both so it
    // means fresh input, matching the Claude schema.
    let uncached = input_tokens.saturating_sub(cached_input_tokens);
    let cache_write = u64_field(value, "cache_write_input_tokens").min(uncached);
    Some(TokenUsage {
        input_tokens: uncached - cache_write,
        output_tokens,
        reasoning_output_tokens,
        cache_creation_input_tokens: cache_write,
        cache_read_input_tokens: cached_input_tokens,
        ..TokenUsage::default()
    })
}

pub(super) fn session_model(value: &Value) -> Option<String> {
    string_path(value, &["payload", "model"])
        .or_else(|| {
            string_path(
                value,
                &["payload", "collaboration_mode", "settings", "model"],
            )
        })
        .or_else(|| string_path(value, &["payload", "model_provider"]))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use time::UtcOffset;

    use crate::collector::codex::collect;
    use crate::collector::codex::tests::{
        meta_line, record_line, token_count_line, total_volume, usage_json, write_lines,
    };

    #[test]
    fn deduplicates_copies_but_keeps_distinct_turns() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        let lines = concat!(
            r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}"#,
            "\n",
            r#"{"timestamp":"2026-06-01T00:00:09Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10,"total_tokens":110}}}}"#,
            "\n"
        );
        fs::write(day.join("rollout-original.jsonl"), lines).expect("fixture should be written");
        fs::write(day.join("rollout-copy.jsonl"), lines).expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 2);
        assert_eq!(collection.usage_events.len(), 2);
        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 220);
    }

    /// A re-emitted `token_count` whose cumulative did not advance reports no
    /// new consumption — summing it again would overcount, so it collapses.
    #[test]
    fn re_emitted_token_count_with_unchanged_cumulative_counts_once() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.usage_events[0].usage.token_volume(), 110);
    }

    #[test]
    fn same_last_usage_with_advanced_cumulative_both_survive() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:09Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":200,"cached_input_tokens":0,"output_tokens":20,"total_tokens":220}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        let total: u64 = collection
            .usage_events
            .iter()
            .map(|event| event.usage.token_volume())
            .sum();
        assert_eq!(total, 220);
    }

    /// A malformed cumulative (`{}` / `null`) must not enter the semantic-key
    /// path — it would fingerprint as all-zero and falsely collide distinct
    /// turns. Both events fall back to the positional key and survive.
    #[test]
    fn malformed_cumulative_falls_back_to_positional_key() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:09Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":null}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
    }

    #[test]
    fn rate_limit_renotification_with_moved_window_survives() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}},"rate_limits":{"primary":{"used_percent":37.5,"window_minutes":300,"resets_at":1750000000}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}},"rate_limits":{"primary":{"used_percent":37.5,"window_minutes":300,"resets_at":1750018000}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.rate_limit_samples.len(), 2);
    }

    /// Re-emission across a turn boundary (`turn_context` advances between the
    /// copies) still collapses — the reason `turn_id` is deliberately NOT part
    /// of the usage key.
    #[test]
    fn re_emission_across_turn_boundary_still_collapses() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"high","approval_policy":"on-request","turn_id":"t1"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:04Z","type":"turn_context","payload":{"model":"gpt-5.5","effort":"high","turn_id":"t2"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.effort_events.len(), 2);
    }

    #[test]
    fn cache_write_difference_yields_distinct_keys() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(
            day.join("rollout-session.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-01T00:00:00Z","type":"session_meta","payload":{"id":"s1","model_provider":"openai"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"cache_write_input_tokens":5,"output_tokens":10,"total_tokens":110},"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n"
            ),
        )
        .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
    }

    /// Each record counts once; its `token_count` echoes (one repeated
    /// byte-identically), the embedded copy in `compacted` and the total-only
    /// refill row add nothing.
    #[test]
    fn usage_records_are_primary_and_their_token_count_echoes_count_once() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let first = usage_json(100, 40, 10);
        let compaction = usage_json(5_000, 0, 300);
        let third = usage_json(800, 700, 30);
        let echo = token_count_line("2026-06-01T00:00:03.100Z", &first, 110);
        write_lines(
            &temp.path().join("rollout-s1.jsonl"),
            &[
                meta_line("2026-06-01T00:00:00Z", "s1", r#""cli""#, ""),
                record_line("2026-06-01T00:00:03Z", "s1", "resp_1", &first),
                echo.clone(),
                echo.replace("00:00:03.100Z", "00:00:04.000Z"),
                record_line("2026-06-01T00:00:20Z", "s1", "resp_2", &compaction),
                format!(
                    r#"{{"timestamp":"2026-06-01T00:00:21Z","type":"compacted","payload":{{"message":"","compaction_response_id":"resp_2","latest_token_usage_record":{{"thread_id":"s1","response_id":"resp_2","usage":{compaction}}}}}}}"#
                ),
                r#"{"timestamp":"2026-06-01T00:00:22Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":0,"output_tokens":0,"total_tokens":258400},"last_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0,"total_tokens":27000},"model_context_window":258400}}}"#.to_owned(),
                record_line("2026-06-01T00:00:30Z", "s1", "resp_3", &third),
                token_count_line("2026-06-01T00:00:30.100Z", &third, 5_940),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 3);
        assert_eq!(total_volume(&collection), 110 + 5_300 + 830);
    }

    /// A `token_count` with no matching record still counts beside the
    /// records that follow it.
    #[test]
    fn rollout_resumed_into_usage_records_counts_both_segments_once() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let early = usage_json(100, 0, 10);
        let late = usage_json(200, 100, 20);
        write_lines(
            &temp.path().join("rollout-s1.jsonl"),
            &[
                meta_line("2026-06-01T00:00:00Z", "s1", r#""cli""#, ""),
                token_count_line("2026-06-01T00:00:03Z", &early, 110),
                record_line("2026-06-02T00:00:03Z", "s1", "resp_1", &late),
                token_count_line("2026-06-02T00:00:03.100Z", &late, 330),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        assert_eq!(total_volume(&collection), 110 + 220);
    }

    #[test]
    fn usage_record_of_another_thread_and_its_echo_are_not_counted() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let copied = usage_json(400, 0, 40);
        let own = usage_json(100, 0, 10);
        write_lines(
            &temp.path().join("rollout-c1.jsonl"),
            &[
                meta_line("2026-06-01T00:00:00Z", "c1", r#""cli""#, ""),
                record_line("2026-06-01T00:00:03Z", "p1", "resp_p", &copied),
                token_count_line("2026-06-01T00:00:03.100Z", &copied, 440),
                record_line("2026-06-01T00:00:09Z", "c1", "resp_c", &own),
                token_count_line("2026-06-01T00:00:09.100Z", &own, 550),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(total_volume(&collection), 110);
    }

    #[test]
    fn total_only_token_count_is_not_a_call() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_lines(
            &temp.path().join("rollout-s1.jsonl"),
            &[
                meta_line("2026-06-01T00:00:00Z", "s1", r#""cli""#, ""),
                token_count_line("2026-06-01T00:00:03Z", &usage_json(100, 0, 10), 110),
                r#"{"timestamp":"2026-06-01T00:00:04Z","type":"compacted","payload":{"message":""}}"#.to_owned(),
                r#"{"timestamp":"2026-06-01T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":258400},"last_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0,"total_tokens":27000}}}}"#.to_owned(),
                token_count_line("2026-06-01T00:00:09Z", &usage_json(0, 0, 12), 122),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.usage_events.len(), 2);
        assert_eq!(total_volume(&collection), 122);
    }

    #[test]
    fn cache_write_input_is_cache_creation_inside_input() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_lines(
            &temp.path().join("rollout-s1.jsonl"),
            &[
                meta_line("2026-06-01T00:00:00Z", "s1", r#""cli""#, ""),
                record_line(
                    "2026-06-01T00:00:03Z",
                    "s1",
                    "resp_1",
                    r#"{"input_tokens":100,"cached_input_tokens":40,"cache_write_input_tokens":30,"output_tokens":10,"reasoning_output_tokens":4,"total_tokens":110}"#,
                ),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let usage = &collection.usage_events[0].usage;
        assert_eq!(usage.input_tokens, 30);
        assert_eq!(usage.cache_read_input_tokens, 40);
        assert_eq!(usage.cache_creation_input_tokens, 30);
        assert_eq!(usage.output_tokens, 10);
        assert_eq!(usage.reasoning_output_tokens, 4);
        assert_eq!(usage.token_volume(), 110);
    }
}
