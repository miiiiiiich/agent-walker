//! Antigravity token usage from the CLI's per-conversation SQLite stores.
//!
//! Antigravity's text logs carry activity but no token counts; the real usage
//! lives in `<root>/conversations/<uuid>.db`, table `gen_metadata` — one row per
//! generation, each an **unlabeled protobuf** (no field names). There's no
//! public schema, so this reads the wire format directly and pulls only the
//! fields it needs. The field numbers were reverse-engineered (cross-checked
//! against the `tokscale` project, which maps them the same way):
//!
//! ```text
//! gen_metadata.#1                  chatModel message
//!   #4                             usage message
//!     #1  varint  fixed system-prompt tokens (~1132, billable input)
//!     #2  varint  newly-processed (non-cached) input tokens
//!     #3  varint  output total (== #9 + #10) — used only as a self-check
//!     #5  varint  cacheRead tokens (only once a cached prefix exists)
//!     #9  varint  output (visible text) tokens
//!     #10 varint  thinking / reasoning tokens
//!     #11 string  response id (dedup key)
//!   #9.#4                          per-generation {#1 sec, #2 nanos} timestamp
//!   #19 string                     model id
//! trajectory_metadata_blob.#1.#1   workspace URI (project)
//! trajectory_metadata_blob.#2      {#1 sec, #2 nanos} session-created timestamp
//! ```
//!
//! Because the numbers are unofficial and could shift on an Antigravity update,
//! every row is self-verified against the precomputed output total `#3`: it must
//! equal `#9 + #10`. A mismatch means the layout drifted → the row is dropped and
//! counted as a parse error. A row that omits `#3` can't be verified, so it's
//! skipped too, but quietly (some healthy rows omit it — not an error).

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use time::{OffsetDateTime, UtcOffset};

use crate::collector::project_from_cwd;
use crate::model::{ScanStats, SourceKind, TokenUsage, UsageEvent};

/// Read every `conversations/*.db` under `root` and return real token usage
/// events. `stats` accumulates files/rows/parse-errors. Falls back gracefully
/// (empty) when the directory or a DB is unreadable.
pub(super) fn collect_usage(
    root: &Path,
    mtime_floor: Option<SystemTime>,
    local_offset: UtcOffset,
    stats: &mut ScanStats,
) -> Vec<UsageEvent> {
    let mut events = Vec::new();
    let floor = mtime_floor.and_then(systemtime_to_ms);
    let conversations = root.join("conversations");
    let Ok(entries) = std::fs::read_dir(&conversations) else {
        return events;
    };
    let mut dbs: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("db"))
        })
        .collect();
    dbs.sort();

    // Dedupe retried generations (same response id) across every DB, so a retry
    // never double-counts.
    let mut seen = HashSet::new();
    for db in dbs {
        let newest = newest_mtime_ms(&db);
        // Skip whole files older than the window (cheap mtime gate, like the
        // JSONL collectors) so a long history doesn't reparse every run. In WAL
        // mode recent writes land in `<db>-wal`, so gate on the newest of the DB
        // and its sidecars, not the main file alone.
        if let (Some(floor_ms), Some(mtime)) = (floor, newest)
            && mtime < floor_ms
        {
            continue;
        }
        parse_db(
            &db,
            floor,
            newest,
            local_offset,
            stats,
            &mut seen,
            &mut events,
        );
    }
    events
}

/// Newest mtime (ms) across the DB and its `-wal` / `-shm` sidecars.
fn newest_mtime_ms(db: &Path) -> Option<i64> {
    ["", "-wal", "-shm"]
        .iter()
        .filter_map(|suffix| {
            let path = if suffix.is_empty() {
                db.to_path_buf()
            } else {
                let mut name = db.as_os_str().to_owned();
                name.push(suffix);
                std::path::PathBuf::from(name)
            };
            std::fs::metadata(path)
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(systemtime_to_ms)
        })
        .max()
}

