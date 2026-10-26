//! Files agent-walker writes on its own (the parse cache, the pricing
//! snapshot, the card saved to Downloads) and reads back. Their names are
//! fixed and their directories are ones other software also writes into — an
//! archive unpacked into Downloads can carry a symlink — so nothing found at
//! those paths is trusted.

use std::collections::hash_map::RandomState;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::hash::BuildHasher;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Ordinary user file; the umask still applies.
pub const SHARED_MODE: u32 = 0o666;
/// Holds project paths and session ids derived from the logs.
pub const PRIVATE_MODE: u32 = 0o600;

const TEMP_ATTEMPTS: u32 = 8;

/// Replace `path` with `bytes` through a fresh sibling and a rename.
///
/// `fs::write` opens `path` and follows a symlink planted there, so a link to
/// `~/.zshrc` gets the PNG written into it; truncating a hard link rewrites
/// the other name too. `create_new` refuses any existing entry at the temp
/// name, symlink included, and `rename` swaps the directory entry at `path`
/// instead of writing through it.
pub fn write_replacing(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let (temp, mut file) = create_temp(path, mode)?;
    let written = file.write_all(bytes);
    drop(file);
    let result = written.and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Only directories created here get `0o700`. An existing one keeps its mode:
/// a chmod by path would follow a symlink and change whatever it points at.
#[cfg(unix)]
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Read a file this app wrote, refusing anything else found at `path`.
///
/// A FIFO would block the open until a writer shows up and a device such as
/// `/dev/zero` never reaches EOF, so only a regular file is read. Symlinks are
/// refused rather than followed because `write_replacing` only ever renames a
/// regular file into place, so a link here was planted. `max_len` bounds the
/// read even if the file grows after the check.
pub fn read_regular(path: &Path, max_len: u64) -> io::Result<Vec<u8>> {
    let file = open_no_follow(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(not_regular());
    }
    if metadata.len() > max_len {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(max_len.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_len {
        return Err(too_large());
    }
    Ok(bytes)
}

/// The type is judged on the opened handle, never by path first: an lstat
/// followed by an open lets a FIFO swapped in between block the open, and a
/// symlink swapped in get followed. `O_NOFOLLOW` refuses a link at the last
/// component and `O_NONBLOCK` makes opening a FIFO return at once.
#[cfg(unix)]
fn open_no_follow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                not_regular()
            } else {
                error
            }
        })
}

/// Windows has no FIFOs to block on and std has no no-follow open, so the link
/// check runs by path; a link swapped in before the open is followed, and the
/// handle check still refuses anything but a regular file.
#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> io::Result<File> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(not_regular());
    }
    File::open(path)
}

fn not_regular() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

fn too_large() -> io::Error {
    io::Error::new(io::ErrorKind::FileTooLarge, "file exceeds the read limit")
}

/// A predictable temp name lets anything that can write the directory plant
/// a link there first; `create_new` turns that into a retry, not a write.
fn create_temp(path: &Path, mode: u32) -> io::Result<(PathBuf, File)> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let mut attempt = 1;
    loop {
        let temp = path.with_file_name(temp_name(name));
        match open_new(&temp, mode) {
            Ok(file) => return Ok((temp, file)),
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists && attempt < TEMP_ATTEMPTS =>
            {
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// `.<name>.tmp<digits>`: hidden in Downloads, and the cache sweep recognizes
/// the all-digit suffix as an orphaned temp.
fn temp_name(name: &OsStr) -> OsString {
    let nonce = RandomState::new().hash_one(std::process::id());
    let mut temp = OsString::from(".");
    temp.push(name);
    temp.push(format!(".tmp{nonce}"));
    temp
}

#[cfg(unix)]
fn open_new(path: &Path, mode: u32) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    File::options()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
}

#[cfg(not(unix))]
fn open_new(path: &Path, _mode: u32) -> io::Result<File> {
    File::options().write(true).create_new(true).open(path)
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    #[test]
    fn symlink_at_target_is_replaced_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        let target = dir.path().join("out.png");
        symlink(&victim, &target).unwrap();

        write_replacing(&target, b"new", SHARED_MODE).unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert!(fs::symlink_metadata(&target).unwrap().is_file());
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn hard_link_at_target_keeps_the_other_name() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        let target = dir.path().join("out.png");
        fs::hard_link(&victim, &target).unwrap();

        write_replacing(&target, b"new", SHARED_MODE).unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn temp_open_refuses_a_planted_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
        let temp = dir.path().join(".claude.bin.tmp1");
        symlink(&victim, &temp).unwrap();

        let error = open_new(&temp, PRIVATE_MODE).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn writes_leave_no_temp_and_apply_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("claude.bin");
        write_replacing(&target, b"one", PRIVATE_MODE).unwrap();
        write_replacing(&target, b"two", PRIVATE_MODE).unwrap();

        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["claude.bin"]);
        assert_eq!(fs::read(&target).unwrap(), b"two");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn failed_rename_removes_the_temp() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("claude.bin");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("inside"), b"x").unwrap();

        assert!(write_replacing(&target, b"new", PRIVATE_MODE).is_err());

        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["claude.bin"]);
    }

    #[test]
    fn open_refuses_a_symlink_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let regular = dir.path().join("regular");
        fs::write(&regular, b"12345").unwrap();
        let link = dir.path().join("link");
        symlink(&regular, &link).unwrap();

        assert!(open_no_follow(&regular).is_ok());
        assert_eq!(
            open_no_follow(&link).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn open_returns_at_once_on_a_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("claude.bin");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());

        let (sender, receiver) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = sender.send(open_no_follow(&path).is_ok());
        });
        let opened = receiver.recv_timeout(std::time::Duration::from_secs(5));
        if opened.is_err() {
            // Give the blocked open a writer so the thread can finish.
            let _ = File::options().write(true).open(&fifo);
        }
        assert_eq!(
            opened,
            Ok(true),
            "opening a FIFO must not wait for a writer"
        );
        assert_eq!(
            read_regular(&fifo, 5).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn read_refuses_links_devices_and_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let regular = dir.path().join("regular");
        fs::write(&regular, b"12345").unwrap();
        let link = dir.path().join("link");
        symlink(&regular, &link).unwrap();
        let zero = dir.path().join("zero");
        symlink("/dev/zero", &zero).unwrap();

        assert_eq!(read_regular(&regular, 5).unwrap(), b"12345");
        assert_eq!(
            read_regular(&regular, 4).unwrap_err().kind(),
            io::ErrorKind::FileTooLarge
        );
        assert_eq!(
            read_regular(&link, 5).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            read_regular(&zero, 5).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            read_regular(Path::new("/dev/zero"), 5).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            read_regular(dir.path(), 5).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
