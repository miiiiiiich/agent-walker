/// Fix the TUI window because day charts allocate one column per day.
pub const ANALYSIS_WINDOW_DAYS: u16 = 30;

use std::env;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use time::UtcOffset;

use crate::session_view::SessionArgs;
use crate::summary_view::SummaryArgs;

#[derive(Debug, Parser)]
#[command(
    name = "agent-walker",
    bin_name = "agent-walker",
    about = "Inspect local AI coding-agent usage",
    version,
    args_conflicts_with_subcommands = true,
    disable_help_subcommand = true
)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Each bool is an independent CLI flag, not a state machine."
)]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Ignore the per-file parse cache and rescan everything.
    #[arg(long, global = true)]
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

    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the last 30 days' codename and numbers, and exit.
    Summary(SummaryArgs),
    /// Print how one session went, and exit.
    Session(SessionArgs),
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
    pub use_cache: bool,
    pub local_offset: UtcOffset,
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

    #[test]
    fn removed_flags_are_rejected() {
        for gone in ["--completions=bash", "--claude-dir=x"] {
            assert!(
                Args::try_parse_from(["agent-walker", gone]).is_err(),
                "{gone}"
            );
        }
    }

    #[test]
    fn summary_is_a_subcommand_with_its_own_json_flag() {
        let parsed = Args::try_parse_from(["agent-walker", "summary"]).unwrap();
        let Some(Command::Summary(summary)) = parsed.command else {
            panic!("summary did not parse as a subcommand");
        };
        assert!(!summary.json);

        let parsed = Args::try_parse_from(["agent-walker", "summary", "--json"]).unwrap();
        let Some(Command::Summary(summary)) = parsed.command else {
            panic!("summary --json did not parse as a subcommand");
        };
        assert!(summary.json);
        assert!(!parsed.json);

        assert!(
            Args::try_parse_from(["agent-walker"])
                .unwrap()
                .command
                .is_none()
        );
    }

    #[test]
    fn session_takes_an_id_and_its_own_json_flag() {
        let parsed = Args::try_parse_from(["agent-walker", "session", "abc", "--json"]).unwrap();
        let Some(Command::Session(session)) = parsed.command else {
            panic!("session did not parse as a subcommand");
        };
        assert_eq!(session.id, "abc");
        assert!(session.json);

        assert!(Args::try_parse_from(["agent-walker", "session"]).is_err());
        assert!(Args::try_parse_from(["agent-walker", "--json", "session", "abc"]).is_err());
    }

    #[test]
    fn summary_stays_clear_of_the_export_flags() {
        for argv in [
            vec!["agent-walker", "--json", "summary"],
            vec!["agent-walker", "--share=out.png", "summary"],
            vec!["agent-walker", "--render=80", "summary"],
            vec!["agent-walker", "summary", "--days", "7"],
            vec!["agent-walker", "--json=summary"],
            vec!["agent-walker", "help"],
        ] {
            assert!(Args::try_parse_from(&argv).is_err(), "{argv:?}");
        }
    }

    #[test]
    fn cache_and_verbose_flags_reach_the_subcommand_after_its_name() {
        let args = Args::try_parse_from(["agent-walker", "summary", "--no-cache", "-v"]).unwrap();
        assert!(args.no_cache);
        assert!(args.verbose);
        // Clap's conflict covers global flags too, so they go after the subcommand.
        assert!(Args::try_parse_from(["agent-walker", "--no-cache", "summary"]).is_err());
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
