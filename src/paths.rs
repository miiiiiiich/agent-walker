use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Result, anyhow};

pub fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not locate the user home directory"))
}

fn resolve_root<F>(env_value: Option<OsString>, fallback: F) -> Result<PathBuf>
where
    F: FnOnce() -> Result<PathBuf>,
{
    match env_value {
        Some(value) if !value.is_empty() => Ok(PathBuf::from(value)),
        _ => fallback(),
    }
}

pub fn claude_home() -> Result<PathBuf> {
    resolve_root(std::env::var_os("CLAUDE_CONFIG_DIR"), || {
        Ok(home_dir()?.join(".claude"))
    })
}

pub fn codex_home() -> Result<PathBuf> {
    resolve_root(std::env::var_os("CODEX_HOME"), || {
        Ok(home_dir()?.join(".codex"))
    })
}

pub fn agy_home() -> Result<PathBuf> {
    Ok(home_dir()?.join(".gemini").join("antigravity-cli"))
}

pub fn grok_home() -> Result<PathBuf> {
    resolve_root(std::env::var_os("GROK_HOME"), || {
        Ok(home_dir()?.join(".grok"))
    })
}

pub fn copilot_home() -> Result<PathBuf> {
    resolve_root(std::env::var_os("COPILOT_HOME"), || {
        Ok(home_dir()?.join(".copilot"))
    })
}

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

pub fn cursor_state_db() -> Result<PathBuf> {
    let base =
        dirs::config_dir().ok_or_else(|| anyhow!("could not locate the user config directory"))?;
    Ok(base
        .join("Cursor")
        .join("User")
        .join("globalStorage")
        .join("state.vscdb"))
}

pub fn cursor_cli_config() -> Result<PathBuf> {
    Ok(home_dir()?.join(".cursor").join("cli-config.json"))
}

/// Keep the cache under `<home>/.cache/agent-walker` so existing macOS/Linux
/// users do not lose their warmed cache.
pub fn cache_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".cache").join("agent-walker"))
}

/// Use the OS-native Downloads resolver so localized and relocated folders resolve correctly.
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
        use std::os::unix::ffi::OsStringExt;
        let bytes = vec![b'/', b't', b'm', b'p', b'/', 0xff, 0xfe, b'/', b'x'];
        let raw = OsString::from_vec(bytes.clone());
        let result = resolve_root(Some(raw.clone()), || {
            panic!("fallback must not be called for a non-UTF-8 path")
        });
        assert_eq!(result.unwrap(), PathBuf::from(raw));
    }
}
