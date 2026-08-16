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
///
/// The per-file key remains (mtime, size); `--no-cache` is never required.
const CACHE_VERSION: u32 = 15;

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

fn cache_path(cache_name: &str) -> Option<PathBuf> {
    Some(
        crate::paths::cache_dir()
            .ok()?
            .join(format!("{cache_name}-v{CACHE_VERSION}.bin")),
    )
}

/// A cached file is reusable only when both the format version and the
/// local-offset it was compressed under match the current run; a mismatch in
/// either means the compressed touches could be misplaced, so it is discarded.
fn cache_is_reusable(cache: &CacheFile, offset_seconds: i32) -> bool {
    cache.version == CACHE_VERSION && cache.offset_seconds == offset_seconds
}

fn load_cache(cache_name: &str, offset_seconds: i32) -> CacheFile {
    let Some(path) = cache_path(cache_name) else {
        return CacheFile::default();
    };
    let Ok(bytes) = fs::read(&path) else {
        return CacheFile::default();
    };
    match bincode::deserialize::<CacheFile>(&bytes) {
        Ok(cache) if cache_is_reusable(&cache, offset_seconds) => cache,
        _ => {
            debug!(path = %path.display(), "discarding stale, corrupt, or offset-changed cache");
            CacheFile::default()
        }
    }
}

fn store_cache(cache_name: &str, cache: &CacheFile) {
    let Some(path) = cache_path(cache_name) else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = bincode::serialize(cache) else {
        return;
    };
    let temp = path.with_extension("tmp");
    if fs::write(&temp, bytes).is_ok() {
        // `std::fs::rename` is atomic on Unix and uses
        // `MoveFileExW + MOVEFILE_REPLACE_EXISTING` on Windows, so the
        // destination is overwritten on both platforms without an explicit
        // unlink. Removing the file first would break the Unix atomicity
        // guarantee and momentarily leave the cache missing for concurrent
        // readers.
        let _ = fs::rename(&temp, &path);
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
    let offset_seconds = local_offset.whole_seconds();
    let cache = cache_name
        .map(|name| load_cache(name, offset_seconds))
        .unwrap_or_default();

    let parsed: Vec<(PathBuf, Option<FileEvents>, Option<FileStamp>)> = files
        .par_iter()
        .map(|path| {
            let stamp = file_stamp(path);
            if let Some(stamp) = stamp
                && let Some(entry) = cache.entries.get(path)
                && entry.mtime_ns == stamp.mtime_ns
                && entry.size == stamp.size
            {
                return (path.clone(), Some(entry.events.clone()), Some(stamp));
            }
            (path.clone(), parse(path), stamp)
        })
        .collect();

    let mut next = CacheFile {
        version: CACHE_VERSION,
        offset_seconds,
        entries: HashMap::with_capacity(parsed.len()),
    };
    let mut results = Vec::with_capacity(parsed.len());
    for (path, events, stamp) in parsed {
        if cache_name.is_some()
            && let (Some(events), Some(stamp)) = (&events, stamp)
        {
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
    if let Some(name) = cache_name {
        store_cache(name, &next);
    }
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
}
