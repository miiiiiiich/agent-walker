use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use time::UtcOffset;
use tracing::debug;

use super::events::FileEvents;

/// Parsing or attribution changes require a version bump so cached events do not retain old semantics.
const CACHE_VERSION: u32 = 20;

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

const ORPHAN_TEMP_AGE: Duration = Duration::from_hours(1);
const OLD_EXPORT_AGE: Duration = Duration::from_hours(24);

pub fn sweep_cache_dir() {
    if let Ok(dir) = crate::paths::cache_dir() {
        sweep(&dir, SystemTime::now());
    }
}

fn all_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

fn sweep(dir: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let legacy = name
            .strip_suffix(".bin")
            .or_else(|| name.strip_suffix(".tmp"))
            .and_then(|stem| stem.rsplit_once("-v"))
            .is_some_and(|(_, version)| all_digits(version));
        let age = || {
            entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
        };
        let orphan_temp = name
            .rsplit_once(".tmp")
            .is_some_and(|(_, pid)| all_digits(pid))
            && age().is_some_and(|age| age > ORPHAN_TEMP_AGE);
        // Cursor exports outlive their usefulness after a day (see
        // `cursor::CSV_STALE_FOR`); an account switch would otherwise leave
        // the old account's usage on disk for good.
        let old_export = name
            .strip_prefix("cursor-")
            .and_then(|rest| rest.rsplit_once('.'))
            .is_some_and(|(_, ext)| matches!(ext, "csv" | "failed"))
            && age().is_some_and(|age| age > OLD_EXPORT_AGE);
        if legacy || orphan_temp || old_export {
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
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
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
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::write(path, bytes)
}

/// The cache directory, created owner-only: file names in it are derived
/// from account ids, so even the listing stays private.
pub(crate) fn private_dir() -> Option<PathBuf> {
    let dir = crate::paths::cache_dir().ok()?;
    fs::create_dir_all(&dir).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    Some(dir)
}

fn store_cache(path: &Path, cache: &CacheFile) {
    if private_dir().is_none() {
        return;
    }
    let Ok(bytes) = bincode::serialize(cache) else {
        return;
    };
    // Per-process name: two concurrent runs must never share a temp inode.
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    if write_private(&temp, &bytes).is_ok() {
        // Replace by rename without unlinking first, preserving atomic replacement on Unix.
        let _ = fs::rename(&temp, path);
    }
}

/// Preserve input file order so downstream deduplication stays deterministic.
pub fn parse_files_cached(
    cache_name: Option<&str>,
    files: &[PathBuf],
    local_offset: UtcOffset,
    parse: impl Fn(&Path) -> Option<FileEvents> + Sync,
) -> Vec<(PathBuf, Option<FileEvents>)> {
    let cache_file =
        cache_name.and_then(|name| Some(cache_file(&crate::paths::cache_dir().ok()?, name)));
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

        assert!(cache_is_reusable(&cache_with(CACHE_VERSION, jst), jst));
        assert!(!cache_is_reusable(&cache_with(CACHE_VERSION, jst), 0));
        assert!(!cache_is_reusable(&cache_with(CACHE_VERSION - 1, jst), jst));
    }

    #[test]
    fn sweep_removes_legacy_files_and_stale_temps_only() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "claude-v19.bin",
            "claude-v19.tmp",
            "codex-v20.bin",
            "claude.tmp111",
            "claude.tmp222",
            "cursor-aa.csv",
            "cursor-bb.csv",
            "claude.bin",
            "claude-vx.bin",
            "pricing.json",
        ] {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        let now = SystemTime::now();
        fs::File::options()
            .write(true)
            .open(dir.path().join("claude.tmp222"))
            .unwrap()
            .set_modified(now - ORPHAN_TEMP_AGE * 2)
            .unwrap();
        fs::File::options()
            .write(true)
            .open(dir.path().join("cursor-bb.csv"))
            .unwrap()
            .set_modified(now - OLD_EXPORT_AGE * 2)
            .unwrap();
        sweep(dir.path(), now);
        let mut left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "claude-vx.bin",
                "claude.bin",
                "claude.tmp111",
                "cursor-aa.csv",
                "pricing.json"
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
