//! With no public schema, read the wire format directly and pull only the needed fields.
//! Field numbers follow the usage message in the protobuf descriptor embedded in
//! the agy binary, cross-checked against the `tokscale` project (its #1610 /
//! #1611 identified `#1` as the model enum). `#1` is not a token count: reading
//! it as system-prompt tokens added the enum value to every row's input.
//! Because the numbers are unofficial and could shift on an Antigravity update,
//! every row is self-verified against the precomputed output total `#3` (fields:
//! `#3` output total, `#9` thinking, `#10` response text, `#11` response id): it
//! must equal `#9 + #10`. A mismatch means the layout drifted → the row is dropped
//! and counted as a parse error. A row that omits `#3` can't be verified, so it's
//! skipped too, but quietly (some healthy rows omit it — not an error).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use time::{OffsetDateTime, UtcOffset};

use crate::collector::project_from_cwd;
use crate::collector::store::{Store, open_store, read_deadline};
use crate::model::{ScanStats, SourceKind, TokenUsage, UsageEvent};
use crate::timestamp::{from_unix_millis, to_local};

use label::{display_name_label, selected_models, served_model_label};
use proto::{message_field, proto_timestamp_ms, string_field, varint_field};
use trajectory::trajectory_meta;

pub(in crate::collector::agy) use trajectory::Subagent;

mod label;
mod proto;
mod trajectory;

pub(super) struct Usage {
    pub(super) events: Vec<UsageEvent>,
    pub(super) subagents: HashMap<String, Subagent>,
}

pub(super) fn collect_usage(
    stores: &[PathBuf],
    mtime_floor: Option<SystemTime>,
    local_offset: UtcOffset,
    stats: &mut ScanStats,
) -> Usage {
    let floor = mtime_floor.and_then(systemtime_to_ms);
    let mut dbs = Vec::new();
    for store in stores {
        let Ok(entries) = std::fs::read_dir(store.join("conversations")) else {
            continue;
        };
        let mut store_dbs: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("db"))
            })
            .collect();
        store_dbs.sort();
        dbs.extend(store_dbs);
    }

    let mut generations = Generations::default();
    let mut subagents = HashMap::new();
    let deadline = read_deadline();
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
        let read = parse_db(&db, newest, deadline, local_offset, stats, &mut generations);
        if let Some((conversation_id, subagent)) = read {
            subagents.insert(conversation_id, subagent);
        }
    }
    // Apply the window only after every copy has been weighed: filtering per row
    // lets an out-of-window copy (dated from its conversation start) claim the
    // id and shadow an in-window copy from the other store.
    let events = generations
        .rows
        .into_iter()
        .map(|(_, event)| attributed(event, &subagents))
        .filter(|event| {
            !floor.is_some_and(|floor| {
                event
                    .timestamp
                    .is_some_and(|ts| ts.unix_timestamp_nanos() / 1_000_000 < i128::from(floor))
            })
        })
        .collect();
    Usage { events, subagents }
}

/// Attributed once every copy is read, by the conversation a row leaves
/// `parse_gen` under: only one store's copy may name the parent, and when the
/// other copy won on time its row stayed Main in a session of its own.
fn attributed(mut event: UsageEvent, subagents: &HashMap<String, Subagent>) -> UsageEvent {
    if let Some(subagent) = event.session_id.as_ref().and_then(|id| subagents.get(id)) {
        event.session_id = Some(subagent.session_id.clone());
        event.source_kind = SourceKind::Subagent;
        event.attribution_agent.clone_from(&subagent.name);
    }
    event
}

/// How a row's time was found, weakest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TimeSource {
    FileMtime,
    ConversationStart,
    Step,
    Row,
}

/// One generation per key across every DB of every store, so neither a retried
/// generation nor a conversation present in two stores double-counts. The copy
/// with the strongest time evidence wins, so the result doesn't depend on which
/// store was scanned first; on a tie the first copy stays.
#[derive(Default)]
struct Generations {
    index: HashMap<String, usize>,
    rows: Vec<(TimeSource, UsageEvent)>,
}

