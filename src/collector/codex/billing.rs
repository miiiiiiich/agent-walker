//! The service tier a Codex thread's calls are billed at.
use serde_json::Value;

use super::string_path;
use crate::model::ServiceTier;

/// The tier a `thread_settings_applied` row sets; it holds for the thread's
/// calls until the next such row. A row naming another thread is not this
/// thread's setting. Older rollouts write the row without `thread_id`; it
/// counts for the rollout's own thread, since requiring the field would drop
/// every tier those rows set.
pub(super) fn applied_tier(value: &Value, own_thread_id: Option<&str>) -> Option<ServiceTier> {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("thread_settings_applied") {
        return None;
    }
    if let (Some(thread), Some(own)) =
        (string_path(value, &["payload", "thread_id"]), own_thread_id)
        && thread != own
    {
        return None;
    }
    Some(
        match payload
            .pointer("/thread_settings/service_tier")
            .and_then(Value::as_str)
        {
            Some("priority") => ServiceTier::Priority,
            Some("ultrafast") => ServiceTier::Ultrafast,
            _ => ServiceTier::Standard,
        },
    )
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use time::UtcOffset;

    use crate::collector::codex::collect;
    use crate::collector::codex::tests::{meta_line, token_count_line, usage_json, write_lines};
    use crate::model::ServiceTier;

    fn settings_line(ts: &str, thread: &str, tier: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"thread_settings_applied","thread_id":"{thread}","thread_settings":{{"model":"gpt-6-astra","service_tier":"{tier}"}}}}}}"#
        )
    }

    fn unnamed_settings_line(ts: &str, tier: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"thread_settings_applied","thread_settings":{{"model":"gpt-6-astra","service_tier":"{tier}"}}}}}}"#
        )
    }

    /// A fork child's own settings row sits inside the fork window; a row
    /// naming the parent does not set the child's tier, one naming no thread
    /// (the older format) does, and each later row replaces the last.
    #[test]
    fn calls_take_the_tier_their_thread_last_applied() {
        let temp = TempDir::new().expect("test tempdir should be created");
        write_lines(
            &temp.path().join("rollout-c1.jsonl"),
            &[
                meta_line(
                    "2026-11-15T11:08:36Z",
                    "c1",
                    r#"{"subagent":{"thread_spawn":{"parent_thread_id":"p1","depth":1}}}"#,
                    r#","forked_from_id":"p1""#,
                ),
                meta_line("2026-11-15T11:08:36Z", "p1", r#""cli""#, ""),
                settings_line("2026-11-15T11:08:36.005Z", "p1", "ultrafast"),
                settings_line("2026-11-15T11:08:36.010Z", "c1", "priority"),
                token_count_line("2026-11-15T11:08:41Z", &usage_json(100, 0, 10), 110),
                settings_line("2026-11-15T11:08:42Z", "p1", "default"),
                token_count_line("2026-11-15T11:08:43Z", &usage_json(200, 0, 10), 320),
                settings_line("2026-11-15T11:08:44Z", "c1", "default"),
                token_count_line("2026-11-15T11:08:45Z", &usage_json(300, 0, 10), 630),
                settings_line("2026-11-15T11:08:46Z", "c1", "ultrafast"),
                token_count_line("2026-11-15T11:08:47Z", &usage_json(400, 0, 10), 1040),
                settings_line("2026-11-15T11:08:48Z", "c1", "flex"),
                token_count_line("2026-11-15T11:08:49Z", &usage_json(500, 0, 10), 1550),
                unnamed_settings_line("2026-11-15T11:08:50Z", "priority"),
                token_count_line("2026-11-15T11:08:51Z", &usage_json(600, 0, 10), 2160),
            ],
        );

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        let tiers: Vec<(u64, ServiceTier)> = collection
            .usage_events
            .iter()
            .map(|event| (event.usage.input_tokens, event.billing.tier))
            .collect();
        assert_eq!(
            tiers,
            [
                (100, ServiceTier::Priority),
                (200, ServiceTier::Priority),
                (300, ServiceTier::Standard),
                (400, ServiceTier::Ultrafast),
                (500, ServiceTier::Standard),
                (600, ServiceTier::Priority),
            ]
        );
    }
}
