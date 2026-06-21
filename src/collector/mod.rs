pub mod agy;
pub mod claude;
pub mod codex;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime, UtcOffset};
use tracing::debug;

use crate::model::{Collection, DurationEvent, ScanStats, SessionTouch, ToolEvent, UsageEvent};

/// Bumped to 7 when session-touch compression moved from UTC to local-day
/// bucketing: cached `FileEvents` carry compressed touches that depend on the
/// local offset. A cache is now invalidated by EITHER a version bump OR a
/// changed `local_offset` (recorded in `CacheFile::offset_seconds`), so a
/// machine-TZ change is detected and the cache rebuilt automatically — no
/// `--no-cache` needed. The per-file key remains (mtime, size).
const CACHE_VERSION: u32 = 7;

/// Normalize a working-directory path into a project label: strip the home
/// prefix, keep the real path separators ("/Users/me/code/app" -> "code/app",
/// "C:\\Users\\me\\code\\app" -> "code\\app"). A session whose cwd is exactly
/// the home directory renders as "~" rather than an empty label so the
/// PROJECTS row has something readable.
pub fn project_from_cwd(cwd: &str) -> String {
    let home = crate::paths::home_dir()
        .ok()
        .and_then(|home| home.to_str().map(ToOwned::to_owned));
    let stripped = home
        .and_then(|home| strip_home_prefix(cwd, &home))
        .unwrap_or_else(|| cwd.trim_start_matches(['/', '\\']).to_owned());
    if stripped.is_empty() {
        "~".to_owned()
    } else {
        stripped
    }
}

/// Strip the home prefix and the single separator that follows it. Windows
/// filesystems are case-insensitive, so a cwd recorded as `c:\Users\me\…`
/// must still match a home of `C:\Users\me`; compare case-insensitively but
/// slice the original cwd so the rest of the path keeps its real casing.
/// Backslashes and forward slashes are equivalent on Windows, so the prefix
/// match normalizes both to `/` before comparing — npm/Node tooling often
/// records cwds with forward slashes even on Windows. Requires a path-
/// component boundary after the prefix so that `C:\Users\metadata` does not
/// get stripped against home `C:\Users\me`.
#[cfg(windows)]
fn strip_home_prefix(cwd: &str, home: &str) -> Option<String> {
    // Trim any trailing separator on `home` (e.g. a drive-root home like
    // `D:\`) so `home.len()` doesn't include the separator and the boundary
    // check below stays meaningful.
    let home = home.trim_end_matches(['/', '\\']);
    if home.is_empty() {
        return None;
    }
    // `get` keeps us safe if `home.len()` lands inside a multi-byte UTF-8
    // character in `cwd`; `split_at` would panic there.
    let head = cwd.get(..home.len())?;
    let rest = cwd.get(home.len()..)?;
    if !head
        .replace('\\', "/")
        .eq_ignore_ascii_case(&home.replace('\\', "/"))
    {
        return None;
    }
    if !rest.is_empty() && !rest.starts_with(['/', '\\']) {
        return None;
    }
    Some(rest.trim_start_matches(['/', '\\']).to_owned())
}

/// Same shape on Unix: require a path-component boundary so that home
/// `/Users/me` does not silently strip a cwd like `/Users/metadata/app` into
/// `tadata/app` (an attribution bug the previous `{home}/` prefix avoided).
#[cfg(not(windows))]
fn strip_home_prefix(cwd: &str, home: &str) -> Option<String> {
    // Trim any trailing slash on `home` so the boundary check below isn't
    // defeated when `dirs::home_dir` returns `/home/me/`.
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return None;
    }
    let rest = cwd.strip_prefix(home)?;
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    Some(rest.trim_start_matches('/').to_owned())
}

/// Events extracted from a single log file. The unit of caching: parsed once,
/// reused as long as (mtime, size) of the source file are unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileEvents {
    pub usage_events: Vec<KeyedUsageEvent>,
    pub tool_events: Vec<KeyedToolEvent>,
    pub session_touches: Vec<SessionTouch>,
    pub duration_events: Vec<DurationEvent>,
    pub lines_seen: usize,
    pub parse_errors: usize,
}

