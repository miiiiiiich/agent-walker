pub mod agy;
pub mod claude;
pub mod codex;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};
use tracing::debug;

use crate::model::{Collection, DurationEvent, ScanStats, SessionTouch, ToolEvent, UsageEvent};

const CACHE_VERSION: u32 = 6;

/// Normalize a working-directory path into a project label: strip the home
/// prefix, keep the real path separators ("/Users/me/code/app" -> "code/app").
pub fn project_from_cwd(cwd: &str) -> String {
    let stripped = std::env::var("HOME")
        .ok()
        .and_then(|home| cwd.strip_prefix(&format!("{home}/")).map(ToOwned::to_owned))
        .unwrap_or_else(|| cwd.to_owned());
    stripped.trim_start_matches('/').to_owned()
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
    /// Compress raw session touches: per (session, date) only the first and
    /// last touch matter for sessions / active-day / span aggregation.
    /// Keeps memory and cache size bounded for 100k-line session files.
    pub fn compress_touches(&mut self) {
        if self.session_touches.len() <= 2 {
            return;
        }
        let mut bounds: HashMap<(String, Date), (OffsetDateTime, OffsetDateTime)> = HashMap::new();
        for touch in self.session_touches.drain(..) {
            let key = (touch.session_id, touch.timestamp.date());
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
    entries: HashMap<PathBuf, CacheEntry>,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    mtime_ns: u128,
    size: u64,
    events: FileEvents,
}

fn cache_path(cache_name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".cache")
            .join("agentwalker")
            .join(format!("{cache_name}-v{CACHE_VERSION}.bin")),
    )
}

fn load_cache(cache_name: &str) -> CacheFile {
    let Some(path) = cache_path(cache_name) else {
        return CacheFile::default();
    };
    let Ok(bytes) = fs::read(&path) else {
        return CacheFile::default();
    };
    match bincode::deserialize::<CacheFile>(&bytes) {
        Ok(cache) if cache.version == CACHE_VERSION => cache,
        _ => {
            debug!(path = %path.display(), "discarding stale or corrupt cache");
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
        let _ = fs::rename(&temp, &path);
    }
}

/// Parse `files` through `parse`, reusing cached per-file results when the
/// file is byte-identical to the last run ((mtime, size) match). Cache misses
/// are parsed in parallel; results are returned in `files` order so that
/// downstream deduplication stays deterministic. `cache_name: None` disables
/// the on-disk cache (tests, ad-hoc directories).
pub fn parse_files_cached(
    cache_name: Option<&str>,
    files: &[PathBuf],
    parse: impl Fn(&Path) -> Option<FileEvents> + Sync,
) -> Vec<(PathBuf, Option<FileEvents>)> {
    let cache = cache_name.map(load_cache).unwrap_or_default();

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
