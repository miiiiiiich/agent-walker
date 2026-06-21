//! Cross-platform path helpers. All home / cache / downloads resolution goes
//! through here so a switch from `HOME` to `dirs::*` happens once, and Windows
//! and Unix share the same code path.

use std::path::PathBuf;

use anyhow::{Result, anyhow};

/// The user's home directory. `dirs::home_dir` consults `$HOME` on Unix and
/// `%USERPROFILE%` on Windows.
pub fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not locate the user home directory"))
}

/// Base directory for `agent-walker`'s parse cache. Stays at
/// `<home>/.cache/agent-walker` on every platform so a macOS/Linux user
/// upgrading does not lose their warmed cache; on Windows this resolves under
/// `%USERPROFILE%\.cache\agent-walker`.
pub fn cache_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".cache").join("agent-walker"))
}

/// Default save directory for the share card. `dirs::download_dir` consults
/// the OS-native location (Known Folders on Windows, `NSDownloadsDirectory`
/// on macOS, XDG on Linux), so a non-English or relocated Downloads folder
/// still resolves correctly. Falls back to the home directory when the OS
/// doesn't report a Downloads location.
pub fn downloads_dir() -> Result<PathBuf> {
    let home = home_dir()?;
    Ok(dirs::download_dir()
        .filter(|path| path.is_dir())
        .unwrap_or(home))
}
