//! The per-file parse cache: (mtime, size)-keyed, versioned, local-offset
//! aware. Parsing semantics changes MUST bump `CACHE_VERSION` (see its doc).
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use time::UtcOffset;
use tracing::debug;

use super::events::FileEvents;

/// Bump whenever the serialized layout OR the parsing semantics (event
/// extraction, dedup keys) of a cached `FileEvents` change — stale caches
/// would otherwise deserialize into garbage, or replay outdated keys that
/// defeat a dedup fix.
/// - 7: session-touch compression moved from UTC to local-day bucketing (cached
///   touches depend on the local offset; a cache is invalidated by EITHER a
///   version bump OR a changed `local_offset`, recorded in
///   `CacheFile::offset_seconds`, so a machine-TZ change rebuilds automatically).
/// - 8: `UsageEvent` gained `reported_cost_usd`, changing its bincode layout.
/// - 9: v0.9 events — `UsageEvent.attribution_skill`, plus rate-limit /
///   effort / mode event lists on `FileEvents`.
/// - 10: Codex dedup keys became content-based (fork-replay dedup, GH-36) —
///   same layout, but cached events carry the old positional keys.
/// - 11: Claude `usage.iterations` parsing (fallback/advisor calls) — cached
///   `FileEvents` lack the iteration events.
/// - 12: `FileEvents` gained `credit_samples` (Copilot CREDITS), changing its
///   bincode layout.
/// - 13: duration events became keyed (`KeyedDurationEvent`, Grok fork
///   dedup), changing the `FileEvents` layout.
/// - 14: Claude top-level `effort` extraction — v13 caches deserialize fine
///   but carry empty effort events for already-parsed sessions.
/// - 15: `FileEvents` gained `permission_events` (autonomy mix), changing
///   its bincode layout.
/// - 16: `FileEvents` gained `interrupt_events` (esc / `turn_aborted` counts),
///   changing its bincode layout.
/// - 17: interrupt admission tightened (exact Claude marker forms as the
///   sole content block, subagent file provenance, Codex
///   `reason == "interrupted"` + required `turn_id`) — v16 caches carry
///   over-counted interrupt events.
/// - 18: `DurationEvent` gained `human_wait_ms` (Claude `AskUserQuestion`
///   answer time inside a turn) and `FileEvents` gained `pace_events` (the
///   gap before each prompt), changing the bincode layout; Claude turns are
///   now stamped at their end and keyed by prompt uuid for fork dedup.
/// - 19: `DurationEvent` gained `model_ms` (the model's own share of a
///   turn vs tool runs), changing its bincode layout.
///
/// The per-file key remains (mtime, size); `--no-cache` is never required.
const CACHE_VERSION: u32 = 19;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileStamp {
    mtime_ns: u128,
    size: u64,
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = fs::metadata(path).ok()?;
    let mtime_ns = metadata
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(FileStamp {
        mtime_ns,
        size: metadata.len(),
    })
}

#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    version: u32,
    /// Local UTC offset (seconds) the cached events were compressed under.
    /// `compress_touches` buckets touches by local day, so a cache built in a
    /// different timezone would silently misplace boundary-day touches; a
    /// mismatch here invalidates the whole cache, same as a version bump.
    #[serde(default)]
    offset_seconds: i32,
    entries: HashMap<PathBuf, CacheEntry>,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    mtime_ns: u128,
    size: u64,
    events: FileEvents,
}

/// One file per provider, `<name>.bin`, versioned by the header inside it:
/// a bump overwrites the old file on the next store instead of leaving a
/// sibling behind.
fn cache_file(dir: &Path, cache_name: &str) -> PathBuf {
    dir.join(format!("{cache_name}.bin"))
}

