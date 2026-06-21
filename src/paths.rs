//! Cross-platform path helpers. All home / cache / downloads resolution goes
//! through here so a switch from `HOME` to `dirs::*` happens once, and Windows
//! and Unix share the same code path.
//!
//! Per-tool roots can be overridden by an environment variable so users with a
//! relocated config (multi-account setups, repos on a different drive on
//! Windows) don't end up with a blank dashboard. The variables we honor are
//! the ones each tool itself reads; see `claude_home` / `codex_home` for the
//! per-tool notes.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

/// The user's home directory. `dirs::home_dir` consults `$HOME` on Unix and
/// `%USERPROFILE%` on Windows.
pub fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not locate the user home directory"))
}

/// Pure helper: if `env_value` is `Some` use it as the root, otherwise fall
/// back to `home.join(fallback_subdir)`. Split out so the env logic can be
/// tested without mutating process environment variables.
fn resolve_root(env_value: Option<&str>, home: &Path, fallback_subdir: &str) -> PathBuf {
    match env_value {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => home.join(fallback_subdir),
    }
}

/// Root directory Claude Code reads. Defaults to `~/.claude`.
///
/// `CLAUDE_CONFIG_DIR` overrides it if set — Anthropic has not officially
/// documented this variable as of 2026-06 (it's a recurring feature request),
/// but Claude Code does read it in practice. We honor it best-effort so
/// agent-walker doesn't disagree with users who relocate their config; if
/// Anthropic ever changes the contract we simply fall back to `~/.claude`.
pub fn claude_home() -> Result<PathBuf> {
    Ok(resolve_root(
        std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
        &home_dir()?,
        ".claude",
    ))
}

/// Root directory Codex CLI reads. Defaults to `~/.codex`.
///
/// `CODEX_HOME` overrides it if set — this is the official variable
/// documented at <https://developers.openai.com/codex/environment-variables>.
pub fn codex_home() -> Result<PathBuf> {
    Ok(resolve_root(
        std::env::var("CODEX_HOME").ok().as_deref(),
        &home_dir()?,
        ".codex",
    ))
}

/// Root directory Antigravity CLI reads. No env override is known; the value
/// is `~/.gemini/antigravity-cli`.
pub fn agy_home() -> Result<PathBuf> {
    Ok(home_dir()?.join(".gemini").join("antigravity-cli"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_root_uses_env_when_set() {
        let home = PathBuf::from("/home/me");
        assert_eq!(
            resolve_root(Some("/srv/codex"), &home, ".codex"),
            PathBuf::from("/srv/codex"),
        );
    }

    #[test]
    fn resolve_root_falls_back_to_home_when_env_missing() {
        let home = PathBuf::from("/home/me");
        assert_eq!(
            resolve_root(None, &home, ".codex"),
            PathBuf::from("/home/me/.codex"),
        );
    }

    #[test]
    fn resolve_root_treats_empty_env_as_unset() {
        // An empty value is almost always a misconfigured env (`CODEX_HOME=`
        // in a shell script clears it); treat it like unset rather than
        // pointing the tool at the current working directory.
        let home = PathBuf::from("/home/me");
        assert_eq!(
            resolve_root(Some(""), &home, ".codex"),
            PathBuf::from("/home/me/.codex"),
        );
    }
}
