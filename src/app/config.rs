/// Fix the TUI window because day charts allocate one column per day.
pub const ANALYSIS_WINDOW_DAYS: u16 = 30;

use std::env;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use time::UtcOffset;

#[derive(Debug, Parser)]
#[command(
    name = "agent-walker",
    bin_name = "agent-walker",
    about = "Inspect local AI coding-agent usage",
    version
)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Each bool is an independent CLI flag, not a state machine."
)]
pub struct Args {
    /// Read Cursor's usage from its dashboard. This is the one request that
    /// carries a credential (your local Cursor session cookie, sent to
    /// cursor.com), so it is off unless asked for. `CURSOR_TOKEN` supplies
    /// the session JWT directly.
    #[arg(long)]
    pub cursor: bool,

    /// Ignore the per-file parse cache and rescan everything.
    #[arg(long)]
    pub no_cache: bool,

    /// Render the shareable stats card to a PNG path and print the caption.
    #[arg(long, value_name = "PATH")]
    pub share: Option<PathBuf>,

    /// Export the summary and dated events as one JSON document (experimental).
    #[arg(long, conflicts_with_all = ["share", "render"])]
    pub json: bool,

    /// Analysis window in days (with --json only).
    #[arg(
        long,
        value_name = "N",
        default_value_t = ANALYSIS_WINDOW_DAYS,
        requires = "json",
        value_parser = clap::value_parser!(u16).range(1..)
    )]
    pub days: u16,

    /// Render the TUI of every provider tab as plain text at the given
    /// terminal width and exit.
    #[arg(
        long,
        hide = true,
        value_name = "WIDTH",
        num_args = 0..=1,
        default_missing_value = "140"
    )]
    pub render: Option<u16>,

    #[arg(short, long)]
    pub verbose: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub demo: bool,
    pub claude_dir: PathBuf,
    pub codex_dir: PathBuf,
    /// Home-resolution failure is nonfatal because Antigravity is optional.
    pub agy_dir: Option<PathBuf>,
    pub grok_dir: Option<PathBuf>,
    pub copilot_dir: Option<PathBuf>,
    pub opencode_dir: Option<PathBuf>,
    pub cursor: Option<CursorConfig>,
    pub use_cache: bool,
    pub local_offset: UtcOffset,
}

#[derive(Clone)]
pub struct CursorConfig {
    pub state_db: PathBuf,
    pub cli_config: PathBuf,
    pub token: Option<String>,
}

// Manual `Debug` so the session token never lands in a `{config:?}` dump (a log
// line, stderr, a panic message). Only the presence of a token is shown.
impl std::fmt::Debug for CursorConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CursorConfig")
            .field("state_db", &self.state_db)
            .field("cli_config", &self.cli_config)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

pub(super) fn default_claude_dir() -> Result<PathBuf> {
    Ok(crate::paths::claude_home()?.join("projects"))
}

pub(super) fn default_codex_dir() -> Result<PathBuf> {
    Ok(crate::paths::codex_home()?.join("sessions"))
}

pub(super) fn default_agy_dir() -> Result<PathBuf> {
    crate::paths::agy_home()
}

pub(super) fn default_opencode_dir() -> Result<PathBuf> {
    crate::paths::opencode_home()
}

pub(super) fn cursor_config(args: &Args) -> Option<CursorConfig> {
    // Checked first so a run without `--cursor` reads neither `CURSOR_TOKEN`
    // nor the store.
    if !args.cursor {
        return None;
    }
    let token = env::var("CURSOR_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty());
    let state_db = crate::paths::cursor_state_db().ok();
    let store_present = state_db.as_ref().is_some_and(|path| path.exists());
    if token.is_none() && !store_present {
        return None;
    }
    // Empty paths let an explicit token work when home resolution fails.
    Some(CursorConfig {
        state_db: state_db.unwrap_or_default(),
        cli_config: crate::paths::cursor_cli_config().unwrap_or_default(),
        token,
    })
}

pub(super) fn demo_enabled() -> bool {
    let Some(value) = env::var_os("AGENT_WALKER_DEMO") else {
        return false;
    };
    let Some(value) = value.to_str() else {
        return false;
    };
    value == "1"
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("on")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_requires_json() {
        let error = Args::try_parse_from(["agent-walker", "--days", "60"]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert!(Args::try_parse_from(["agent-walker"]).is_ok());
        let defaults = Args::try_parse_from(["agent-walker", "--json"]).unwrap();
        assert_eq!(defaults.days, ANALYSIS_WINDOW_DAYS);
        let custom = Args::try_parse_from(["agent-walker", "--json", "--days", "60"]).unwrap();
        assert_eq!(custom.days, 60);
        for invalid in ["0", "65536"] {
            assert!(Args::try_parse_from(["agent-walker", "--json", "--days", invalid]).is_err());
        }
    }

    /// Cursor is the one collector that sends a credential, so it is opt-in:
    /// without the flag no Cursor config exists, whatever the environment holds.
    #[test]
    fn cursor_is_off_unless_asked() {
        let args = Args::try_parse_from(["agent-walker"]).unwrap();
        assert!(!args.cursor);
        assert!(cursor_config(&args).is_none());
        assert!(
            Args::try_parse_from(["agent-walker", "--cursor"])
                .unwrap()
                .cursor
        );
    }

    #[test]
    fn removed_flags_are_rejected() {
        for gone in [
            "--completions=bash",
            "--claude-dir=x",
            "--cursor-state-db=x",
            "--no-cursor",
        ] {
            assert!(
                Args::try_parse_from(["agent-walker", gone]).is_err(),
                "{gone}"
            );
        }
    }

    #[test]
    fn json_is_public_and_exclusive() {
        use clap::CommandFactory;

        let help = Args::command().render_long_help().to_string();
        assert!(help.contains("--json"));
        assert!(help.contains("--days <N>"));
        assert!(Args::try_parse_from(["agent-walker", "--snapshot"]).is_err());
        for flag in ["--render", "--share=out.png"] {
            assert!(Args::try_parse_from(["agent-walker", "--json", flag]).is_err());
        }
    }
}
