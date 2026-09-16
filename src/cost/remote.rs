//! The ONLY network egress in the cost pipeline (and, besides the auto-detected,
//! opt-out Cursor collector, in the whole binary): fetching LiteLLM's community
//! pricing table. Anything that changes what leaves the machine or where it
//! goes lives in this file — a diff touching `cost/remote.rs` is an egress
//! change by definition. Only pricing metadata is fetched; no usage data is
//! ever sent.
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::debug;

use super::{Pricing, Snapshot, loaded, parse_snapshot_json, replace_loaded};

/// Decoded-body cap; the table is a few MB.
const MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;

const PRICING_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// Fetch the upstream `LiteLLM` database and reduce it to the snapshot
/// format: bare model ids (any provider) with per-token costs. No provider
/// allowlist — a model is priced if its id is in the table; unknown ids remain
/// unpriced. Provider/region duplicates and absurd rates are the actual guards.
fn fetch_snapshot_json() -> Option<String> {
    // No env proxy: ureq 3 would pick up `HTTPS_PROXY` & co. by default,
    // which would silently change where this request leaves the machine.
    let mut response = ureq::get(PRICING_URL)
        .config()
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .call()
        .ok()?;
    // Cap the *decoded* body: ureq's own limit counts compressed bytes, and
    // the table arrives gzipped.
    let mut raw = String::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_string(&mut raw)
        .ok()?;
    if raw.len() as u64 > MAX_BODY_BYTES {
        return None;
    }
    let upstream: HashMap<String, serde_json::Value> = serde_json::from_str(&raw).ok()?;

    let mut models = HashMap::new();
    for (key, entry) in &upstream {
        // Provider/region variants are dropped in favor of bare model ids —
        // EXCEPT `xai/`: LiteLLM registers Grok models only under the
        // provider prefix (`xai/grok-4.5`, no bare key), so the prefix is
        // stripped so those models can resolve to a price.
        let key = key.strip_prefix("xai/").unwrap_or(key);
        if key.contains('/')
            || key.starts_with("anthropic.")
            || key.starts_with("global.")
            || key.starts_with("us.")
            || key.starts_with("eu.")
            || key.starts_with("au.")
            || key.starts_with("apac.")
        {
            continue; // provider/region variants; keep bare model ids only
        }
        // Accept only chat, completion, and responses model modes.
        if !matches!(
            entry.get("mode").and_then(serde_json::Value::as_str),
            Some("chat" | "completion" | "responses")
        ) {
            continue;
        }
        // Reject non-finite, negative, or absurd rates (> $1/token) so a bad
        // upstream entry cannot poison cached cost estimates.
        let cost = |field: &str| {
            entry
                .get(field)
                .and_then(serde_json::Value::as_f64)
                .filter(|value| value.is_finite() && (0.0..1.0).contains(value))
        };
        let Some(input) = cost("input_cost_per_token") else {
            continue;
        };
        models.insert(
            key.to_owned(),
            Pricing {
                input,
                output: cost("output_cost_per_token").unwrap_or(0.0),
                cache_read: cost("cache_read_input_token_cost").unwrap_or(0.0),
                cache_write_5m: cost("cache_creation_input_token_cost").unwrap_or(0.0),
                cache_write_1h: cost("cache_creation_input_token_cost_above_1hr").unwrap_or(0.0),
            },
        );
    }
    if models.is_empty() {
        return None;
    }

    let fetched = time::OffsetDateTime::now_utc().date().to_string();
    serde_json::to_string_pretty(&serde_json::json!({
        "_source": PRICING_URL,
        "_fetched": fetched,
        "models": models,
    }))
    .ok()
}

fn pricing_file() -> Option<PathBuf> {
    Some(crate::paths::cache_dir().ok()?.join("pricing.json"))
}

fn store_snapshot(path: &Path, serialized: &str) {
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    if let Err(error) = fs::write(&temp, serialized).and_then(|()| fs::rename(&temp, path)) {
        debug!(path = %path.display(), %error, "pricing snapshot not stored; next run fetches again");
    }
}

enum Refreshed {
    /// Came off the network this call.
    Fetched(Snapshot),
    /// Read from disk: either fetched earlier today, or the fallback after
    /// a failed fetch.
    Stored(Snapshot),
    Nothing,
}