/// Before 0.17 the version sat in the file name (`claude-v19.bin`) and no
/// bump ever removed the previous one, so long-lived installs carried one
/// full cache per bump. Sweep those on startup.
fn remove_legacy_caches(dir: &Path, cache_name: &str) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{cache_name}-v");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let legacy = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".bin"))
            .is_some_and(|version| {
                !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit())
            });
        if legacy {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// A cached file is reusable only when both the format version and the
/// local-offset it was compressed under match the current run; a mismatch in
/// either means the compressed touches could be misplaced, so it is discarded.
fn cache_is_reusable(cache: &CacheFile, offset_seconds: i32) -> bool {
    cache.version == CACHE_VERSION && cache.offset_seconds == offset_seconds
}

/// `None` when nothing reusable is on disk (missing, corrupt, or built under
/// another version / offset), so the caller knows it must write.
fn load_cache(path: &Path, offset_seconds: i32) -> Option<CacheFile> {
    let bytes = fs::read(path).ok()?;
    match bincode::deserialize::<CacheFile>(&bytes) {
        Ok(cache) if cache_is_reusable(&cache, offset_seconds) => Some(cache),
        _ => {
            debug!(path = %path.display(), "discarding stale, corrupt, or offset-changed cache");
            None
        }
    }
}

/// The cache holds project paths and session ids derived from the logs, so
/// it is written owner-only where the platform can express that.
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)?;
    // `mode` only applies on create; a leftover temp keeps its old bits.
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::write(path, bytes)
}

fn store_cache(path: &Path, cache: &CacheFile) {
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = bincode::serialize(cache) else {
        return;
    };
    // Per-process name: two concurrent runs must never share a temp inode.
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    if write_private(&temp, &bytes).is_ok() {
        // `std::fs::rename` is atomic on Unix and uses
        // `MoveFileExW + MOVEFILE_REPLACE_EXISTING` on Windows, so the
        // destination is overwritten on both platforms without an explicit
        // unlink. Removing the file first would break the Unix atomicity
        // guarantee and momentarily leave the cache missing for concurrent
        // readers.
        let _ = fs::rename(&temp, path);
    }
}

/// Parse `files` through `parse`, reusing cached per-file results when the
/// file is byte-identical to the last run ((mtime, size) match) AND the cache
/// was built under the same `local_offset` (compressed touches are
/// offset-dependent). Cache misses are parsed in parallel; results are returned
/// in `files` order so that downstream deduplication stays deterministic.
/// `cache_name: None` disables the on-disk cache (tests, ad-hoc directories).
pub fn parse_files_cached(
    cache_name: Option<&str>,
    files: &[PathBuf],
    local_offset: UtcOffset,
    parse: impl Fn(&Path) -> Option<FileEvents> + Sync,
) -> Vec<(PathBuf, Option<FileEvents>)> {
    let cache_file = cache_name.and_then(|name| {
        let dir = crate::paths::cache_dir().ok()?;
        remove_legacy_caches(&dir, name);
        Some(cache_file(&dir, name))
    });
    parse_files_with_cache(cache_file.as_deref(), files, local_offset, parse)
}