impl Generations {
    fn offer(&mut self, key: String, source: TimeSource, event: UsageEvent) {
        match self.index.entry(key) {
            Entry::Occupied(slot) => {
                let row = &mut self.rows[*slot.get()];
                if source > row.0 {
                    *row = (source, event);
                }
            }
            Entry::Vacant(slot) => {
                slot.insert(self.rows.len());
                self.rows.push((source, event));
            }
        }
    }
}

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
    db_mtime_ms: Option<i64>,
    deadline: Instant,
    local_offset: UtcOffset,
    stats: &mut ScanStats,
    generations: &mut Generations,
) -> Option<(String, Subagent)> {
    // Skipped unopened: there is nothing to read yet, and a 0-byte file is still
    // in rollback-journal mode, where our read lock would block agy's first write.
    if std::fs::metadata(db).is_ok_and(|meta| meta.len() == 0) {
        return None;
    }
    let Some(store) = open_store(db, deadline) else {
        stats.unreadable_files += 1;
        return None;
    };
    stats.files_seen += 1;
    let conversation_id = db
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("antigravity")
        .to_owned();
    let trajectory = trajectory_meta(&store, &conversation_id);
    // Fallback chain for a row's time: its own stamp → its step's created-at →
    // the conversation's created-at → the DB file mtime (so a missing timestamp
    // dates the event near when the file was written, never 1970).
    let fallback = if trajectory.started_ms > 0 {
        (TimeSource::ConversationStart, trajectory.started_ms)
    } else {
        (TimeSource::FileMtime, db_mtime_ms.unwrap_or(0))
    };
    let subagent = trajectory
        .subagent
        .map(|subagent| (conversation_id.clone(), subagent));

    if !store.has_table("gen_metadata", &["idx", "data"]) {
        return subagent;
    }
    let Ok(mut stmt) = store.prepare("SELECT idx, data FROM gen_metadata ORDER BY idx") else {
        return subagent;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        store.charge(row, 2)?;
        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
    }) else {
        return subagent;
    };
    let mut blobs = Vec::new();
    for row in rows {
        stats.lines_seen += 1;
        match row {
            Ok(blob) => blobs.push(blob),
            Err(_) if store.spent() => {
                stats.parse_errors += 1;
                break;
            }
            Err(_) => stats.parse_errors += 1,
        }
    }
    let conversation = Conversation {
        id: &conversation_id,
        project: trajectory.project.as_deref(),
        fallback,
        selected_models: selected_models(&blobs),
        step_times_ms: step_times_by_response(&store),
    };
    for (idx, blob) in &blobs {
        match parse_gen(blob, *idx, &conversation, local_offset) {
            ParseGen::Event(generation) => {
                let Generation { key, source, event } = *generation;
                generations.offer(key, source, event);
            }
            ParseGen::Empty => {}
            ParseGen::Drift => stats.parse_errors += 1,
        }
    }
    subagent
}

enum ParseGen {
    Event(Box<Generation>),
    Empty,
    Drift,
}

struct Generation {
    key: String,
    source: TimeSource,
    event: UsageEvent,
}

struct Conversation<'a> {
    /// The DB file name. Id-less rows are keyed by it, not by the session: a
    /// subagent shares its parent's session, and the same row index in both
    /// would collide and drop one row's tokens.
    id: &'a str,
    project: Option<&'a str>,
    fallback: (TimeSource, i64),
    selected_models: Vec<String>,
    step_times_ms: HashMap<String, i64>,
}

