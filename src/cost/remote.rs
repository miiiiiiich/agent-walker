//! Keep network egress here so changes to what leaves the machine can be audited by file.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::{debug, warn};

use super::{SNAPSHOT_FORMAT, Snapshot, litellm, loaded, parse_snapshot_json, replace_loaded};
use crate::owned_file;

/// Decoded bytes, with room for the table to grow: past the cap pricing
/// freezes on the stored snapshot. What is stored is a filtered subset of a
/// fetched table, so the same cap bounds reading it back.
const MAX_TABLE_BYTES: u64 = 32 * 1024 * 1024;

const PRICING_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        // No env proxy: ureq 3 would pick up `HTTPS_PROXY` & co. by default,
        // which would silently change where this request leaves the machine.
        .proxy(None)
        // Checked on every hop: a redirect to plain http would let anyone on
        // the path rewrite the rates.
        .https_only(true)
        .max_redirects(3)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .new_agent()
}

enum FetchError {
    /// No answer, which is ordinary offline; the COST panel's date already
    /// shows how old the stored table is.
    Unreachable(String),
    /// An answer that was refused. Unlike being offline this does not clear
    /// up on its own, so the stored table would freeze unseen.
    Refused(String),
}

impl From<ureq::Error> for FetchError {
    fn from(error: ureq::Error) -> Self {
        match error {
            ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::HostNotFound
            | ureq::Error::ConnectionFailed => Self::Unreachable(error.to_string()),
            _ => Self::Refused(error.to_string()),
        }
    }
}

fn fetch_snapshot_json() -> Option<String> {
    match fetch_from(&agent(), PRICING_URL) {
        Ok(serialized) => Some(serialized),
        Err(FetchError::Unreachable(error)) => {
            debug!(%error, "pricing fetch failed; using the stored snapshot if any");
            None
        }
        Err(FetchError::Refused(reason)) => {
            warn!(%reason, "pricing table refused; using the stored snapshot if any");
            None
        }
    }
}

fn fetch_from(agent: &ureq::Agent, url: &str) -> Result<String, FetchError> {
    let mut response = agent.get(url).call()?;
    // Cap the *decoded* body: ureq's own limit counts compressed bytes, and
    // the table arrives gzipped.
    let mut raw = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_TABLE_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|error| FetchError::Unreachable(error.to_string()))?;
    if raw.len() as u64 > MAX_TABLE_BYTES {
        return Err(FetchError::Refused(format!(
            "decoded body over {MAX_TABLE_BYTES} bytes"
        )));
    }
    let models =
        litellm::models_from_table(&raw).map_err(|error| FetchError::Refused(error.to_string()))?;
    if models.is_empty() {
        return Err(FetchError::Refused("no model rate in bounds".to_owned()));
    }

    let fetched = time::OffsetDateTime::now_utc().date().to_string();
    serde_json::to_string_pretty(&serde_json::json!({
        "_source": PRICING_URL,
        "_fetched": fetched,
        "_format": SNAPSHOT_FORMAT,
        "models": models,
    }))
    .map_err(|error| FetchError::Refused(error.to_string()))
}

fn pricing_file() -> Option<PathBuf> {
    Some(crate::paths::cache_dir().ok()?.join("pricing.json"))
}

/// A FIFO planted at the path would block `open`, and startup joins this
/// thread without a timeout.
fn read_stored(path: &Path) -> Option<String> {
    let bytes = owned_file::read_regular(path, MAX_TABLE_BYTES).ok()?;
    String::from_utf8(bytes).ok()
}