fn parse_db(
    db: &Path,
    floor_ms: Option<i64>,
    db_mtime_ms: Option<i64>,
    local_offset: UtcOffset,
    stats: &mut ScanStats,
    seen: &mut HashSet<String>,
    events: &mut Vec<UsageEvent>,
) {
    let Some(conn) = open_readonly(db) else {
        stats.unreadable_files += 1;
        return;
    };
    stats.files_seen += 1;
    let session_id = db
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("antigravity")
        .to_owned();
    let (session_ts, project) = trajectory_meta(&conn);
    // Fallback chain for a row's time: its own stamp → the conversation's
    // created-at → the DB file mtime (so a missing timestamp dates the event near
    // when the file was written, never 1970).
    let fallback_ts = if session_ts > 0 {
        session_ts
    } else {
        db_mtime_ms.unwrap_or(0)
    };

    let Ok(mut stmt) = conn.prepare("SELECT data FROM gen_metadata ORDER BY idx") else {
        return;
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0)) else {
        return;
    };
    for row in rows {
        stats.lines_seen += 1;
        let Ok(blob) = row else {
            stats.parse_errors += 1;
            continue;
        };
        match parse_gen(
            &blob,
            &session_id,
            fallback_ts,
            project.as_deref(),
            local_offset,
            seen,
        ) {
            ParseGen::Event(event) => {
                if floor_ms.is_some_and(|floor| {
                    event
                        .timestamp
                        .is_some_and(|ts| ts.unix_timestamp_nanos() / 1_000_000 < i128::from(floor))
                }) {
                    continue;
                }
                events.push(*event);
            }
            ParseGen::Empty => {}
            ParseGen::Drift => stats.parse_errors += 1,
        }
    }
}

enum ParseGen {
    Event(Box<UsageEvent>),
    Empty,
    Drift,
}

fn parse_gen(
    blob: &[u8],
    session_id: &str,
    fallback_ts_ms: i64,
    project: Option<&str>,
    local_offset: UtcOffset,
    seen: &mut HashSet<String>,
) -> ParseGen {
    let Some(chat_model) = message_field(blob, 1) else {
        return ParseGen::Empty;
    };
    let Some(usage) = message_field(chat_model, 4) else {
        return ParseGen::Empty;
    };

    // Clamp untrusted u64 varints into i64 (a corrupt blob could exceed i64::MAX)
    // and combine with saturating arithmetic so totals never wrap negative.
    let v = |field: u64| varint_field(usage, field).unwrap_or(0);
    let to_u64 = |x: u64| x.min(i64::MAX as u64);
    let system = to_u64(v(1));
    let new_input = to_u64(v(2));
    let cache_read = to_u64(v(5));
    let output_text = to_u64(v(9));
    let thinking = to_u64(v(10));

    // Self-verify the field map against the stored output total (#3):
    // - present and == text + thinking → trusted.
    // - present but != → the layout drifted; flag it (Drift → parse error).
    // - absent → can't verify this row, so don't trust it, but absence happens in
    //   healthy data (some rows omit #3), so skip it quietly rather than as an
    //   error.
    match varint_field(usage, 3) {
        Some(total) if to_u64(total) == output_text.saturating_add(thinking) => {}
        Some(_) => return ParseGen::Drift,
        None => return ParseGen::Empty,
    }

    let input = system.saturating_add(new_input);
    if input == 0 && output_text == 0 && cache_read == 0 && thinking == 0 {
        return ParseGen::Empty;
    }

    // Skip a retried generation already counted (same response id, field #11).
    if let Some(id) = string_field(usage, 11).filter(|id| !id.trim().is_empty())
        && !seen.insert(id.to_owned())
    {
        return ParseGen::Empty;
    }

    let timestamp = message_field(chat_model, 9)
        .and_then(|node| message_field(node, 4))
        .and_then(proto_timestamp_ms)
        .filter(|&ms| ms > 0)
        .unwrap_or(fallback_ts_ms);
    let timestamp = ms_to_offset(timestamp, local_offset);

    let model = string_field(chat_model, 19)
        .filter(|text| !text.trim().is_empty())
        .map(ToOwned::to_owned);

    let usage = TokenUsage {
        input_tokens: input,
        // Fold thinking into output so token_volume() counts it (same convention
        // as the OpenCode collector); reasoning is kept separately for display.
        output_tokens: output_text.saturating_add(thinking),
        reasoning_output_tokens: thinking,
        cache_read_input_tokens: cache_read,
        ..TokenUsage::default()
    };
    ParseGen::Event(Box::new(UsageEvent {
        timestamp,
        session_id: Some(session_id.to_owned()),
        model,
        source_kind: SourceKind::Main,
        attribution_agent: None,
        project: project.map(project_from_cwd),
        usage,
        reported_cost_usd: None,
    }))
}