fn parse_gen(
    blob: &[u8],
    idx: i64,
    conversation: &Conversation<'_>,
    local_offset: UtcOffset,
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
    let input = to_u64(v(2));
    let cache_write = to_u64(v(4));
    let cache_read = to_u64(v(5));
    let thinking = to_u64(v(9));
    let output_text = to_u64(v(10));

    match varint_field(usage, 3) {
        Some(total) if to_u64(total) == output_text.saturating_add(thinking) => {}
        Some(_) => return ParseGen::Drift,
        None => return ParseGen::Empty,
    }

    if input == 0 && output_text == 0 && cache_read == 0 && cache_write == 0 && thinking == 0 {
        return ParseGen::Empty;
    }

    let response_id = string_field(usage, 11).filter(|id| !id.trim().is_empty());
    let key = response_id.map_or_else(|| format!("{}#{idx}", conversation.id), ToOwned::to_owned);

    let (source, timestamp_ms) = message_field(chat_model, 9)
        .and_then(|node| message_field(node, 4))
        .and_then(proto_timestamp_ms)
        .filter(|&ms| plausible_ms(ms))
        .map(|ms| (TimeSource::Row, ms))
        .or_else(|| {
            response_id
                .and_then(|id| conversation.step_times_ms.get(id).copied())
                .map(|ms| (TimeSource::Step, ms))
        })
        .unwrap_or(conversation.fallback);
    let timestamp = ms_to_offset(timestamp_ms, local_offset);

    // Prefer the display name (#21, e.g. "Gemini 3.5 Flash (High)") — it names
    // the selection, while the internal id #19 can be a cryptic,
    // version-mismatched codename like "gemini-3-flash-a". Rows without it
    // fall back to #19, reconciled with the selected model so a routing tag
    // doesn't split one model into several unpriced labels.
    let model = string_field(chat_model, 21)
        .and_then(display_name_label)
        .or_else(|| {
            string_field(chat_model, 19)
                .filter(|served| !served.trim().is_empty())
                .map(|served| served_model_label(served, &conversation.selected_models).to_owned())
        });

    let usage = TokenUsage {
        input_tokens: input,
        // Fold thinking into output so token_volume() counts it (same convention
        // as the OpenCode collector); reasoning is kept separately for display.
        output_tokens: output_text.saturating_add(thinking),
        reasoning_output_tokens: thinking,
        cache_creation_input_tokens: cache_write,
        cache_read_input_tokens: cache_read,
        ..TokenUsage::default()
    };
    let event = UsageEvent {
        timestamp,
        session_id: Some(conversation.id.to_owned()),
        model,
        source_kind: SourceKind::Main,
        attribution_agent: None,
        attribution_skill: None,
        project: conversation.project.and_then(project_from_cwd),
        usage,
        granularity: crate::model::UsageGranularity::Call,
        billing: crate::model::Billing::default(),
    };
    ParseGen::Event(Box::new(Generation { key, source, event }))
}

/// Keyed by response id, not matched by position: some conversations hold
/// more planner steps than gen rows, so the n-th step is not the n-th row.
fn step_times_by_response(store: &Store) -> HashMap<String, i64> {
    let mut times = HashMap::new();
    if !store.has_table("steps", &["idx", "metadata"]) {
        return times;
    }
    let Ok(mut stmt) = store.prepare("SELECT metadata FROM steps ORDER BY idx") else {
        return times;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        store.charge(row, 1)?;
        row.get::<_, Option<Vec<u8>>>(0)
    }) else {
        return times;
    };
    for metadata in rows.flatten().flatten() {
        let Some(id) = message_field(&metadata, 9)
            .and_then(|usage| string_field(usage, 11))
            .filter(|id| !id.trim().is_empty())
        else {
            continue;
        };
        let Some(ms) = message_field(&metadata, 1)
            .and_then(proto_timestamp_ms)
            .filter(|&ms| plausible_ms(ms))
        else {
            continue;
        };
        times.entry(id.to_owned()).or_insert(ms);
    }
    times
}

fn systemtime_to_ms(time: SystemTime) -> Option<i64> {
    let nanos = time.duration_since(UNIX_EPOCH).ok()?.as_millis();
    i64::try_from(nanos).ok()
}

fn ms_to_offset(ms: i64, local_offset: UtcOffset) -> Option<OffsetDateTime> {
    to_local(from_unix_millis(ms)?, local_offset)
}

/// A stamp past the plausible range is treated like a missing one, so the
/// row falls back to the next source instead of going undated.
pub(in crate::collector::agy) fn plausible_ms(ms: i64) -> bool {
    ms > 0 && from_unix_millis(ms).is_some()
}

#[cfg(test)]
pub(super) mod tests {
    use std::time::Duration;

