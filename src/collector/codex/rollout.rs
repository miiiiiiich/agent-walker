use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use ruzstd::decoding::StreamingDecoder;

const COMPRESSED_SUFFIX: &str = ".zst";

/// Bounds the memory one line can take; a longer line is skipped as a parse
/// error instead of being buffered.
pub(super) const MAX_LINE_BYTES: u64 = 64 << 20;

/// Decoded bytes a `.zst` may expand to, per on-disk byte: far above what
/// JSONL compresses to, far below what a run-length frame can claim. Past the
/// budget the file stops parsing.
const MAX_DECODE_RATIO: u64 = 64;
/// Floor of the decode budget, so a tiny file's frame overhead never trips it.
const MIN_DECODE_BUDGET: u64 = 64 << 20;

fn is_compressed(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".jsonl.zst"))
}

pub(super) fn is_rollout_file(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "jsonl") || is_compressed(path)
}

pub(super) fn plain_rollout_path(path: &Path) -> PathBuf {
    if !is_compressed(path) {
        return path.to_path_buf();
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(COMPRESSED_SUFFIX))
        .unwrap_or_default();
    path.with_file_name(name)
}

/// Plain beats compressed, as in Codex. The frame header's content size is
/// not compared: it is whatever the file claims, and a forged one would drop
/// the plain file's usage.
pub(super) fn supersedes(candidate: &Path, existing: &Path) -> bool {
    let disk_len = |path: &Path| std::fs::metadata(path).map_or(0, |meta| meta.len());
    match (is_compressed(candidate), is_compressed(existing)) {
        (false, true) => true,
        (true, false) => false,
        _ => disk_len(candidate) > disk_len(existing),
    }
}

pub(super) fn open_rollout(path: &Path) -> Option<Box<dyn BufRead>> {
    let file = File::open(path).ok()?;
    if is_compressed(path) {
        let budget = file
            .metadata()
            .ok()?
            .len()
            .saturating_mul(MAX_DECODE_RATIO)
            .max(MIN_DECODE_BUDGET);
        let decoder = StreamingDecoder::new(BufReader::new(file)).ok()?;
        return Some(Box::new(BufReader::new(DecodeBudget {
            inner: decoder,
            remaining: budget,
        })));
    }
    Some(Box::new(BufReader::new(file)))
}

/// Fails the read once more than `remaining` bytes would come out, so the
/// overrun surfaces as an error rather than a silently truncated file.
struct DecodeBudget<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for DecodeBudget<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            let mut probe = [0_u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(io::Error::other("decoded rollout exceeds its budget")),
            };
        }
        let allowed = usize::try_from(self.remaining).map_or(buf.len(), |left| left.min(buf.len()));
        let read = self.inner.read(&mut buf[..allowed])?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

pub(super) enum Line {
    Text,
    Oversized,
}