/// `trajectory_metadata_blob` carries the session-created timestamp (`#2`) and
/// the workspace URI (`#1.#1`, used as the project label).
fn trajectory_meta(conn: &Connection) -> (i64, Option<String>) {
    let blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT data FROM trajectory_metadata_blob LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();
    let Some(blob) = blob else {
        return (0, None);
    };
    let ts = message_field(&blob, 2)
        .and_then(proto_timestamp_ms)
        .unwrap_or(0);
    let project = message_field(&blob, 1)
        .and_then(|folder| string_field(folder, 1))
        .map(file_uri_to_path);
    (ts, project)
}

/// `file:///Users/me/x` → `/Users/me/x`; `file:///C:/Users/me/x` →
/// `C:/Users/me/x` (drop the slash Windows leaves before the drive letter).
fn file_uri_to_path(uri: &str) -> String {
    let path = uri.trim_start_matches("file://");
    // A Windows drive path is `/C:/...`; strip the leading slash so it reads as
    // `C:/...`. Unix paths (`/Users/...`) keep their leading slash.
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[2] == b':' && bytes[1].is_ascii_alphabetic() {
        path[1..].to_owned()
    } else {
        path.to_owned()
    }
}

fn open_readonly(db: &Path) -> Option<Connection> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(500));
    Some(conn)
}

fn systemtime_to_ms(time: SystemTime) -> Option<i64> {
    let nanos = time.duration_since(UNIX_EPOCH).ok()?.as_millis();
    i64::try_from(nanos).ok()
}

fn ms_to_offset(ms: i64, local_offset: UtcOffset) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()
        .map(|time| time.to_offset(local_offset))
}

/// A protobuf `{#1: seconds, #2: nanos}` Timestamp → epoch milliseconds.
fn proto_timestamp_ms(ts: &[u8]) -> Option<i64> {
    let seconds = i64::try_from(varint_field(ts, 1)?).ok()?;
    let nanos = i64::try_from(varint_field(ts, 2).unwrap_or(0)).ok()?;
    seconds.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}

// ── Minimal protobuf wire reader (no prost / schema) ───────────────────────

enum Wire<'a> {
    Varint(u64),
    Len(&'a [u8]),
}

struct ProtoReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ProtoReader<'a> {
    fn read_varint(&mut self) -> Option<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *self.buf.get(self.pos)?;
            self.pos += 1;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
    }

    fn next_field(&mut self) -> Option<(u64, Wire<'a>)> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let tag = self.read_varint()?;
        let wire = match tag & 0x7 {
            0 => Wire::Varint(self.read_varint()?),
            1 => {
                self.pos = self.pos.checked_add(8).filter(|&p| p <= self.buf.len())?;
                return Some((tag >> 3, Wire::Varint(0))); // fixed64, value unused
            }
            2 => {
                let len = usize::try_from(self.read_varint()?).ok()?;
                let end = self.pos.checked_add(len).filter(|&p| p <= self.buf.len())?;
                let bytes = &self.buf[self.pos..end];
                self.pos = end;
                Wire::Len(bytes)
            }
            5 => {
                self.pos = self.pos.checked_add(4).filter(|&p| p <= self.buf.len())?;
                return Some((tag >> 3, Wire::Varint(0))); // fixed32, value unused
            }
            _ => return None,
        };
        Some((tag >> 3, wire))
    }
}

fn message_field(buf: &[u8], field: u64) -> Option<&[u8]> {
    let mut reader = ProtoReader { buf, pos: 0 };
    while let Some((found, wire)) = reader.next_field() {
        if found == field
            && let Wire::Len(bytes) = wire
        {
            return Some(bytes);
        }
    }
    None
}