/// Usage event with an optional cross-file deduplication key
/// (e.g. Claude message id appearing in both a session file and a fork).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedUsageEvent {
    pub key: Option<String>,
    pub event: UsageEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedToolEvent {
    pub key: Option<String>,
    pub event: ToolEvent,
}

impl FileEvents {
    /// Compress raw session touches: per (session, local date) only the first
    /// and last touch matter for sessions / active-day / span aggregation.
    /// Keeps memory and cache size bounded for 100k-line session files.
    ///
    /// Bucketing uses the local-offset date to match the analyzer, which buckets
    /// concurrency / longest-session / daily-sessions by local day. The result
    /// therefore depends on `local_offset`; cached `FileEvents` embed this
    /// interpretation (the cache is keyed on file mtime/size, so a machine-TZ
    /// change is not reflected automatically — rerun with `--no-cache`).
    pub fn compress_touches(&mut self, local_offset: UtcOffset) {
        if self.session_touches.len() <= 2 {
            return;
        }
        let mut bounds: HashMap<(String, Date), (OffsetDateTime, OffsetDateTime)> = HashMap::new();
        for touch in self.session_touches.drain(..) {
            let key = (
                touch.session_id,
                touch.timestamp.to_offset(local_offset).date(),
            );
            bounds
                .entry(key)
                .and_modify(|(start, end)| {
                    *start = (*start).min(touch.timestamp);
                    *end = (*end).max(touch.timestamp);
                })
                .or_insert((touch.timestamp, touch.timestamp));
        }
        for ((session_id, _), (start, end)) in bounds {
            self.session_touches.push(SessionTouch {
                timestamp: start,
                session_id: session_id.clone(),
            });
            if end != start {
                self.session_touches.push(SessionTouch {
                    timestamp: end,
                    session_id,
                });
            }
        }
    }
}

/// Recursively list files under `dir` with the given extension whose mtime is
/// at or after `mtime_floor`. A file whose last write predates the analysis
/// window cannot contain in-window lines, so it is skipped entirely.
pub fn list_files(
    dir: &Path,
    extension: &str,
    mtime_floor: Option<SystemTime>,
    stats: &mut ScanStats,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk(dir, extension, mtime_floor, stats, &mut files);
    files.sort();
    files
}

fn walk(
    dir: &Path,
    extension: &str,
    mtime_floor: Option<SystemTime>,
    stats: &mut ScanStats,
    files: &mut Vec<PathBuf>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            stats.unreadable_dirs += 1;
            debug!(path = %dir.display(), %error, "skipping unreadable directory");
            return;
        }
    };

    for entry in entries {
        let Ok(entry) = entry else {
            stats.unreadable_files += 1;
            continue;
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                stats.unreadable_files += 1;
                debug!(path = %path.display(), %error, "skipping path with unreadable file type");
                continue;
            }
        };
        if file_type.is_dir() {
            walk(&path, extension, mtime_floor, stats, files);
            continue;
        }
        if path.extension().is_none_or(|value| value != extension) {
            continue;
        }
        if let Some(floor) = mtime_floor
            && entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified < floor)
        {
            continue;
        }
        files.push(path);
    }
}

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

