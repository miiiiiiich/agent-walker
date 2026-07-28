//! CLI arguments, resolved configuration, and the default log-location
//! probes — what the app reads and which mode it runs in (report, share,
//! render, completions), separate from how the report is assembled.
use std::env;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use clap_complete::Shell;
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
    #[arg(long, value_name = "DIR")]
    pub claude_dir: Option<PathBuf>,

    #[arg(long, value_name = "DIR")]
    pub codex_dir: Option<PathBuf>,

    /// Override the Antigravity log directory. Antigravity is auto-detected:
    /// its tab appears only when logs are present. Text logs supply activity
    /// only; token usage is decoded from the conversation store, so the tab
    /// feeds the token totals like every other provider.
    #[arg(long, value_name = "DIR")]
    pub agy_dir: Option<PathBuf>,

    /// Override the Grok Build root (default `~/.grok`, or `$GROK_HOME`).
    /// Auto-detected: its tab appears only when session logs are present
    /// under `sessions/`.
    #[arg(long, value_name = "DIR")]
    pub grok_dir: Option<PathBuf>,

    /// Override the GitHub Copilot CLI root (default `~/.copilot`, or
    /// `$COPILOT_HOME`). Auto-detected: its tab appears only when
    /// `session-state` session logs are present. Token totals come from the
    /// per-session shutdown records the CLI writes on clean exit.
    #[arg(long, value_name = "DIR")]
    pub copilot_dir: Option<PathBuf>,

    /// Override the OpenCode data directory (default `~/.local/share/opencode`,
    /// or `$OPENCODE_HOME` / `$XDG_DATA_HOME/opencode`). Auto-detected: its tab
    /// appears only when `opencode.db` is present. Tokens are read from the
    /// local SQLite store.
    #[arg(long, value_name = "DIR")]
    pub opencode_dir: Option<PathBuf>,

    /// Override the path to Cursor's `state.vscdb` (default is the platform
    /// config dir, e.g. `~/Library/Application Support/Cursor/...`). To supply a
    /// session JWT directly, set the `CURSOR_TOKEN` env var — a token on the
    /// command line would leak into `ps` and shell history.
    #[arg(long, value_name = "PATH")]
    pub cursor_state_db: Option<PathBuf>,

    /// Disable the Cursor collector. Cursor is the only collector that sends a
    /// credential off the machine — your local Cursor session cookie, to
    /// cursor.com, to read your own usage. Pass this to stop that egress.
    /// (Anonymous model-pricing metadata is still fetched; it carries no
    /// credential.)
    #[arg(long)]
    pub no_cursor: bool,

    /// Analysis window. Defaults to 30 days — Claude Code retains roughly a
    /// month of logs. The codename level is always computed from the most recent
    /// 30 days, so changing this only resizes the graphs, never the title.
    #[arg(long, default_value_t = 30, value_name = "DAYS")]
    pub days: u16,

    /// Ignore the per-file parse cache and rescan everything.
    #[arg(long)]
    pub no_cache: bool,

    /// Render the shareable stats card to a PNG path and print the caption.
    #[arg(long, value_name = "PATH")]
    pub share: Option<PathBuf>,

    /// Print shell completions for the given shell and exit.
    #[arg(long, value_enum, value_name = "SHELL")]
    pub completions: Option<Shell>,

    #[arg(long, hide = true)]
    pub snapshot: bool,

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
    /// Antigravity log directory, or `None` when it can't be resolved (e.g.
    /// `dirs::home_dir()` fails in a sandbox). Resolution failure is swallowed
    /// rather than fatal, since Antigravity is optional — its tab only shows up
    /// when logs are actually found there.
    pub agy_dir: Option<PathBuf>,
    /// Grok Build root, or `None` when it can't be resolved. Optional and
    /// auto-detected like the other secondary providers.
    pub grok_dir: Option<PathBuf>,
    /// GitHub Copilot CLI root, or `None` when it can't be resolved. Optional
    /// and auto-detected like Antigravity: the tab appears only when session
    /// logs exist under `session-state/`.
    pub copilot_dir: Option<PathBuf>,
    /// OpenCode data directory, or `None` when it can't be resolved. Optional and
    /// auto-detected like Antigravity: the tab appears only when `opencode.db`
    /// exists there.
    pub opencode_dir: Option<PathBuf>,
    /// Cursor settings, or `None` when there's nothing to read (no Cursor store
    /// and no `CURSOR_TOKEN`). `Some` carries the resolved `state.vscdb` path,
    /// the CLI-config path, and an optional token override. Auto-detected, but
    /// the one collector that reaches the network.
    pub cursor: Option<CursorConfig>,
    pub days: u16,
    pub use_cache: bool,
    /// Local UTC offset captured at startup (single-threaded moment), used to
    /// bucket all timestamps into the user's local days and hours.
    pub local_offset: UtcOffset,
}

/// Resolved Cursor settings (see `Config::cursor`).
#[derive(Clone)]
pub struct CursorConfig {
    pub state_db: PathBuf,
    pub cli_config: PathBuf,
    /// Token override from `CURSOR_TOKEN`; `None` reads the local `state.vscdb`.
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

/// Build the Cursor config. Cursor is **auto-detected** like the other providers
/// — it runs whenever there's something to read (an explicit `CURSOR_TOKEN`, or a
/// local `state.vscdb` that exists) — but because it's the one collector that
/// reaches the network, `--no-cursor` turns it off entirely. With nothing to
/// detect it's skipped. Signed out (store exists but no token) it stays silent
/// and never hits the network — handled in the collector. Path resolution
/// failures fall back to empty paths so an explicit token still works in CI /
/// sandboxes where the home dir can't be resolved.
pub(super) fn cursor_config(args: &Args) -> Option<CursorConfig> {
    // The one network-reaching collector is opt-out: honor --no-cursor before
    // touching the env or disk so nothing is read and no request is made.
    if args.no_cursor {
        return None;
    }
    let token = env::var("CURSOR_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty());
    let state_db = args
        .cursor_state_db
        .clone()
        .or_else(|| crate::paths::cursor_state_db().ok());
    // Nothing to collect unless there's a token source: an explicit token, or a
    // Cursor store present on disk. (A present store with no token — signed out —
    // is handled in the collector: it reads no token and makes no request.)
    let store_present = state_db.as_ref().is_some_and(|path| path.exists());
    if token.is_none() && !store_present {
        return None;
    }
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