/// Reads the next line into `line`; `Ok(None)` at end of input. A line past
/// `max_bytes` is consumed without being kept.
pub(super) fn next_line(
    reader: &mut dyn BufRead,
    line: &mut Vec<u8>,
    max_bytes: u64,
) -> io::Result<Option<Line>> {
    line.clear();
    let read = reader.take(max_bytes + 1).read_until(b'\n', line)?;
    if read == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') && line.len() as u64 > max_bytes {
        line.clear();
        reader.skip_until(b'\n')?;
        return Ok(Some(Line::Oversized));
    }
    Ok(Some(Line::Text))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use time::UtcOffset;

    use super::*;
    use crate::collector::FileEvents;
    use crate::collector::codex::tests::{
        meta_line, record_line, token_count_line, total_volume, usage_json, write_lines,
    };
    use crate::collector::codex::{collect, parse_file};

    const COMPRESSED_PLAIN: &str =
        include_str!("../../../tests/fixtures/codex/rollout-compressed.jsonl");
    const COMPRESSED_ZST: &[u8] =
        include_bytes!("../../../tests/fixtures/codex/rollout-compressed.jsonl.zst");
    /// `resp_a` (1000 + 50) + `resp_b` (1200 + 80) in the fixture.
    const COMPRESSED_TOTAL: u64 = 2_330;

    #[test]
    fn compressed_fixture_decodes_to_its_plain_text() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let path = temp.path().join("rollout-z.jsonl.zst");
        fs::write(&path, COMPRESSED_ZST).expect("fixture should be written");

        let mut decoded = Vec::new();
        open_rollout(&path)
            .expect("compressed rollout should open")
            .read_to_end(&mut decoded)
            .expect("compressed rollout should decode");

        assert_eq!(decoded, COMPRESSED_PLAIN.as_bytes());
    }

    #[test]
    fn reads_compressed_rollouts() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(day.join("rollout-z.jsonl.zst"), COMPRESSED_ZST)
            .expect("fixture should be written");
        fs::write(day.join("rollout-z.jsonl.zst.tmp"), b"partial")
            .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.usage_events.len(), 2);
        assert_eq!(total_volume(&collection), COMPRESSED_TOTAL);
        assert_eq!(collection.tool_events[0].tool_name, "cargo");
        assert_eq!(collection.duration_events.len(), 1);
    }

    /// The parse cache stores events per physical path, so the two
    /// representations must produce the same keys: when Codex compresses or
    /// restores a file between runs, the merge then sees one rollout, not two.
    #[test]
    fn plain_and_compressed_representations_yield_identical_keys() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let plain = temp.path().join("a/rollout-z.jsonl");
        let compressed = temp.path().join("b/rollout-z.jsonl.zst");
        fs::create_dir_all(plain.parent().expect("has parent")).expect("dirs");
        fs::create_dir_all(compressed.parent().expect("has parent")).expect("dirs");
        fs::write(&plain, COMPRESSED_PLAIN).expect("fixture should be written");
        fs::write(&compressed, COMPRESSED_ZST).expect("fixture should be written");

        let plain = parse_file(&plain, UtcOffset::UTC).expect("plain parses");
        let compressed = parse_file(&compressed, UtcOffset::UTC).expect("zst parses");

        let usage_keys = |events: &FileEvents| -> Vec<Option<String>> {
            events.usage_events.iter().map(|e| e.key.clone()).collect()
        };
        let tool_keys = |events: &FileEvents| -> Vec<Option<String>> {
            events.tool_events.iter().map(|e| e.key.clone()).collect()
        };
        let limit_keys = |events: &FileEvents| -> Vec<Option<String>> {
            events
                .rate_limit_samples
                .iter()
                .map(|e| e.key.clone())
                .collect()
        };
        assert_eq!(usage_keys(&plain), usage_keys(&compressed));
        assert_eq!(tool_keys(&plain), tool_keys(&compressed));
        assert_eq!(limit_keys(&plain), limit_keys(&compressed));
        assert_eq!(
            plain.session_touches.len(),
            compressed.session_touches.len()
        );
        assert_eq!(plain.lines_seen, compressed.lines_seen);
    }

    #[test]
    fn flipping_representation_between_runs_counts_the_session_once() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        let plain = day.join("rollout-z.jsonl");
        let compressed = day.join("rollout-z.jsonl.zst");
        let run = || collect(temp.path(), None, false, UtcOffset::UTC);

        fs::write(&plain, COMPRESSED_PLAIN).expect("fixture should be written");
        let before = run();
        fs::write(&compressed, COMPRESSED_ZST).expect("fixture should be written");
        let both = run();
        fs::remove_file(&plain).expect("plain should be removed");
        let after = run();

        for collection in [&before, &both, &after] {
            assert_eq!(collection.stats.files_seen, 1);
            assert_eq!(collection.usage_events.len(), 2);
            assert_eq!(total_volume(collection), COMPRESSED_TOTAL);
            assert_eq!(collection.duration_events.len(), 1);
            assert_eq!(collection.tool_events.len(), 1);
        }
    }

    /// A plain file beside its `.zst` wins, including the turn appended to it.
    #[test]
    fn resumed_plain_rollout_supersedes_its_compressed_sibling() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let day = temp.path().join("2026/06/01");
        fs::create_dir_all(&day).expect("test dirs should be created");
        fs::write(day.join("rollout-z.jsonl.zst"), COMPRESSED_ZST)
            .expect("fixture should be written");
        let resumed = format!(
            "{COMPRESSED_PLAIN}{}\n",
            record_line(
                "2026-06-01T01:00:00Z",
                "z1",
                "resp_c",
                &usage_json(300, 0, 20)
            )
        );
        fs::write(day.join("rollout-z.jsonl"), resumed).expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.usage_events.len(), 3);
        assert_eq!(total_volume(&collection), COMPRESSED_TOTAL + 320);
    }

    #[test]
    fn identical_plain_and_compressed_prefer_plain() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let plain = temp.path().join("rollout-z.jsonl");
        let compressed = temp.path().join("rollout-z.jsonl.zst");
        fs::write(&plain, COMPRESSED_PLAIN).expect("fixture should be written");
        fs::write(&compressed, COMPRESSED_ZST).expect("fixture should be written");

        assert!(supersedes(&plain, &compressed));
        assert!(!supersedes(&compressed, &plain));
    }

    /// Neither the frame header's claimed size nor the on-disk sizes decide:
    /// a plain copy smaller than the `.zst` itself still wins, in another
    /// directory too.
    #[test]
    fn plain_copy_wins_over_compressed_even_when_smaller() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let plain = temp.path().join("sessions/2026/06/01/rollout-z.jsonl");
        write_lines(
            &plain,
            &[
                meta_line("2026-06-01T00:00:00Z", "z1", r#""exec""#, ""),
                token_count_line("2026-06-01T00:00:03Z", &usage_json(100, 0, 10), 110),
            ],
        );
        let archived_day = temp.path().join("archived_sessions/2026/06/01");
        fs::create_dir_all(&archived_day).expect("test dirs should be created");
        fs::write(archived_day.join("rollout-z.jsonl.zst"), COMPRESSED_ZST)
            .expect("fixture should be written");
        assert!(fs::metadata(&plain).expect("plain exists").len() < COMPRESSED_ZST.len() as u64);

        let collection = collect(&temp.path().join("sessions"), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(total_volume(&collection), 110);
    }

    /// A zstd frame of raw and run-length blocks: `prefix`, `run` copies of
    /// `byte`, then `suffix`, at four bytes per 128 KiB of run.
    fn run_length_frame(prefix: &[u8], byte: u8, run: usize, suffix: &[u8]) -> Vec<u8> {
        const BLOCK: usize = 128 << 10;
        fn header(frame: &mut Vec<u8>, size: usize, kind: u32, last: bool) {
            let size = u32::try_from(size).expect("block size fits");
            let value = (size << 3) | (kind << 1) | u32::from(last);
            frame.extend_from_slice(&value.to_le_bytes()[..3]);
        }
        // Magic number, descriptor without content size, 128 KiB window.
        let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x38];
        header(&mut frame, prefix.len(), 0, false);
        frame.extend_from_slice(prefix);
        let mut left = run;
        while left > 0 {
            let size = left.min(BLOCK);
            header(&mut frame, size, 1, false);
            frame.push(byte);
            left -= size;
        }
        header(&mut frame, suffix.len(), 0, true);
        frame.extend_from_slice(suffix);
        frame
    }

    #[test]
    fn oversized_line_is_skipped_without_being_buffered() {
        let usage_line = format!(
            "{}\n",
            token_count_line("2026-06-01T00:00:03Z", &usage_json(100, 0, 10), 110)
        );
        let frame = run_length_frame(
            br#"{"pad":""#,
            b'a',
            1 << 20,
            format!("\"}}\n{usage_line}").as_bytes(),
        );
        let mut reader = BufReader::new(
            StreamingDecoder::new(frame.as_slice()).expect("frame header should parse"),
        );
        let mut line = Vec::new();

        let first = next_line(&mut reader, &mut line, 64 << 10).expect("decodes");
        assert!(matches!(first, Some(Line::Oversized)));
        assert!(line.capacity() < 1 << 20);
        let second = next_line(&mut reader, &mut line, 64 << 10).expect("decodes");
        assert!(matches!(second, Some(Line::Text)));
        assert_eq!(line, usage_line.as_bytes());
        assert!(
            next_line(&mut reader, &mut line, 64 << 10)
                .expect("decodes")
                .is_none()
        );
    }

    /// A few kilobytes of run-length blocks claim more than the decode
    /// budget; parsing stops with a parse error, keeping the rows before it.
    #[test]
    fn decompression_bomb_stops_at_the_decode_budget() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let usage_line = format!(
            "{}\n{}\n",
            meta_line("2026-06-01T00:00:00Z", "s1", r#""cli""#, ""),
            token_count_line("2026-06-01T00:00:03Z", &usage_json(100, 0, 10), 110)
        );
        let after_budget = format!(
            "\n{}\n",
            token_count_line("2026-06-01T00:00:09Z", &usage_json(200, 0, 20), 330)
        );
        let run = usize::try_from(MIN_DECODE_BUDGET).expect("budget fits");
        let frame = run_length_frame(usage_line.as_bytes(), b'a', run, after_budget.as_bytes());
        assert!(frame.len() < 8 << 10);
        let path = temp.path().join("rollout-s1.jsonl.zst");
        fs::write(&path, frame).expect("fixture should be written");

        let events = parse_file(&path, UtcOffset::UTC).expect("frame header should parse");

        assert_eq!(events.usage_events.len(), 1);
        assert_eq!(events.parse_errors, 1);
    }

    #[test]
    fn truncated_compressed_rollout_stops_with_a_parse_error() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let path = temp.path().join("rollout-z.jsonl.zst");
        fs::write(&path, &COMPRESSED_ZST[..COMPRESSED_ZST.len() / 2])
            .expect("fixture should be written");

        let events = parse_file(&path, UtcOffset::UTC).expect("frame header is intact");

        assert_eq!(events.parse_errors, 1);
        assert!(events.usage_events.is_empty());
    }

    #[test]
    fn unreadable_compressed_header_counts_as_unreadable_file() {
        let temp = TempDir::new().expect("test tempdir should be created");
        fs::write(temp.path().join("rollout-z.jsonl.zst"), b"not zstd")
            .expect("fixture should be written");

        let collection = collect(temp.path(), None, false, UtcOffset::UTC);

        assert_eq!(collection.stats.files_seen, 1);
        assert_eq!(collection.stats.unreadable_files, 1);
    }
}
