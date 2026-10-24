//! Keep network egress here so changes to what leaves the machine can be audited by file.
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tracing::debug;

use super::{SNAPSHOT_FORMAT, Snapshot, litellm, loaded, parse_snapshot_json, replace_loaded};

const MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;

const PRICING_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

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
    let upstream: HashMap<String, Value> = serde_json::from_str(&raw).ok()?;
    let models = litellm::models_from_upstream(&upstream);
    if models.is_empty() {
        return None;
    }

    let fetched = time::OffsetDateTime::now_utc().date().to_string();
    serde_json::to_string_pretty(&serde_json::json!({
        "_source": PRICING_URL,
        "_fetched": fetched,
        "_format": SNAPSHOT_FORMAT,
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
    Fetched(Snapshot),
    Stored(Snapshot),
    Nothing,
}

/// The snapshot to price this run with. One fetched today in the current
/// format is used as is — rates change on the order of weeks, and this keeps
/// the machine quiet for every run after the first each day. Otherwise fetch
/// and keep the result on disk; when the fetch fails, the stored snapshot
/// (however old, in whatever format) still beats no prices at all.
fn refresh(file: Option<&Path>, today: &str, fetch: impl FnOnce() -> Option<String>) -> Refreshed {
    let stored = file
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|raw| parse_snapshot_json(&raw))
        .map(|mut snapshot| {
            // Kept only as the offline fallback, an older snapshot still must
            // not price cache writes at $0.
            if snapshot.format < SNAPSHOT_FORMAT {
                litellm::backfill_write_rates(&mut snapshot.models);
            }
            snapshot
        });
    let fallback = |stored: Option<Snapshot>| stored.map_or(Refreshed::Nothing, Refreshed::Stored);
    if stored.as_ref().is_some_and(|snapshot| {
        snapshot.fetched.as_deref() == Some(today) && snapshot.format == SNAPSHOT_FORMAT
    }) {
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
        format!(
            r#"{{"_fetched":"{fetched}","_format":{SNAPSHOT_FORMAT},"models":{{"m":{{"input":{input},"output":0.0}}}}}}"#
        )
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
        assert!((snapshot.models["m"].base.input - 1.0).abs() < f64::EPSILON);
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
        assert!((snapshot.models["m"].base.input - 2.0).abs() < f64::EPSILON);
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

    /// A snapshot stored by an older build parses fine, so a same-day stamp
    /// alone would keep its rates until tomorrow; offline it is still used,
    /// with missing cache-write rates filled in.
    #[test]
    fn a_snapshot_in_an_older_format_is_refetched_but_kept_offline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pricing.json");
        let older = r#"{"_fetched":"2026-09-16","models":{"m":{"input":1.0,"output":0.0}}}"#;
        fs::write(&file, older).unwrap();
        let fetched = Cell::new(false);
        let refreshed = refresh(Some(&file), "2026-09-16", || {
            fetched.set(true);
            Some(snapshot_json("2026-09-16", 2.0))
        });
        assert!(fetched.get());
        assert!(matches!(refreshed, Refreshed::Fetched(_)));

        fs::write(&file, older).unwrap();
        let Refreshed::Stored(offline) = refresh(Some(&file), "2026-09-16", || None) else {
            panic!("an older snapshot still beats no prices offline");
        };
        let rates = offline.models["m"].base;
        assert!((rates.input - 1.0).abs() < f64::EPSILON);
        assert!((rates.cache_write_5m - 1.0).abs() < f64::EPSILON);
        assert!((rates.cache_write_1h - 1.0).abs() < f64::EPSILON);
    }
}