    use rusqlite::Connection;

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
        model_enum: u64,
        input: u64,
        cache: u64,
        text: u64,
        think: u64,
        total: Option<u64>,
        resp_id: &str,
    ) -> Vec<u8> {
        let mut usage = Vec::new();
        usage.extend(varint(1, model_enum));
        usage.extend(varint(2, input));
        if let Some(total) = total {
            usage.extend(varint(3, total));
        }
        usage.extend(varint(5, cache));
        usage.extend(varint(9, think));
        usage.extend(varint(10, text));
        usage.extend(lenf(11, resp_id.as_bytes()));
        let mut chat = varint(3, model_enum);
        chat.extend(lenf(4, &usage));
        chat.extend(lenf(19, b"gemini-3-flash"));
        lenf(1, &chat)
    }

    fn conversation(fallback_ts_ms: i64) -> Conversation<'static> {
        Conversation {
            id: "s1",
            project: None,
            fallback: (TimeSource::ConversationStart, fallback_ts_ms),
            selected_models: Vec::new(),
            step_times_ms: HashMap::new(),
        }
    }

    fn parse(blob: &[u8]) -> ParseGen {
        parse_gen(blob, 0, &conversation(1), UtcOffset::UTC)
    }

    fn event_of(result: ParseGen) -> UsageEvent {
        let ParseGen::Event(generation) = result else {
            panic!("expected event");
        };
        generation.event
    }

    #[test]
    fn maps_tokens_and_model() {
        let blob = gen_blob(1132, 500, 16000, 300, 40, Some(340), "r1");
        let conversation = Conversation {
            project: Some("/x/proj"),
            ..conversation(1_700_000_000_000)
        };
        let e = event_of(parse_gen(&blob, 0, &conversation, UtcOffset::UTC));
        assert_eq!(e.usage.input_tokens, 500);
        assert_eq!(e.usage.cache_read_input_tokens, 16000);
        assert_eq!(e.usage.cache_creation_input_tokens, 0);
        assert_eq!(e.usage.output_tokens, 340); // 300 + 40 (thinking folded in)
        assert_eq!(e.usage.reasoning_output_tokens, 40);
        assert_eq!(e.model.as_deref(), Some("gemini-3-flash"));
    }

    #[test]
    fn model_enum_alone_is_empty() {
        let blob = gen_blob(1532, 0, 0, 0, 0, Some(0), "r1");
        assert!(matches!(parse(&blob), ParseGen::Empty));
    }

    #[test]
    fn cache_write_counts_as_cache_creation() {
        let mut usage = varint(1, 1532);
        usage.extend(varint(2, 100));
        usage.extend(varint(3, 10));
        usage.extend(varint(4, 7000));
        usage.extend(varint(10, 10));
        usage.extend(lenf(11, b"r1"));
        let e = event_of(parse(&lenf(1, &lenf(4, &usage))));
        assert_eq!(e.usage.input_tokens, 100);
        assert_eq!(e.usage.cache_creation_input_tokens, 7000);
        assert_eq!(e.usage.token_volume(), 7110);

        let mut only_write = varint(3, 0);
        only_write.extend(varint(4, 7000));
        only_write.extend(lenf(11, b"r2"));
        let e = event_of(parse(&lenf(1, &lenf(4, &only_write))));
        assert_eq!(e.usage.cache_creation_input_tokens, 7000);
    }

    #[test]
    fn prefers_display_name_over_internal_id() {
        let mut usage = Vec::new();
        usage.extend(varint(9, 10));
        usage.extend(varint(10, 0));
        usage.extend(varint(3, 10));
        usage.extend(lenf(11, b"r9"));
        let mut chat = Vec::new();
        chat.extend(lenf(4, &usage));
        chat.extend(lenf(19, b"gemini-3-flash-a"));
        chat.extend(lenf(21, b"Gemini 3.5 Flash (High)"));
        let blob = lenf(1, &chat);
        let e = event_of(parse(&blob));
        assert_eq!(e.model.as_deref(), Some("gemini-3.5-flash"));
    }

    #[test]
    fn rows_with_and_without_a_display_name_share_one_label() {
        let mut with_name = served_only_gen_blob("gemini-3.8-flash-tiered", "r1", None);
        let chat = message_field(&with_name, 1).expect("generation").to_vec();
        let mut named = chat.clone();
        named.extend(lenf(21, b"Gemini 3.8 Flash (Low)"));
        with_name = lenf(1, &named);
        let without_name = served_only_gen_blob("gemini-3.8-flash-tiered", "r2", None);
        let conversation = Conversation {
            selected_models: vec!["gemini-3.8-flash-tiered".to_owned()],
            ..conversation(1)
        };
        let labels: Vec<_> = [with_name, without_name]
            .iter()
            .map(|blob| event_of(parse_gen(blob, 0, &conversation, UtcOffset::UTC)).model)
            .collect();
        assert_eq!(
            labels,
            [
                Some("gemini-3.8-flash".to_owned()),
                Some("gemini-3.8-flash".to_owned())
            ]
        );
    }

    #[test]
    fn blank_or_unusable_display_name_falls_back_to_the_served_id() {
        for display in [&b" (Medium)"[..], b"Gemini private-repo\t(High)"] {
            let mut usage = varint(3, 10);
            usage.extend(varint(10, 10));
            usage.extend(lenf(11, b"r1"));
            let mut chat = lenf(4, &usage);
            chat.extend(lenf(19, b"gemini-4-argon"));
            chat.extend(lenf(21, display));
            let e = event_of(parse(&lenf(1, &chat)));
            assert_eq!(e.model.as_deref(), Some("gemini-4-argon"));
        }
    }

    #[test]
    fn self_verify_rejects_drifted_total() {
        let blob = gen_blob(1132, 500, 0, 300, 40, Some(999), "r1");
        assert!(matches!(parse(&blob), ParseGen::Drift));
    }

    #[test]
    fn missing_output_total_is_skipped_quietly() {
        let blob = gen_blob(1132, 500, 0, 300, 40, None, "r1");
        assert!(matches!(parse(&blob), ParseGen::Empty));
    }

    #[test]
    fn duplicate_response_id_is_skipped() {
        let blob = gen_blob(1132, 500, 0, 300, 40, Some(340), "dup");
        let mut generations = Generations::default();
        for _ in 0..2 {
            let ParseGen::Event(generation) = parse(&blob) else {
                panic!("expected event");
            };
            generations.offer(generation.key, generation.source, generation.event);
        }
        assert_eq!(generations.rows.len(), 1);
    }

    #[test]
    fn all_zero_is_empty() {
        let blob = gen_blob(0, 0, 0, 0, 0, Some(0), "r1");
        assert!(matches!(parse(&blob), ParseGen::Empty));
    }

    fn timestamp(ms: i64) -> Vec<u8> {
        let secs = u64::try_from(ms / 1000).expect("test timestamp should be positive");
        let mut ts = varint(1, secs);
        ts.extend(varint(2, 0));
        ts
    }

    /// A row without a display name (#21) or its own timestamp (#9.#4), whose
    /// #9 carries context-window metadata that must not count as tokens.
    pub(in crate::collector::agy) fn served_only_gen_blob(
        served: &str,
        resp_id: &str,
        selected: Option<&str>,
    ) -> Vec<u8> {
        let mut usage = Vec::new();
        usage.extend(varint(1, 1532));
        usage.extend(varint(2, 200));
        usage.extend(varint(3, 70));
        usage.extend(varint(5, 4000));
        usage.extend(varint(9, 50));
        usage.extend(varint(10, 20));
        usage.extend(lenf(11, resp_id.as_bytes()));
        let mut context = varint(1, 123_456);
        context.extend(varint(4, 256_000));
        let mut node = varint(2, 1);
        node.extend(lenf(10, &context));
        let mut chat = Vec::new();
        chat.extend(lenf(4, &usage));
        chat.extend(lenf(9, &node));
        chat.extend(lenf(19, served.as_bytes()));
        let mut blob = lenf(1, &chat);
        if let Some(selected) = selected {
            blob.extend(lenf(3, &lenf(28, selected.as_bytes())));
        }
        blob
    }

    pub(in crate::collector::agy) fn step_metadata(created_ms: i64, resp_id: &str) -> Vec<u8> {
        let mut metadata = lenf(1, &timestamp(created_ms));
        metadata.extend(lenf(9, &lenf(11, resp_id.as_bytes())));
        metadata
    }

    #[test]
    fn served_only_row_dates_from_its_step_and_ignores_context_metadata() {
        let blob = served_only_gen_blob("gemini-3.8-flash-n", "r1", None);
        let conversation = Conversation {
            selected_models: vec!["gemini-3.8-flash-high".to_owned()],
            step_times_ms: HashMap::from([("r1".to_owned(), 1_790_000_000_000)]),
            ..conversation(1_700_000_000_000)
        };
        let e = event_of(parse_gen(&blob, 0, &conversation, UtcOffset::UTC));
        assert_eq!(
            e.timestamp.map(OffsetDateTime::unix_timestamp),
            Some(1_790_000_000)
        );
        assert_eq!(e.model.as_deref(), Some("gemini-3.8-flash"));
        assert_eq!(e.usage.input_tokens, 200);
        assert_eq!(e.usage.output_tokens, 70);
        assert_eq!(e.usage.reasoning_output_tokens, 50);
        assert_eq!(e.usage.cache_read_input_tokens, 4000);
        assert_eq!(e.usage.token_volume(), 4270);
    }

    #[test]
    fn row_without_its_step_falls_back_to_the_conversation_start() {
        let blob = served_only_gen_blob("gemini-3.8-flash", "r1", None);
        let e = event_of(parse_gen(
            &blob,
            0,
            &conversation(1_700_000_000_000),
            UtcOffset::UTC,
        ));
        assert_eq!(
            e.timestamp.map(OffsetDateTime::unix_timestamp),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn own_timestamp_wins_over_the_step_time() {
        let mut usage = varint(3, 10);
        usage.extend(varint(9, 10));
        usage.extend(lenf(11, b"r1"));
        let mut chat = lenf(4, &usage);
        chat.extend(lenf(9, &lenf(4, &timestamp(1_780_000_000_000))));
        let blob = lenf(1, &chat);
        let conversation = Conversation {
            step_times_ms: HashMap::from([("r1".to_owned(), 1_790_000_000_000)]),
            ..conversation(1)
        };
        let e = event_of(parse_gen(&blob, 0, &conversation, UtcOffset::UTC));
        assert_eq!(
            e.timestamp.map(OffsetDateTime::unix_timestamp),
            Some(1_780_000_000)
        );
    }

    #[test]
    fn row_stamp_past_the_plausible_range_falls_back_to_the_step_time() {
        let mut usage = varint(3, 10);
        usage.extend(varint(9, 10));
        usage.extend(lenf(11, b"r1"));
        let mut chat = lenf(4, &usage);
        chat.extend(lenf(9, &lenf(4, &timestamp(253_402_290_000_000))));
        let blob = lenf(1, &chat);
        let conversation = Conversation {
            step_times_ms: HashMap::from([("r1".to_owned(), 1_790_000_000_000)]),
            ..conversation(1)
        };

        let tokyo = UtcOffset::from_hms(9, 0, 0).expect("offset");
        let e = event_of(parse_gen(&blob, 0, &conversation, tokyo));

        assert_eq!(
            e.timestamp.map(OffsetDateTime::unix_timestamp),
            Some(1_790_000_000)
        );
    }

    fn write_conversation(path: &Path, gens: &[Vec<u8>]) {
        let conn = Connection::open(path).expect("fixture db should open");
        conn.execute_batch("CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);")
            .expect("fixture schema should be created");
        for (idx, blob) in (0_i64..).zip(gens) {
            conn.execute(
                "INSERT INTO gen_metadata VALUES (?1, ?2)",
                rusqlite::params![idx, blob],
            )
            .expect("gen row should insert");
        }
    }

    pub(in crate::collector::agy) fn write_dated_conversation(
        path: &Path,
        started_ms: i64,
        gens: &[Vec<u8>],
        steps: &[Vec<u8>],
    ) {
        write_trajectory_conversation(path, &lenf(2, &timestamp(started_ms)), gens, steps);
    }

    pub(in crate::collector::agy) fn subagent_trajectory(
        started_ms: i64,
        parent: &str,
        root: &str,
        name: &str,
    ) -> Vec<u8> {
        let mut blob = lenf(2, &timestamp(started_ms));
        blob.extend(lenf(5, parent.as_bytes()));
        blob.extend(lenf(6, root.as_bytes()));
        blob.extend(lenf(8, &lenf(1, name.as_bytes())));
        blob.extend(varint(17, 1));
        blob
    }

    pub(in crate::collector::agy) fn write_trajectory_conversation(
        path: &Path,
        trajectory: &[u8],
        gens: &[Vec<u8>],
        steps: &[Vec<u8>],
    ) {
        write_conversation(path, gens);
        let conn = Connection::open(path).expect("fixture db should open");
        conn.execute_batch(
            "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, metadata BLOB);
             CREATE TABLE trajectory_metadata_blob (id TEXT PRIMARY KEY, data BLOB);",
        )
        .expect("fixture schema should be created");
        conn.execute(
            "INSERT INTO trajectory_metadata_blob VALUES ('main', ?1)",
            [trajectory],
        )
        .expect("trajectory row should insert");
        for (idx, metadata) in (0_i64..).zip(steps) {
            conn.execute(
                "INSERT INTO steps VALUES (?1, 15, ?2)",
                rusqlite::params![idx, metadata],
            )
            .expect("step row should insert");
        }
    }

    #[test]
    fn copy_with_the_stronger_time_wins_before_the_window_applies() {
        let now = SystemTime::now();
        let now_ms = systemtime_to_ms(now).expect("clock should be past the epoch");
        let floor = now - Duration::from_hours(24);
        let started_ms = now_ms - 30 * 86_400_000;
        let step_ms = now_ms - 3_600_000;
        let temp = tempfile::TempDir::new().expect("test tempdir should be created");
        let cli = temp.path().join("cli");
        let app = temp.path().join("app");
        let row = served_only_gen_blob("gemini-3.8-flash", "r1", None);
        // The CLI copy has no step for r1, so it can only date the row from the
        // conversation start, a month before the window.
        for (store, steps) in [
            (&cli, Vec::new()),
            (&app, vec![step_metadata(step_ms, "r1")]),
        ] {
            std::fs::create_dir_all(store.join("conversations"))
                .expect("conversations dir should be created");
            write_dated_conversation(
                &store.join("conversations").join("c1.db"),
                started_ms,
                std::slice::from_ref(&row),
                &steps,
            );
        }

        for stores in [[cli.clone(), app.clone()], [app.clone(), cli.clone()]] {
            let mut stats = ScanStats::default();
            let events = collect_usage(&stores, Some(floor), UtcOffset::UTC, &mut stats).events;
            let times: Vec<_> = events
                .iter()
                .map(|e| e.timestamp.map(OffsetDateTime::unix_timestamp))
                .collect();
            assert_eq!(times, [Some(step_ms / 1000)], "store order {stores:?}");
        }
    }

    #[test]
    fn conversation_in_both_stores_counts_once() {
        let temp = tempfile::TempDir::new().expect("test tempdir should be created");
        let stores = [temp.path().join("cli"), temp.path().join("app")];
        for store in &stores {
            std::fs::create_dir_all(store.join("conversations"))
                .expect("conversations dir should be created");
        }
        let gen_row = |rid: &str| served_only_gen_blob("gemini-3.8-flash", rid, None);
        // A row with no response id can only be matched by conversation + row index.
        let unkeyed = gen_blob(1532, 10, 0, 5, 0, Some(5), "");
        // The app's copy of c1 has moved one generation further than the CLI's.
        write_conversation(
            &stores[0].join("conversations").join("c1.db"),
            &[unkeyed.clone(), gen_row("r1"), gen_row("r2")],
        );
        write_conversation(
            &stores[1].join("conversations").join("c1.db"),
            &[unkeyed, gen_row("r1"), gen_row("r2"), gen_row("r3")],
        );
        write_conversation(
            &stores[1].join("conversations").join("c2.db"),
            &[gen_row("r4")],
        );

        let mut stats = ScanStats::default();
        let events = collect_usage(&stores, None, UtcOffset::UTC, &mut stats).events;

        assert_eq!(stats.files_seen, 3);
        assert_eq!(stats.parse_errors, 0);
        assert_eq!(events.len(), 5, "c1 rows once each, plus r3 and r4");
        let input: u64 = events.iter().map(|e| e.usage.input_tokens).sum();
        assert_eq!(input, 10 + 4 * 200);
    }

    #[test]
    fn subagent_rows_join_the_top_level_session_and_keep_their_own_keys() {
        let temp = tempfile::TempDir::new().expect("test tempdir should be created");
        let dir = temp.path().join("conversations");
        std::fs::create_dir_all(&dir).expect("conversations dir should be created");
        let started_ms = 1_780_000_000_000;
        // Neither row has a response id, so both are keyed by DB name + index 0.
        let unkeyed = gen_blob(1532, 10, 0, 5, 0, Some(5), "");
        let mut top = lenf(2, &timestamp(started_ms));
        top.extend(lenf(6, b"parent"));
        write_trajectory_conversation(
            &dir.join("parent.db"),
            &top,
            std::slice::from_ref(&unkeyed),
            &[],
        );
        write_trajectory_conversation(
            &dir.join("child.db"),
            &subagent_trajectory(started_ms, "parent", "parent", "research"),
            std::slice::from_ref(&unkeyed),
            &[],
        );

        let mut stats = ScanStats::default();
        let usage = collect_usage(
            &[temp.path().to_path_buf()],
            None,
            UtcOffset::UTC,
            &mut stats,
        );

        let rows: Vec<_> = usage
            .events
            .iter()
            .map(|e| {
                (
                    e.session_id.as_deref(),
                    e.source_kind,
                    e.attribution_agent.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                (Some("parent"), SourceKind::Subagent, Some("research")),
                (Some("parent"), SourceKind::Main, None),
            ]
        );
        assert_eq!(
            usage
                .subagents
                .get("child")
                .map(|sub| sub.session_id.as_str()),
            Some("parent")
        );
        assert!(!usage.subagents.contains_key("parent"));
    }

    /// Only one copy of a subagent conversation may name its parent, and the
    /// other can win on time or, on a tie, by being read first: the row still
    /// counts as the subagent's, in the parent's session.
    #[test]
    fn a_subagent_row_is_attributed_whichever_copy_wins() {
        let temp = tempfile::TempDir::new().expect("test tempdir should be created");
        let started_ms = 1_780_000_000_000;
        let row = served_only_gen_blob("gemini-3.8-flash", "r1", None);
        for (case, steps) in [
            ("stronger", vec![step_metadata(started_ms + 60_000, "r1")]),
            ("tie", Vec::new()),
        ] {
            let plain = temp.path().join(case).join("plain");
            let marked = temp.path().join(case).join("marked");
            for store in [&plain, &marked] {
                std::fs::create_dir_all(store.join("conversations"))
                    .expect("conversations dir should be created");
            }
            write_dated_conversation(
                &plain.join("conversations").join("child.db"),
                started_ms,
                std::slice::from_ref(&row),
                &steps,
            );
            write_trajectory_conversation(
                &marked.join("conversations").join("child.db"),
                &subagent_trajectory(started_ms, "parent", "parent", "research"),
                std::slice::from_ref(&row),
                &[],
            );

            for stores in [[plain.clone(), marked.clone()], [marked, plain]] {
                let mut stats = ScanStats::default();
                let events = collect_usage(&stores, None, UtcOffset::UTC, &mut stats).events;
                let rows: Vec<_> = events
                    .iter()
                    .map(|e| {
                        (
                            e.session_id.as_deref(),
                            e.source_kind,
                            e.attribution_agent.as_deref(),
                        )
                    })
                    .collect();
                assert_eq!(
                    rows,
                    [(Some("parent"), SourceKind::Subagent, Some("research"))],
                    "{case}, store order {stores:?}"
                );
            }
        }
    }

    #[test]
    fn reads_a_live_wal_conversation_and_skips_an_empty_one() {
        let temp = tempfile::TempDir::new().expect("test tempdir should be created");
        let dir = temp.path().join("conversations");
        std::fs::create_dir_all(&dir).expect("conversations dir should be created");
        std::fs::write(dir.join("empty.db"), b"").expect("empty db should be written");

        // Kept open for the whole scan, like a conversation agy is still writing.
        let writer = Connection::open(dir.join("live.db")).expect("fixture db should open");
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);
                 CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, metadata BLOB);
                 CREATE TABLE trajectory_metadata_blob (id TEXT PRIMARY KEY, data BLOB);",
            )
            .expect("fixture schema should be created");
        let started_ms = 1_780_000_000_000;
        writer
            .execute(
                "INSERT INTO trajectory_metadata_blob VALUES ('main', ?1)",
                [lenf(2, &timestamp(started_ms))],
            )
            .expect("trajectory row should insert");
        // The selection rides on a later row than the one it labels.
        let gens = [
            served_only_gen_blob("gemini-4-argon-trial", "r1", None),
            served_only_gen_blob("gemini-4-argon-trial", "r2", Some("gemini-4-argon-medium")),
        ];
        for (idx, blob) in (0_i64..).zip(&gens) {
            writer
                .execute(
                    "INSERT INTO gen_metadata VALUES (?1, ?2)",
                    rusqlite::params![idx, blob],
                )
                .expect("gen row should insert");
        }
        // An extra planner step without a gen row sits between the two, so a
        // positional match would date r2 from the wrong step.
        let steps = [
            (15, step_metadata(started_ms + 60_000, "r1")),
            (15, lenf(1, &timestamp(started_ms + 90_000))),
            (15, step_metadata(started_ms + 3_600_000, "r2")),
        ];
        for (idx, (step_type, metadata)) in (0_i64..).zip(&steps) {
            writer
                .execute(
                    "INSERT INTO steps VALUES (?1, ?2, ?3)",
                    rusqlite::params![idx, step_type, metadata],
                )
                .expect("step row should insert");
        }

        let mut stats = ScanStats::default();
        let events = collect_usage(
            &[temp.path().to_path_buf()],
            None,
            UtcOffset::UTC,
            &mut stats,
        )
        .events;
        drop(writer);

        assert_eq!(stats.files_seen, 1);
        assert_eq!(stats.unreadable_files, 0);
        assert_eq!(stats.parse_errors, 0);
        let times: Vec<_> = events
            .iter()
            .map(|e| e.timestamp.map(OffsetDateTime::unix_timestamp))
            .collect();
        assert_eq!(
            times,
            [Some(1_780_000_060), Some(1_780_003_600)],
            "each row should date from its own step"
        );
        assert!(
            events
                .iter()
                .all(|e| e.model.as_deref() == Some("gemini-4-argon"))
        );
    }
}
