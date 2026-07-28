pub mod agy;
mod agy_conv;
pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod grok;
pub mod opencode;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime, UtcOffset};
use tracing::debug;

use crate::model::{
    Collection, CreditSample, DurationEvent, EffortEvent, ModeEvent, RateLimitSample, ScanStats,
    SessionTouch, ToolEvent, UsageEvent,
};

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
///
/// The per-file key remains (mtime, size); `--no-cache` is never required.
const CACHE_VERSION: u32 = 13;

/// Normalize a working-directory path into a project label: strip the home
/// prefix and (on Windows) normalize separators to `/` so the same repo
/// collapses to one project key regardless of native (`\`) vs npm/Node-style
/// (`/`) cwd. "/Users/me/code/app" -> "code/app",
/// "C:\\Users\\me\\code\\app" -> "code/app". A session whose cwd is exactly
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
    // Normalize backslashes to forward slashes in the remainder so the same
    // repository visited as `C:\Users\me\code\app` (native) and
    // `C:/Users/me/code/app` (npm/Node tooling) collapses to one project key
    // (`code/app`) instead of splitting the totals across two labels.
    Some(rest.trim_start_matches(['/', '\\']).replace('\\', "/"))
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
    pub duration_events: Vec<KeyedDurationEvent>,
    pub rate_limit_samples: Vec<KeyedRateLimitSample>,
    pub credit_samples: Vec<KeyedCreditSample>,
    pub effort_events: Vec<KeyedEffortEvent>,
    pub mode_events: Vec<KeyedModeEvent>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedRateLimitSample {
    pub key: Option<String>,
    pub event: RateLimitSample,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedCreditSample {
    pub key: Option<String>,
    pub event: CreditSample,
}

/// Duration event with an optional cross-file dedup key. Most collectors
/// leave it `None` (their durations never appear twice); Grok keys turn
/// durations by `prompt_id` because fork copies replay the parent's turns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedDurationEvent {
    pub key: Option<String>,
    pub event: DurationEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedEffortEvent {
    pub key: Option<String>,
    pub event: EffortEvent,
}

/// Mode event keyed by message id. Duplicate lines for the same message can
/// disagree (a streaming fragment without the thinking block yet), so
/// duplicates merge with OR semantics instead of being dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedModeEvent {
    pub key: Option<String>,
    pub event: ModeEvent,
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

/// Fill missing metadata on `target` from `source`. Duplicate keyed usage
/// lines can carry different metadata (a streaming fragment has the tokens
/// but not yet the attribution fields), so whichever variant wins on token
/// volume must still absorb the other's metadata instead of discarding it.
fn fill_usage_metadata(target: &mut UsageEvent, source: &UsageEvent) {
    if target.timestamp.is_none() {
        target.timestamp = source.timestamp;
    }
    if target.session_id.is_none() {
        target.session_id.clone_from(&source.session_id);
    }
    if target.model.is_none() {
        target.model.clone_from(&source.model);
    }
    if target.attribution_agent.is_none() {
        target
            .attribution_agent
            .clone_from(&source.attribution_agent);
    }
    if target.attribution_skill.is_none() {
        target
            .attribution_skill
            .clone_from(&source.attribution_skill);
    }
    if target.project.is_none() {
        target.project.clone_from(&source.project);
    }
    if target.reported_cost_usd.is_none() {
        target.reported_cost_usd = source.reported_cost_usd;
    }
}

/// Earlier of two optional timestamps; a lone `Some` beats `None`. Used on
/// keyed duplicates so the original observation's time wins over a replayed
/// copy stamped at the fork instant, whatever the file scan order.
fn older_timestamp(a: Option<OffsetDateTime>, b: Option<OffsetDateTime>) -> Option<OffsetDateTime> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Insert one keyed event into `sink`: unkeyed events pass through, the first
/// occurrence of a key is kept, and later duplicates fold into it via `merge`
/// (metadata fill, OR flags, earliest timestamp).
fn dedupe_into<E>(
    sink: &mut Vec<E>,
    seen: &mut HashMap<String, usize>,
    key: Option<String>,
    event: E,
    merge: impl FnOnce(&mut E, E),
) {
    match key {
        Some(key) => {
            if let Some(index) = seen.get(&key).copied() {
                merge(&mut sink[index], event);
            } else {
                seen.insert(key, sink.len());
                sink.push(event);
            }
        }
        None => sink.push(event),
    }
}

fn absorb_file_stats(collection: &mut Collection, events: &FileEvents) {
    collection.stats.lines_seen += events.lines_seen;
    collection.stats.parse_errors += events.parse_errors;
}

/// Merge ordered per-file events into the collection, applying cross-file
/// deduplication. Keyed usage duplicates keep the variant with the larger
/// token volume but fill missing metadata from the loser; keyed tool /
/// rate-limit / effort duplicates are dropped; keyed mode duplicates merge
/// their flags with OR. Every keyed duplicate keeps the EARLIEST observed
/// timestamp: a fork replay is stamped at the fork instant, and file scan
/// order doesn't put originals first (`archived_sessions` sorts before
/// `sessions` wholesale), so first-seen-wins would let a replay shift an
/// event's day attribution.
#[allow(
    clippy::too_many_lines,
    reason = "One homogeneous keyed-dedupe loop per event kind; splitting them adds indirection without reuse and the count grows with event kinds, not complexity."
)]
pub fn merge_into(collection: &mut Collection, per_file: Vec<(PathBuf, Option<FileEvents>)>) {
    let mut seen_usage: HashMap<String, usize> = HashMap::new();
    let mut seen_tools: HashMap<String, usize> = HashMap::new();
    let mut seen_limits: HashMap<String, usize> = HashMap::new();
    let mut seen_credits: HashMap<String, usize> = HashMap::new();
    let mut seen_durations: HashMap<String, usize> = HashMap::new();
    let mut seen_efforts: HashMap<String, usize> = HashMap::new();
    let mut seen_modes: HashMap<String, usize> = HashMap::new();

    for (path, events) in per_file {
        collection.stats.files_seen += 1;
        let Some(events) = events else {
            collection.stats.unreadable_files += 1;
            debug!(path = %path.display(), "skipping unreadable log file");
            continue;
        };
        absorb_file_stats(collection, &events);
        collection.session_touches.extend(events.session_touches);
        for keyed in events.duration_events {
            dedupe_into(
                &mut collection.duration_events,
                &mut seen_durations,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.timestamp = older_timestamp(existing.timestamp, incoming.timestamp);
                },
            );
        }

        for keyed in events.usage_events {
            dedupe_into(
                &mut collection.usage_events,
                &mut seen_usage,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    let timestamp = older_timestamp(existing.timestamp, incoming.timestamp);
                    if incoming.usage.token_volume() > existing.usage.token_volume() {
                        let mut incoming = incoming;
                        fill_usage_metadata(&mut incoming, existing);
                        *existing = incoming;
                    } else {
                        fill_usage_metadata(existing, &incoming);
                    }
                    existing.timestamp = timestamp;
                },
            );
        }

        for keyed in events.tool_events {
            dedupe_into(
                &mut collection.tool_events,
                &mut seen_tools,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.timestamp = older_timestamp(existing.timestamp, incoming.timestamp);
                },
            );
        }

        for keyed in events.rate_limit_samples {
            dedupe_into(
                &mut collection.rate_limit_samples,
                &mut seen_limits,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.timestamp = existing.timestamp.min(incoming.timestamp);
                },
            );
        }

        for keyed in events.credit_samples {
            dedupe_into(
                &mut collection.credit_samples,
                &mut seen_credits,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.timestamp = existing.timestamp.min(incoming.timestamp);
                },
            );
        }

        for keyed in events.effort_events {
            dedupe_into(
                &mut collection.effort_events,
                &mut seen_efforts,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.timestamp = older_timestamp(existing.timestamp, incoming.timestamp);
                },
            );
        }

        for keyed in events.mode_events {
            dedupe_into(
                &mut collection.mode_events,
                &mut seen_modes,
                keyed.key,
                keyed.event,
                |existing, incoming| {
                    existing.has_thinking |= incoming.has_thinking;
                    existing.fast |= incoming.fast;
                    existing.timestamp = older_timestamp(existing.timestamp, incoming.timestamp);
                },
            );
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
            // Lowercase drive letter still strips, remainder normalized.
            assert_eq!(
                strip_home_prefix(r"c:\users\me\code\app", r"C:\Users\me"),
                Some("code/app".to_owned()),
            );
            // Mixed separators (forward-slashed cwd from npm/Node tooling,
            // backslashed home from dirs::home_dir) — and the remainder is
            // returned with normalized forward slashes so a native-style
            // visit to the same repo also lands at `code/app`, not `code\app`.
            assert_eq!(
                strip_home_prefix("C:/Users/me/code/app", r"C:\Users\me"),
                Some("code/app".to_owned()),
            );
            assert_eq!(
                strip_home_prefix(r"C:\Users\me\code\app", r"C:\Users\me"),
                Some("code/app".to_owned()),
            );
            // Non-boundary sibling does not strip.
            assert_eq!(
                strip_home_prefix(r"C:\Users\metadata", r"C:\Users\me"),
                None
            );
            // Drive-root home with a trailing separator still strips (and
            // the remainder is normalized to forward slashes).
            assert_eq!(
                strip_home_prefix(r"D:\code\app", r"D:\"),
                Some("code/app".to_owned()),
            );
        }
    }

    /// A keyed usage duplicate (Claude streaming fragments of one message,
    /// or a Codex fork replay) keeps the larger token volume, the EARLIEST
    /// timestamp, and metadata from both sides — whichever file order they
    /// arrive in.
    #[test]
    fn keyed_usage_merge_keeps_larger_volume_and_earliest_timestamp() {
        use crate::model::{Provider, TokenUsage};

        let early = OffsetDateTime::from_unix_timestamp(1_000).expect("valid timestamp");
        let late = OffsetDateTime::from_unix_timestamp(2_000).expect("valid timestamp");
        let event =
            |timestamp, input_tokens, model: Option<&str>, project: Option<&str>| KeyedUsageEvent {
                key: Some("message:m1".to_owned()),
                event: UsageEvent {
                    timestamp: Some(timestamp),
                    session_id: Some("s1".to_owned()),
                    model: model.map(ToOwned::to_owned),
                    source_kind: crate::model::SourceKind::Main,
                    attribution_agent: None,
                    attribution_skill: None,
                    project: project.map(ToOwned::to_owned),
                    usage: TokenUsage {
                        input_tokens,
                        ..TokenUsage::default()
                    },
                    reported_cost_usd: None,
                },
            };
        // Small fragment first with the early timestamp and a project; the
        // larger fragment arrives later with the model but no project.
        let small_early = event(early, 10, None, Some("proj"));
        let large_late = event(late, 20, Some("claude"), None);

        let mut collection = Collection::new(Provider::Claude, PathBuf::new());
        let per_file = vec![
            (
                PathBuf::from("a.jsonl"),
                Some(FileEvents {
                    usage_events: vec![small_early],
                    ..FileEvents::default()
                }),
            ),
            (
                PathBuf::from("b.jsonl"),
                Some(FileEvents {
                    usage_events: vec![large_late],
                    ..FileEvents::default()
                }),
            ),
        ];
        merge_into(&mut collection, per_file);

        assert_eq!(collection.usage_events.len(), 1);
        let merged = &collection.usage_events[0];
        assert_eq!(merged.usage.input_tokens, 20); // larger volume wins
        assert_eq!(merged.timestamp, Some(early)); // earliest timestamp wins
        assert_eq!(merged.model.as_deref(), Some("claude")); // metadata from both
        assert_eq!(merged.project.as_deref(), Some("proj"));
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