/// The cache dir holds what the next run trusts, so only the owner may
/// write it.
fn store_snapshot(path: &Path, serialized: &str) {
    let Some(parent) = path.parent() else {
        return;
    };
    let stored = owned_file::create_private_dir(parent).and_then(|()| {
        owned_file::write_replacing(path, serialized.as_bytes(), owned_file::PRIVATE_MODE)
    });
    if let Err(error) = stored {
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
        .and_then(read_stored)
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
        return fallback(stored);
    };
    let Some(snapshot) = parse_snapshot_json(&serialized) else {
        warn!("fetched pricing did not parse; using the stored snapshot if any");
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
    use std::{fs, io};

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
        fs::write(&file, snapshot_json("2026-09-16", 1e-6)).unwrap();
        let fetched = Cell::new(false);
        let Refreshed::Stored(snapshot) = refresh(Some(&file), "2026-09-16", || {
            fetched.set(true);
            None
        }) else {
            panic!("expected the stored snapshot");
        };
        assert!(!fetched.get());
        assert!((snapshot.models["m"].base.input - 1e-6).abs() < f64::EPSILON);
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
                Some(snapshot_json("2026-09-16", 3e-6))
            });
            assert!(fetched.get(), "{broken}");
            assert!(matches!(refreshed, Refreshed::Fetched(_)), "{broken}");
        }
    }

    #[test]
    fn a_stale_snapshot_is_refetched_and_stored() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pricing.json");
        fs::write(&file, snapshot_json("2026-09-15", 1e-6)).unwrap();
        let Refreshed::Fetched(snapshot) = refresh(Some(&file), "2026-09-16", || {
            Some(snapshot_json("2026-09-16", 2e-6))
        }) else {
            panic!("expected a fetched snapshot");
        };
        assert_eq!(snapshot.fetched.as_deref(), Some("2026-09-16"));
        assert!((snapshot.models["m"].base.input - 2e-6).abs() < f64::EPSILON);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            snapshot_json("2026-09-16", 2e-6)
        );
    }

    #[test]
    fn a_failed_fetch_falls_back_to_the_stored_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pricing.json");
        fs::write(&file, snapshot_json("2026-09-15", 1e-6)).unwrap();
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
        let older = r#"{"_fetched":"2026-09-16","models":{"m":{"input":1e-6,"output":0.0}}}"#;
        fs::write(&file, older).unwrap();
        let fetched = Cell::new(false);
        let refreshed = refresh(Some(&file), "2026-09-16", || {
            fetched.set(true);
            Some(snapshot_json("2026-09-16", 2e-6))
        });
        assert!(fetched.get());
        assert!(matches!(refreshed, Refreshed::Fetched(_)));

        fs::write(&file, older).unwrap();
        let Refreshed::Stored(offline) = refresh(Some(&file), "2026-09-16", || None) else {
            panic!("an older snapshot still beats no prices offline");
        };
        let rates = offline.models["m"].base;
        assert!((rates.input - 1e-6).abs() < f64::EPSILON);
        assert!((rates.cache_write_5m - 1e-6).abs() < f64::EPSILON);
        assert!((rates.cache_write_1h - 1e-6).abs() < f64::EPSILON);
    }
    /// `https_only` applies to every hop, redirects included, so a plain
    /// http URL standing in for a redirect target is refused before any
    /// connection. A local https redirect would need a certificate the
    /// client trusts, so the redirect cap is checked on the config.
    #[test]
    fn the_fetch_refuses_plain_http_without_connecting() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/pricing.json", listener.local_addr().unwrap());
        let agent = agent();
        assert!(matches!(
            fetch_from(&agent, &url),
            Err(FetchError::Refused(reason)) if reason.contains("https only")
        ));
        assert_eq!(
            listener.accept().err().map(|error| error.kind()),
            Some(io::ErrorKind::WouldBlock)
        );
        let config = agent.config();
        assert!(config.https_only());
        assert_eq!(config.max_redirects(), 3);
        assert!(config.proxy().is_none());
    }

    /// A stored snapshot stamped today is trusted without a fetch, so its
    /// rates must clear the same bounds; nothing left in bounds refetches.
    #[test]
    fn a_stored_snapshot_out_of_bounds_is_refetched() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pricing.json");
        fs::write(&file, snapshot_json("2026-10-26", 0.5)).unwrap();
        let fetched = Cell::new(false);
        let refreshed = refresh(Some(&file), "2026-10-26", || {
            fetched.set(true);
            Some(snapshot_json("2026-10-26", 2e-6))
        });
        assert!(fetched.get());
        let Refreshed::Fetched(snapshot) = refreshed else {
            panic!("expected a fetched snapshot");
        };
        assert!((snapshot.models["m"].base.input - 2e-6).abs() < f64::EPSILON);
    }

    #[test]
    fn a_fetched_snapshot_keeps_only_rates_in_bounds() {
        let fetched = format!(
            r#"{{"_fetched":"2026-10-26","_format":{SNAPSHOT_FORMAT},"models":{{
                "m":{{"input":2e-6,"output":0.0}},
                "poisoned":{{"input":1e-6,"output":-1.0}}
            }}}}"#
        );
        let Refreshed::Fetched(snapshot) = refresh(None, "2026-10-26", || Some(fetched)) else {
            panic!("expected a fetched snapshot");
        };
        assert!(snapshot.models.contains_key("m"));
        assert!(!snapshot.models.contains_key("poisoned"));
    }

    /// The stamp is printed in the COST panel; one that is not a date is
    /// dropped, and with it the same-day trust.
    #[test]
    fn a_stamp_that_is_not_a_date_is_dropped_and_refetched() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pricing.json");
        // JSON escapes: the parsed stamp carries the control characters.
        let stamp = r"2026-10-26\u001b]0;title\u0007";
        fs::write(&file, snapshot_json(stamp, 1e-6)).unwrap();
        let Refreshed::Stored(offline) = refresh(Some(&file), "2026-10-26", || None) else {
            panic!("the snapshot itself is still usable");
        };
        assert_eq!(offline.fetched, None);
        let fetched = Cell::new(false);
        let _ = refresh(Some(&file), "2026-10-26", || {
            fetched.set(true);
            None
        });
        assert!(fetched.get());
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_or_device_at_the_cache_path_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pricing.json");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        assert!(made.success());
        let (sender, receiver) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let refreshed = refresh(Some(&path), "2026-10-26", || None);
            let _ = sender.send(matches!(refreshed, Refreshed::Nothing));
        });
        let outcome = receiver.recv_timeout(Duration::from_secs(5));
        if outcome.is_err() {
            // Release the reader blocked in `open` so the thread ends.
            drop(fs::OpenOptions::new().write(true).open(&fifo));
        }
        assert_eq!(outcome, Ok(true));

        let device = dir.path().join("device.json");
        std::os::unix::fs::symlink("/dev/zero", &device).unwrap();
        assert!(matches!(
            refresh(Some(&device), "2026-10-26", || None),
            Refreshed::Nothing
        ));
    }

    #[cfg(unix)]
    #[test]
    fn the_snapshot_is_stored_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache").join("agent-walker");
        let file = cache.join("pricing.json");
        store_snapshot(&file, "{}");
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&cache), 0o700);
        assert_eq!(mode(&file), 0o600);
        assert_eq!(
            fs::read_dir(&cache).unwrap().count(),
            1,
            "no temp file left"
        );
    }

    #[cfg(unix)]
    #[test]
    fn storing_never_writes_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("agent-walker");
        fs::create_dir(&cache).unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "original").unwrap();
        let file = cache.join("pricing.json");
        // A link at a temp name derived from the pid, and one at the target
        // itself: neither is followed.
        let predictable = file.with_extension(format!("tmp{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &predictable).unwrap();
        std::os::unix::fs::symlink(&victim, &file).unwrap();
        store_snapshot(&file, "stored");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "original");
        assert!(fs::symlink_metadata(&file).unwrap().is_file());
        assert_eq!(fs::read_to_string(&file).unwrap(), "stored");
    }
}
