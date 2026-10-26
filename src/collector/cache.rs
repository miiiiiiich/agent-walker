use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use time::UtcOffset;
use tracing::debug;

use super::events::FileEvents;
use crate::owned_file;

/// Parsing, attribution, or encoding changes require a version bump so cached
/// events are never read with old semantics.
const CACHE_VERSION: u32 = 24;

/// Far above what months of heavy use encode to; bounds what a crafted or
/// runaway file can make a load read into memory.
const MAX_CACHE_BYTES: u64 = 512 * 1024 * 1024;

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
        let orphan_temp = name
            .rsplit_once(".tmp")
            .is_some_and(|(_, pid)| all_digits(pid))
            && entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > ORPHAN_TEMP_AGE);
        if legacy || orphan_temp {
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
    let bytes = owned_file::read_regular(path, MAX_CACHE_BYTES)
        .inspect_err(|error| debug!(path = %path.display(), %error, "cache not read"))
        .ok()?;
    match decode(&bytes) {
        Some(cache) if cache_is_reusable(&cache, offset_seconds) => Some(cache),
        _ => {
            debug!(path = %path.display(), "discarding stale, corrupt, or offset-changed cache");
            None
        }
    }
}

/// postcard stops where the struct ends, so a file in another encoding can
/// decode as a short prefix (a bincode-era file stamped with the current
/// version reads as an empty cache); only an exact fit counts.
fn decode(bytes: &[u8]) -> Option<CacheFile> {
    match postcard::take_from_bytes::<CacheFile>(bytes) {
        Ok((cache, [])) => Some(cache),
        _ => None,
    }
}

fn store_cache(path: &Path, cache: &CacheFile) {
    let Some(parent) = path.parent() else {
        return;
    };
    if owned_file::create_private_dir(parent).is_err() {
        return;
    }
    let Ok(bytes) = postcard::to_stdvec(cache) else {
        return;
    };
    // Every later load would refuse it, so writing it only costs time.
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        debug!(path = %path.display(), bytes = bytes.len(), "cache over the read limit; not stored");
        return;
    }
    if let Err(error) = owned_file::write_replacing(path, &bytes, owned_file::PRIVATE_MODE) {
        debug!(path = %path.display(), %error, "cache not stored; next run re-parses");
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

    fn encoded(cache: &CacheFile) -> Vec<u8> {
        postcard::to_stdvec(cache).unwrap()
    }

    /// `CacheFile` as bincode 1 wrote it: fixed-width little-endian integers
    /// and u64 length prefixes, one entry with empty events.
    fn bincode_era_cache(version: u32) -> Vec<u8> {
        let path = b"/logs/a.jsonl";
        let mut bytes = Vec::new();
        bytes.extend(version.to_le_bytes());
        bytes.extend(0_i32.to_le_bytes());
        bytes.extend(1_u64.to_le_bytes());
        bytes.extend((path.len() as u64).to_le_bytes());
        bytes.extend(path);
        bytes.extend(0_u128.to_le_bytes());
        bytes.extend(0_u64.to_le_bytes());
        // 11 event vectors, then lines_seen and parse_errors.
        for _ in 0..13 {
            bytes.extend(0_u64.to_le_bytes());
        }
        bytes
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
            ".claude.bin.tmp333",
            "claude.bin",
            "claude-vx.bin",
            "pricing.json",
        ] {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        let now = SystemTime::now();
        for orphan in ["claude.tmp222", ".claude.bin.tmp333"] {
            fs::File::options()
                .write(true)
                .open(dir.path().join(orphan))
                .unwrap()
                .set_modified(now - ORPHAN_TEMP_AGE * 2)
                .unwrap();
        }
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
        let stale = encoded(&cache_with(CACHE_VERSION - 1, 0));
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

    #[test]
    fn bincode_era_cache_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("claude.bin");
        for version in [CACHE_VERSION - 1, CACHE_VERSION] {
            fs::write(&cache, bincode_era_cache(version)).unwrap();
            assert!(load_cache(&cache, 0).is_none(), "version {version}");
        }
    }

    #[test]
    fn length_prefixes_past_the_input_are_rejected() {
        let huge = postcard::to_stdvec(&u64::MAX).unwrap();
        // Version and offset; the trailing zero is the entry count.
        let mut header = encoded(&cache_with(CACHE_VERSION, 0));
        assert_eq!(header.pop(), Some(0));
        let one_entry = [header.clone(), vec![1]].concat();
        let path = postcard::to_stdvec(&PathBuf::from("/a")).unwrap();

        // mtime_ns, size, 11 empty event vectors, lines_seen, parse_errors.
        let valid = [one_entry.clone(), path.clone(), vec![0; 15]].concat();
        assert!(
            decode(&valid).is_some(),
            "the crafted layout is a real cache"
        );

        let entry_count = [header, huge.clone()].concat();
        let path_len = [one_entry.clone(), huge.clone()].concat();
        let events_len = [one_entry, path, vec![0, 0], huge].concat();

        for bytes in [entry_count, path_len, events_len] {
            assert!(decode(&bytes).is_none());
        }
    }

    #[test]
    fn oversized_cache_file_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("claude.bin");
        fs::write(&cache, encoded(&cache_with(CACHE_VERSION, 0))).unwrap();
        assert!(load_cache(&cache, 0).is_some());

        fs::File::options()
            .write(true)
            .open(&cache)
            .unwrap()
            .set_len(MAX_CACHE_BYTES + 1)
            .unwrap();
        assert_eq!(
            owned_file::read_regular(&cache, MAX_CACHE_BYTES)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::FileTooLarge
        );
        assert!(load_cache(&cache, 0).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_cache_file_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere.bin");
        fs::write(&elsewhere, encoded(&cache_with(CACHE_VERSION, 0))).unwrap();
        assert!(load_cache(&elsewhere, 0).is_some());
        let cache = dir.path().join("claude.bin");
        std::os::unix::fs::symlink(&elsewhere, &cache).unwrap();

        assert!(load_cache(&cache, 0).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn fifo_at_the_cache_path_is_skipped_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("claude.bin");
        let made = std::process::Command::new("mkfifo")
            .arg(&cache)
            .status()
            .unwrap();
        assert!(made.success());

        let (sender, receiver) = std::sync::mpsc::channel();
        let path = cache.clone();
        std::thread::spawn(move || {
            let _ = sender.send(load_cache(&path, 0).is_none());
        });
        let outcome = receiver.recv_timeout(Duration::from_secs(5));
        if outcome.is_err() {
            // Give the blocked open a writer so the thread can finish.
            let _ = fs::File::options().write(true).open(&cache);
        }
        assert_eq!(outcome, Ok(true), "a FIFO must be skipped, not opened");
    }

    #[cfg(unix)]
    #[test]
    fn store_ignores_a_symlink_planted_at_the_old_temp_name() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(
            &victim,
            dir.path().join(format!("claude.tmp{}", std::process::id())),
        )
        .unwrap();
        let cache = dir.path().join("claude.bin");

        parse_files_with_cache(Some(&cache), &[], UtcOffset::UTC, |_| None);

        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(fs::symlink_metadata(&cache).unwrap().is_file());
        assert!(load_cache(&cache, 0).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn created_cache_dir_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("agent-walker").join("claude.bin");

        parse_files_with_cache(Some(&cache), &[], UtcOffset::UTC, |_| None);

        let parent = fs::metadata(cache.parent().unwrap()).unwrap();
        assert_eq!(parent.permissions().mode() & 0o777, 0o700);
        assert!(load_cache(&cache, 0).is_some());
    }
}
