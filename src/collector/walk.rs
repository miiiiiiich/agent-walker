//! Log-directory walking with mtime filtering and scan statistics.
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tracing::debug;

use crate::model::ScanStats;

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