fn parse_files_with_cache(
    cache_file: Option<&Path>,
    files: &[PathBuf],
    local_offset: UtcOffset,
    parse: impl Fn(&Path) -> Option<FileEvents> + Sync,
) -> Vec<(PathBuf, Option<FileEvents>)> {
    let offset_seconds = local_offset.whole_seconds();
    let loaded = cache_file.and_then(|path| load_cache(path, offset_seconds));
    let reusable = loaded.is_some();
    let cache = loaded.unwrap_or_default();

    // (path, events, stamp, served from cache)
    let parsed: Vec<(PathBuf, Option<FileEvents>, Option<FileStamp>, bool)> = files
        .par_iter()
        .map(|path| {
            let stamp = file_stamp(path);
            if let Some(stamp) = stamp
                && let Some(entry) = cache.entries.get(path)
                && entry.mtime_ns == stamp.mtime_ns
                && entry.size == stamp.size
            {
                return (path.clone(), Some(entry.events.clone()), Some(stamp), true);
            }
            (path.clone(), parse(path), stamp, false)
        })
        .collect();

    // Every storable file came from the cache and nothing was pruned: the
    // file on disk already says exactly this, so skip the rebuild and write.
    let storable = parsed
        .iter()
        .filter(|(_, events, stamp, _)| events.is_some() && stamp.is_some())
        .count();
    let hits = parsed.iter().filter(|(_, _, _, hit)| *hit).count();
    let unchanged = reusable && hits == storable && storable == cache.entries.len();

    let Some(cache_file) = cache_file.filter(|_| !unchanged) else {
        return parsed
            .into_iter()
            .map(|(path, events, _, _)| (path, events))
            .collect();
    };

    let mut next = CacheFile {
        version: CACHE_VERSION,
        offset_seconds,
        entries: HashMap::with_capacity(storable),
    };
    let mut results = Vec::with_capacity(parsed.len());
    for (path, events, stamp, _) in parsed {
        if let (Some(events), Some(stamp)) = (&events, stamp) {
            next.entries.insert(
                path.clone(),
                CacheEntry {
                    mtime_ns: stamp.mtime_ns,
                    size: stamp.size,
                    events: events.clone(),
                },
            );
        }
        results.push((path, events));
    }
    store_cache(cache_file, &next);
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_with(version: u32, offset_seconds: i32) -> CacheFile {
        CacheFile {
            version,
            offset_seconds,
            entries: HashMap::new(),
        }
    }

    #[test]
    fn cache_invalidated_on_offset_or_version_change() {
        let jst = 9 * 3600; // +09:00 in seconds

        // Same version and offset: reusable.
        assert!(cache_is_reusable(&cache_with(CACHE_VERSION, jst), jst));
        // Offset changed (e.g. the machine moved timezones): discard.
        assert!(!cache_is_reusable(&cache_with(CACHE_VERSION, jst), 0));
        // Version changed: discard regardless of offset.
        assert!(!cache_is_reusable(&cache_with(CACHE_VERSION - 1, jst), jst));
    }

    #[test]
    fn legacy_versioned_caches_are_swept_but_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "claude-v19.bin",
            "claude-v20.bin",
            "claude.bin",
            "codex-v19.bin",
            "claude-vx.bin",
            "claude-v19.tmp",
        ] {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        remove_legacy_caches(dir.path(), "claude");
        let mut left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "claude-v19.tmp",
                "claude-vx.bin",
                "claude.bin",
                "codex-v19.bin"
            ]
        );
    }

    /// A cache discarded for its header is rewritten even when there is
    /// nothing to store, so a bump never leaves the old file behind.
    #[test]
    fn discarded_cache_is_replaced_even_with_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("claude.bin");
        let stale = bincode::serialize(&cache_with(CACHE_VERSION - 1, 0)).unwrap();
        fs::write(&cache, stale).unwrap();
        assert!(load_cache(&cache, 0).is_none());

        parse_files_with_cache(Some(&cache), &[], UtcOffset::UTC, |_| None);
        assert!(
            load_cache(&cache, 0).is_some(),
            "rewritten with the current header"
        );
    }

    /// An unchanged file set is served from the cache without rewriting it;
    /// a changed file re-parses and rewrites. The file is owner-only on Unix.
    #[test]
    fn unchanged_runs_do_not_rewrite_the_cache() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("a.jsonl");
        fs::write(&log, b"one\n").unwrap();
        let cache = dir.path().join("claude.bin");
        let parses = AtomicUsize::new(0);
        let parse = |_: &Path| {
            parses.fetch_add(1, Ordering::SeqCst);
            Some(FileEvents::default())
        };
        let files = vec![log.clone()];

        parse_files_with_cache(Some(&cache), &files, UtcOffset::UTC, parse);
        assert_eq!(parses.load(Ordering::SeqCst), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        // Stamp the cache with a time no write could produce, so "rewritten"
        // is a plain inequality instead of a sleep-dependent ordering.
        let sentinel = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        fs::File::options()
            .write(true)
            .open(&cache)
            .unwrap()
            .set_modified(sentinel)
            .unwrap();
        let modified = || fs::metadata(&cache).unwrap().modified().unwrap();
        assert_eq!(modified(), sentinel);

        parse_files_with_cache(Some(&cache), &files, UtcOffset::UTC, parse);
        assert_eq!(parses.load(Ordering::SeqCst), 1, "served from cache");
        assert_eq!(modified(), sentinel, "unchanged run must not rewrite");

        fs::write(&log, b"one\ntwo\n").unwrap();
        parse_files_with_cache(Some(&cache), &files, UtcOffset::UTC, parse);
        assert_eq!(parses.load(Ordering::SeqCst), 2, "changed file re-parses");
        assert_ne!(modified(), sentinel, "changed run rewrites");
    }
}