/// The snapshot to price this run with. One fetched today is used as is —
/// rates change on the order of weeks, and this keeps the machine quiet for
/// every run after the first each day. Otherwise fetch and keep the result
/// on disk; when the fetch fails, the stored snapshot (however old) still
/// beats no prices at all.
fn refresh(file: Option<&Path>, today: &str, fetch: impl FnOnce() -> Option<String>) -> Refreshed {
    let stored = file
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|raw| parse_snapshot_json(&raw));
    let fallback = |stored: Option<Snapshot>| stored.map_or(Refreshed::Nothing, Refreshed::Stored);
    if stored
        .as_ref()
        .is_some_and(|snapshot| snapshot.fetched.as_deref() == Some(today))
    {
        return fallback(stored);
    }
    let Some(serialized) = fetch() else {
        debug!("pricing fetch failed; using the stored snapshot if any");
        return fallback(stored);
    };
    let Some(snapshot) = parse_snapshot_json(&serialized) else {
        debug!("fetched pricing did not parse; using the stored snapshot if any");
        return fallback(stored);
    };
    if let Some(path) = file {
        store_snapshot(path, &serialized);
    }
    Refreshed::Fetched(snapshot)
}

/// Refresh active pricing from `LiteLLM`. A snapshot off the network always
/// wins; one read from disk only fills an empty table, so a reload that
/// falls back to disk never downgrades prices already in memory. Nothing
/// usable leaves the last good snapshot in place.
pub(super) fn refresh_pricing() {
    let file = pricing_file();
    let today = time::OffsetDateTime::now_utc().date().to_string();
    match refresh(file.as_deref(), &today, fetch_snapshot_json) {
        Refreshed::Fetched(snapshot) => replace_loaded(Some(snapshot)),
        Refreshed::Stored(snapshot) => {
            if loaded().read().is_ok_and(|current| current.is_none()) {
                replace_loaded(Some(snapshot));
            }
        }
        Refreshed::Nothing => {}
    }
}

pub fn spawn_pricing_refresh() -> std::thread::JoinHandle<()> {
    std::thread::spawn(refresh_pricing)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn snapshot_json(fetched: &str, input: f64) -> String {
        format!(r#"{{"_fetched":"{fetched}","models":{{"m":{{"input":{input},"output":0.0}}}}}}"#)
    }

    #[test]
    fn a_snapshot_fetched_today_is_used_without_a_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pricing.json");
        fs::write(&file, snapshot_json("2026-09-16", 1.0)).unwrap();
        let fetched = Cell::new(false);
        let Refreshed::Stored(snapshot) = refresh(Some(&file), "2026-09-16", || {
            fetched.set(true);
            None
        }) else {
            panic!("expected the stored snapshot");
        };
        assert!(!fetched.get());
        assert!((snapshot.models["m"].input - 1.0).abs() < f64::EPSILON);
    }

    /// A file that does not parse, or has no models, counts as absent even
    /// when stamped today: refetch rather than price nothing.
    #[test]
    fn an_unusable_snapshot_stamped_today_is_refetched() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pricing.json");
        for broken in [r#"{"_fetched":"2026-09-16","models":{}}"#, "not json"] {
            fs::write(&file, broken).unwrap();
            let fetched = Cell::new(false);
            let refreshed = refresh(Some(&file), "2026-09-16", || {
                fetched.set(true);
                Some(snapshot_json("2026-09-16", 3.0))
            });
            assert!(fetched.get(), "{broken}");
            assert!(matches!(refreshed, Refreshed::Fetched(_)), "{broken}");
        }
    }

    #[test]
    fn a_stale_snapshot_is_refetched_and_stored() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pricing.json");
        fs::write(&file, snapshot_json("2026-09-15", 1.0)).unwrap();
        let Refreshed::Fetched(snapshot) = refresh(Some(&file), "2026-09-16", || {
            Some(snapshot_json("2026-09-16", 2.0))
        }) else {
            panic!("expected a fetched snapshot");
        };
        assert_eq!(snapshot.fetched.as_deref(), Some("2026-09-16"));
        assert!((snapshot.models["m"].input - 2.0).abs() < f64::EPSILON);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            snapshot_json("2026-09-16", 2.0)
        );
    }

    #[test]
    fn a_failed_fetch_falls_back_to_the_stored_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pricing.json");
        fs::write(&file, snapshot_json("2026-09-15", 1.0)).unwrap();
        let Refreshed::Stored(snapshot) = refresh(Some(&file), "2026-09-16", || None) else {
            panic!("expected the stale stored snapshot");
        };
        assert_eq!(snapshot.fetched.as_deref(), Some("2026-09-15"));
        assert!(matches!(
            refresh(Some(&dir.path().join("none.json")), "2026-09-16", || None),
            Refreshed::Nothing
        ));
    }
}
