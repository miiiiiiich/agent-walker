//! Bounded reading of agent log files, shared by every line-based collector.

use std::fs::File;
use std::io::{self, BufRead, Read};
use std::path::Path;

use super::FileEvents;

/// Bounds the memory one line can take; a longer line is skipped as a parse
/// error instead of being buffered. Lines that grow this long are tool output
/// (a command's output, a tool result, an image), which carries no usage or
/// tool call; the lines that do stay far below it.
pub(in crate::collector) const MAX_LINE_BYTES: u64 = 64 << 20;

/// Opens `path` only when it resolves to a regular file. A FIFO blocks the
/// open forever and a device streams without end (a `.jsonl` symlinked to
/// `/dev/zero` grew to gigabytes), so neither is read. Symlinks are followed:
/// dotfile managers and synced folders link real logs, and a regular target
/// is as finite as a plain file. The type is checked on the opened handle;
/// checking the path first left a window in which a FIFO swapped in blocked
/// the open. The file is read to its end, so lines an agent appends while it
/// is read still count.
pub(in crate::collector) fn open_regular(path: &Path) -> io::Result<File> {
    // Skips opening a device at all in the common case.
    if !std::fs::metadata(path)?.is_file() {
        return Err(not_regular());
    }
    open_checked(path)
}

/// The part of `open_regular` that holds whatever the path names by the time
/// it is opened.
fn open_checked(path: &Path) -> io::Result<File> {
    let file = open_nonblocking(path)?;
    if !file.metadata()?.is_file() {
        return Err(not_regular());
    }
    Ok(file)
}