fn varint_field(buf: &[u8], field: u64) -> Option<u64> {
    let mut reader = ProtoReader { buf, pos: 0 };
    while let Some((found, wire)) = reader.next_field() {
        if found == field
            && let Wire::Varint(value) = wire
        {
            return Some(value);
        }
    }
    None
}

fn string_field(buf: &[u8], field: u64) -> Option<&str> {
    message_field(buf, field).and_then(|bytes| std::str::from_utf8(bytes).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(field: u64, value: u64) -> Vec<u8> {
        let mut out = enc(field << 3);
        out.extend(enc(value));
        out
    }
    fn lenf(field: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = enc((field << 3) | 2);
        out.extend(enc(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }
    fn enc(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn gen_blob(
        system: u64,
        input: u64,
        cache: u64,
        output: u64,
        think: u64,
        total: Option<u64>,
        resp_id: &str,
    ) -> Vec<u8> {
        let mut usage = Vec::new();
        usage.extend(varint(1, system));
        usage.extend(varint(2, input));
        if let Some(total) = total {
            usage.extend(varint(3, total));
        }
        usage.extend(varint(5, cache));
        usage.extend(varint(9, output));
        usage.extend(varint(10, think));
        usage.extend(lenf(11, resp_id.as_bytes()));
        let mut chat = Vec::new();
        chat.extend(lenf(4, &usage));
        chat.extend(lenf(19, b"gemini-3-flash"));
        lenf(1, &chat)
    }

    fn parse(blob: &[u8]) -> ParseGen {
        parse_gen(blob, "s1", 1, None, UtcOffset::UTC, &mut HashSet::new())
    }

    #[test]
    fn maps_tokens_and_model() {
        let blob = gen_blob(1132, 500, 16000, 300, 40, Some(340), "r1");
        let ParseGen::Event(e) = parse_gen(
            &blob,
            "s1",
            1_700_000_000_000,
            Some("/x/proj"),
            UtcOffset::UTC,
            &mut HashSet::new(),
        ) else {
            panic!("expected event");
        };
        assert_eq!(e.usage.input_tokens, 1632); // 1132 + 500
        assert_eq!(e.usage.cache_read_input_tokens, 16000);
        assert_eq!(e.usage.output_tokens, 340); // 300 + 40 (thinking folded in)
        assert_eq!(e.usage.reasoning_output_tokens, 40);
        assert_eq!(e.model.as_deref(), Some("gemini-3-flash"));
    }

    #[test]
    fn self_verify_rejects_drifted_total() {
        // #3 (999) != #9 + #10 (300 + 40) → layout drift → Drift, not garbage.
        let blob = gen_blob(1132, 500, 0, 300, 40, Some(999), "r1");
        assert!(matches!(parse(&blob), ParseGen::Drift));
    }

    #[test]
    fn missing_output_total_is_skipped_quietly() {
        // No #3 at all → can't self-verify → skip (not trusted), but quietly:
        // some healthy rows omit it, so it's not an error.
        let blob = gen_blob(1132, 500, 0, 300, 40, None, "r1");
        assert!(matches!(parse(&blob), ParseGen::Empty));
    }

    #[test]
    fn duplicate_response_id_is_skipped() {
        let blob = gen_blob(1132, 500, 0, 300, 40, Some(340), "dup");
        let mut seen = HashSet::new();
        assert!(matches!(
            parse_gen(&blob, "s1", 1, None, UtcOffset::UTC, &mut seen),
            ParseGen::Event(_)
        ));
        // Same response id again (a retry) → not counted twice.
        assert!(matches!(
            parse_gen(&blob, "s1", 1, None, UtcOffset::UTC, &mut seen),
            ParseGen::Empty
        ));
    }

    #[test]
    fn all_zero_is_empty() {
        let blob = gen_blob(0, 0, 0, 0, 0, Some(0), "r1");
        assert!(matches!(parse(&blob), ParseGen::Empty));
    }

    #[test]
    fn windows_file_uri_drops_the_drive_slash() {
        assert_eq!(
            file_uri_to_path("file:///C:/Users/me/repo"),
            "C:/Users/me/repo"
        );
        assert_eq!(file_uri_to_path("file:///Users/me/repo"), "/Users/me/repo");
    }
}
