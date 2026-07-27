//! Cross-platform path helpers. All home / cache / downloads resolution goes
//! through here so a switch from `HOME` to `dirs::*` happens once, and Windows
//! and Unix share the same code path.
//!
//! Per-tool roots can be overridden by an environment variable so users with a
//! relocated config (multi-account setups, repos on a different drive on
//! Windows) don't end up with a blank dashboard. The variables we honor are
//! the ones each tool itself reads; see `claude_home` / `codex_home` for the
//! per-tool notes.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Result, anyhow};

/// The user's home directory. `dirs::home_dir` consults `$HOME` on Unix and
/// `%USERPROFILE%` on Windows.
pub fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not locate the user home directory"))
}

/// Pure helper: if `env_value` is a non-empty `Some` use it as the root,
/// otherwise call `fallback()`. The fallback is lazy so `home_dir()` (which
/// can itself fail) is never invoked when an env override is in effect — that
/// lets a user with no resolvable home still point agent-walker at their
/// relocated agent state via the env variable. Takes an `OsString` (not a
/// `String`) so a path with non-UTF-8 bytes — legal on Unix — round-trips
/// instead of being silently dropped. Split out so the env logic can be
/// tested without mutating process environment variables.
fn resolve_root<F>(env_value: Option<OsString>, fallback: F) -> Result<PathBuf>
where
    F: FnOnce() -> Result<PathBuf>,
{
    match env_value {
        Some(value) if !value.is_empty() => Ok(PathBuf::from(value)),
        _ => fallback(),
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
    resolve_root(std::env::var_os("CLAUDE_CONFIG_DIR"), || {
        Ok(home_dir()?.join(".claude"))
    })
}

/// Root directory Codex CLI reads. Defaults to `~/.codex`.
///
/// `CODEX_HOME` overrides it if set — this is the official variable
/// documented at <https://developers.openai.com/codex/environment-variables>.
pub fn codex_home() -> Result<PathBuf> {
    resolve_root(std::env::var_os("CODEX_HOME"), || {
        Ok(home_dir()?.join(".codex"))
    })
}

/// Root directory Antigravity CLI reads. No env override is known; the value
/// is `~/.gemini/antigravity-cli`.
pub fn agy_home() -> Result<PathBuf> {
    Ok(home_dir()?.join(".gemini").join("antigravity-cli"))
}

/// Root directory Grok Build (xAI's agentic CLI) writes. `GROK_HOME`
/// overrides it; the default is `~/.grok`, with per-cwd session logs under
/// `<root>/sessions/<encoded-cwd>/<session-id>/updates.jsonl`.
pub fn grok_home() -> Result<PathBuf> {
    resolve_root(std::env::var_os("GROK_HOME"), || {
        Ok(home_dir()?.join(".grok"))
    })
}

/// Root directory GitHub Copilot CLI writes. `COPILOT_HOME` overrides it;
/// the default is `~/.copilot`, with session logs under
/// `<root>/session-state/<uuid>/events.jsonl`.
pub fn copilot_home() -> Result<PathBuf> {
    resolve_root(std::env::var_os("COPILOT_HOME"), || {
        Ok(home_dir()?.join(".copilot"))
    })
}

/// Data directory OpenCode reads. `OPENCODE_HOME` overrides it; otherwise it
/// follows the XDG data dir (`$XDG_DATA_HOME/opencode`, default
/// `~/.local/share/opencode`), as documented at
/// <https://opencode.ai/docs/troubleshooting/>. The SQLite store lives at
/// `<root>/opencode.db`.
pub fn opencode_home() -> Result<PathBuf> {
    resolve_root(std::env::var_os("OPENCODE_HOME"), || {
        let xdg = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty());
        let base = match xdg {
            Some(value) => PathBuf::from(value),
            None => home_dir()?.join(".local").join("share"),
        };
        Ok(base.join("opencode"))
    })
}

/// Cursor's Electron `state.vscdb`, where the auth token lives. Cursor follows
/// the VS Code layout under the platform config dir: `Application Support`
/// (macOS), `%APPDATA%` (Windows), `~/.config` (Linux).
pub fn cursor_state_db() -> Result<PathBuf> {
    let base =
        dirs::config_dir().ok_or_else(|| anyhow!("could not locate the user config directory"))?;
    Ok(base
        .join("Cursor")
        .join("User")
        .join("globalStorage")
        .join("state.vscdb"))
}

/// Cursor CLI config, which carries the account's `authId` used to build the
/// session cookie.
pub fn cursor_cli_config() -> Result<PathBuf> {
    Ok(home_dir()?.join(".cursor").join("cli-config.json"))
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
    fn resolve_root_uses_env_when_set_without_touching_fallback() {
        // The closure must NOT run when an env override is in effect — that's
        // what lets users with an unresolvable home still point us at their
        // relocated state via `CODEX_HOME` / `CLAUDE_CONFIG_DIR`.
        let result = resolve_root(Some(OsString::from("/srv/codex")), || {
            panic!("fallback must not be called")
        });
        assert_eq!(result.unwrap(), PathBuf::from("/srv/codex"));
    }

    #[test]
    fn resolve_root_falls_back_when_env_missing() {
        let result = resolve_root(None, || Ok(PathBuf::from("/home/me/.codex")));
        assert_eq!(result.unwrap(), PathBuf::from("/home/me/.codex"));
    }

    #[test]
    fn resolve_root_treats_empty_env_as_unset() {
        // An empty value is almost always a misconfigured env (`CODEX_HOME=`
        // in a shell script clears it); treat it like unset rather than
        // pointing the tool at the current working directory.
        let result = resolve_root(Some(OsString::new()), || {
            Ok(PathBuf::from("/home/me/.codex"))
        });
        assert_eq!(result.unwrap(), PathBuf::from("/home/me/.codex"));
    }

    #[test]
    fn resolve_root_propagates_fallback_error() {
        let result: Result<PathBuf> = resolve_root(None, || Err(anyhow!("home not found")));
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_root_preserves_non_utf8_env_path() {
        // Unix paths can contain non-UTF-8 bytes. The earlier `env::var`-based
        // path silently dropped these into the fallback; `env::var_os` round-
        // trips them so the override still wins.
        use std::os::unix::ffi::OsStringExt;
        let bytes = vec![b'/', b't', b'm', b'p', b'/', 0xff, 0xfe, b'/', b'x'];
        let raw = OsString::from_vec(bytes.clone());
        let result = resolve_root(Some(raw.clone()), || {
            panic!("fallback must not be called for a non-UTF-8 path")
        });
        assert_eq!(result.unwrap(), PathBuf::from(raw));
    }
}