/// Opening a FIFO without a writer returns at once under `O_NONBLOCK`; the
/// flag stays on the handle, which is harmless because reads of a regular
/// file never block.
#[cfg(unix)]
fn open_nonblocking(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_nonblocking(path: &Path) -> io::Result<File> {
    File::open(path)
}

fn not_regular() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

pub(in crate::collector) enum Line {
    Text,
    Oversized,
}

/// Reads the next line into `line`; `Ok(None)` at end of input. A line past
/// `max_bytes` is consumed without being kept.
pub(in crate::collector) fn next_line(
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

/// Lines of a log, each counted in `lines_seen`. An oversized line counts as
/// a parse error and is skipped; a read error counts as one and ends the
/// file, since a broken zstd frame (or a directory read as a file) fails the
/// same way on every read and skipping it like a bad line would loop forever.
pub(in crate::collector) struct Lines<R> {
    reader: R,
    line: Vec<u8>,
    index: usize,
    done: bool,
}

impl<R: BufRead> Lines<R> {
    pub(in crate::collector) fn new(reader: R) -> Self {
        Self {
            reader,
            line: Vec::new(),
            index: 0,
            done: false,
        }
    }

    /// The next line's index among all lines read (skipped ones included) and
    /// its bytes without the `\n` / `\r\n` terminator, as `BufRead::lines`
    /// yields them.
    pub(in crate::collector) fn next(&mut self, events: &mut FileEvents) -> Option<(usize, &[u8])> {
        loop {
            if self.done {
                return None;
            }
            let read = next_line(&mut self.reader, &mut self.line, MAX_LINE_BYTES);
            if matches!(read, Ok(None)) {
                self.done = true;
                return None;
            }
            events.lines_seen += 1;
            let index = self.index;
            self.index += 1;
            match read {
                Ok(Some(Line::Text)) => {
                    let mut text = self.line.as_slice();
                    if let Some(rest) = text.strip_suffix(b"\n") {
                        text = rest.strip_suffix(b"\r").unwrap_or(rest);
                    }
                    return Some((index, text));
                }
                Ok(Some(Line::Oversized)) => events.parse_errors += 1,
                Ok(None) | Err(_) => {
                    events.parse_errors += 1;
                    self.done = true;
                }
            }
        }
    }
}

#[cfg(test)]
pub(in crate::collector) mod tests {
    use std::io::{BufReader, Cursor};

    use tempfile::TempDir;

    use super::*;

    /// A JSON object padded with whitespace to just past `MAX_LINE_BYTES`;
    /// valid JSON, so only the cap can keep it out.
    pub(in crate::collector) fn oversized_json(object: &str) -> String {
        let body = object
            .strip_prefix('{')
            .expect("fixture should be a JSON object");
        let pad = usize::try_from(MAX_LINE_BYTES).expect("cap fits usize");
        format!("{{{}{body}", " ".repeat(pad))
    }

    /// Runs `work` on its own thread and fails the test if it hasn't finished
    /// in a few seconds, so a reader that blocks reports instead of hanging.
    pub(in crate::collector) fn finishes_quickly<T: Send + 'static>(
        work: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work());
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("reading should not block")
    }

    #[cfg(unix)]
    pub(in crate::collector) fn make_fifo(path: &Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("mkfifo should run");
        assert!(status.success(), "mkfifo should create {}", path.display());
    }

    #[test]
    fn oversized_line_is_skipped_without_being_buffered() {
        let mut reader = BufReader::new(
            io::repeat(b'a')
                .take(1 << 20)
                .chain(Cursor::new(b"\n{\"ok\":1}\n".to_vec())),
        );
        let mut line = Vec::new();

        let first = next_line(&mut reader, &mut line, 64 << 10).expect("reads");
        assert!(matches!(first, Some(Line::Oversized)));
        assert!(line.capacity() < 1 << 20);
        let second = next_line(&mut reader, &mut line, 64 << 10).expect("reads");
        assert!(matches!(second, Some(Line::Text)));
        assert_eq!(line, b"{\"ok\":1}\n");
        assert!(
            next_line(&mut reader, &mut line, 64 << 10)
                .expect("reads")
                .is_none()
        );
    }

    #[test]
    fn lines_strip_terminators_and_count_every_line() {
        let mut events = FileEvents::default();
        let mut lines = Lines::new(Cursor::new(b"a\r\n\nb".to_vec()));
        let mut seen = Vec::new();
        while let Some((index, line)) = lines.next(&mut events) {
            seen.push((index, line.to_vec()));
        }

        assert_eq!(
            seen,
            [(0, b"a".to_vec()), (1, Vec::new()), (2, b"b".to_vec())]
        );
        assert_eq!(events.lines_seen, 3);
        assert_eq!(events.parse_errors, 0);
    }

    #[test]
    fn line_past_the_cap_is_a_parse_error_and_reading_continues() {
        let mut events = FileEvents::default();
        let text = format!("{}\nnext\n", oversized_json("{}"));
        let mut lines = Lines::new(Cursor::new(text.into_bytes()));

        let (index, line) = lines.next(&mut events).expect("the short line is read");
        assert_eq!((index, line), (1, b"next".as_slice()));
        assert!(lines.next(&mut events).is_none());
        assert_eq!(events.lines_seen, 2);
        assert_eq!(events.parse_errors, 1);
    }

    #[cfg(unix)]
    #[test]
    fn special_files_are_not_opened() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let fifo = temp.path().join("fifo.jsonl");
        make_fifo(&fifo);
        let device = temp.path().join("device.jsonl");
        std::os::unix::fs::symlink("/dev/zero", &device).expect("symlink should be created");
        let directory = temp.path().join("dir.jsonl");
        std::fs::create_dir(&directory).expect("dir should be created");

        let opened = finishes_quickly(move || {
            [fifo, device, directory].map(|path| open_regular(&path).is_ok())
        });

        assert_eq!(opened, [false; 3]);
    }

    /// Covers a FIFO swapped in after the path check: opening it must neither
    /// block nor be accepted.
    #[cfg(unix)]
    #[test]
    fn a_fifo_reached_past_the_path_check_does_not_block() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let fifo = temp.path().join("swapped.jsonl");
        make_fifo(&fifo);

        let opened = finishes_quickly(move || open_checked(&fifo).is_ok());

        assert!(!opened);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_a_regular_file_is_followed() {
        let temp = TempDir::new().expect("test tempdir should be created");
        let target = temp.path().join("real.jsonl");
        std::fs::write(&target, "{}\n").expect("fixture should be written");
        let link = temp.path().join("link.jsonl");
        std::os::unix::fs::symlink(&target, &link).expect("symlink should be created");

        let mut text = String::new();
        open_regular(&link)
            .expect("a link to a regular file opens")
            .read_to_string(&mut text)
            .expect("reads");

        assert_eq!(text, "{}\n");
    }
}