/// Merge ordered per-file events into the collection, applying cross-file
/// deduplication. Keyed usage duplicates keep the variant with the larger
/// token volume; keyed tool duplicates are dropped.
pub fn merge_into(collection: &mut Collection, per_file: Vec<(PathBuf, Option<FileEvents>)>) {
    let mut seen_usage: HashMap<String, usize> = HashMap::new();
    let mut seen_tools: HashSet<String> = HashSet::new();

    for (path, events) in per_file {
        collection.stats.files_seen += 1;
        let Some(events) = events else {
            collection.stats.unreadable_files += 1;
            debug!(path = %path.display(), "skipping unreadable log file");
            continue;
        };
        collection.stats.lines_seen += events.lines_seen;
        collection.stats.parse_errors += events.parse_errors;
        collection.session_touches.extend(events.session_touches);
        collection.duration_events.extend(events.duration_events);

        for keyed in events.usage_events {
            match keyed.key {
                Some(key) => {
                    if let Some(index) = seen_usage.get(&key).copied() {
                        if keyed.event.usage.token_volume()
                            > collection.usage_events[index].usage.token_volume()
                        {
                            collection.usage_events[index] = keyed.event;
                        }
                    } else {
                        seen_usage.insert(key, collection.usage_events.len());
                        collection.usage_events.push(keyed.event);
                    }
                }
                None => collection.usage_events.push(keyed.event),
            }
        }

        for keyed in events.tool_events {
            match keyed.key {
                Some(key) => {
                    if seen_tools.insert(key) {
                        collection.tool_events.push(keyed.event);
                    }
                }
                None => collection.tool_events.push(keyed.event),
            }
        }
    }

    collection.stats.usage_events = collection.usage_events.len();
    collection.stats.tool_events = collection.tool_events.len();
    collection.stats.duration_events = collection.duration_events.len();
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
    fn strip_home_prefix_unix() {
        // Posix-only test: case-sensitive byte comparison with boundary check.
        if !cfg!(windows) {
            assert_eq!(
                strip_home_prefix("/Users/me/code/app", "/Users/me"),
                Some("code/app".to_owned()),
            );
            // No prefix match → None so the caller falls back.
            assert_eq!(strip_home_prefix("/var/log/x", "/Users/me"), None);
            // Non-boundary sibling does not strip (would otherwise yield
            // "tadata/app" for "/Users/metadata/app").
            assert_eq!(strip_home_prefix("/Users/metadata/app", "/Users/me"), None);
            assert_eq!(strip_home_prefix("/Users/me-work/app", "/Users/me"), None);
            // Trailing slash on home still strips cleanly.
            assert_eq!(
                strip_home_prefix("/Users/me/code", "/Users/me/"),
                Some("code".to_owned()),
            );
            // Cwd equal to home returns empty (the caller substitutes "~").
            assert_eq!(
                strip_home_prefix("/Users/me", "/Users/me"),
                Some(String::new())
            );
        }
    }

    #[test]
    fn project_from_cwd_renames_home_to_tilde() {
        // When the cwd resolves to the home directory itself, the project
        // label is "~" rather than the empty string.
        if !cfg!(windows) {
            // Cannot easily inject a fake home; only verify the empty-string
            // fallback path through a leading-slash cwd that the home strip
            // would not match (so the trim-only branch runs) is never empty.
            assert_eq!(project_from_cwd("/"), "~");
        }
    }

    #[test]
    fn strip_home_prefix_windows_case_insensitive() {
        // Windows-only test: case-insensitive prefix match with separator
        // normalization and boundary check.
        if cfg!(windows) {
            assert_eq!(
                strip_home_prefix(r"C:\Users\me\code\app", r"C:\Users\me"),
                Some(r"code\app".to_owned()),
            );
            // Lowercase drive letter still strips.
            assert_eq!(
                strip_home_prefix(r"c:\users\me\code\app", r"C:\Users\me"),
                Some(r"code\app".to_owned()),
            );
            // Mixed separators (forward-slashed cwd from npm/Node tooling,
            // backslashed home from dirs::home_dir).
            assert_eq!(
                strip_home_prefix("C:/Users/me/code/app", r"C:\Users\me"),
                Some("code/app".to_owned()),
            );
            // Non-boundary sibling does not strip.
            assert_eq!(
                strip_home_prefix(r"C:\Users\metadata", r"C:\Users\me"),
                None
            );
            // Drive-root home with a trailing separator still strips.
            assert_eq!(
                strip_home_prefix(r"D:\code\app", r"D:\"),
                Some(r"code\app".to_owned()),
            );
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
