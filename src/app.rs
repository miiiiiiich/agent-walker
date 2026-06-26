use std::env;
use std::path::PathBuf;
use std::time::{Duration as StdDuration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use time::{OffsetDateTime, UtcOffset};

use crate::analyzer::summarize;
use crate::collector::{agy, claude, codex, cursor, opencode};
use crate::format::snapshot_app;
use crate::model::{AppSummary, Collection};
use crate::ui;

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
    /// its tab appears only when logs are present. Its logs expose no token
    /// usage (an unlabeled protobuf store), so the tab is activity-only and
    /// never feeds the token totals.
    #[arg(long, value_name = "DIR")]
    pub agy_dir: Option<PathBuf>,

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

    /// Disable the Cursor collector. Cursor is the only provider that reaches the
    /// network — it sends your local Cursor session cookie to cursor.com to read
    /// your own usage. Pass this to keep agent-walker fully offline.
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

pub fn run(args: Args) -> Result<()> {
    if let Some(shell) = args.completions {
        clap_complete::generate(
            shell,
            &mut Args::command(),
            "agent-walker",
            &mut std::io::stdout(),
        );
        return Ok(());
    }

    // Must be read before any worker threads exist; `time` refuses to probe
    // the environment for the local offset once the process is multithreaded.
    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    // Resolve Cursor opt-in before the struct literal below moves other `args`
    // fields out (a borrow of `args` after a partial move won't compile).
    let cursor = cursor_config(&args);
    let config = Config {
        demo: demo_enabled(),
        // `map_or_else(default, Ok)` keeps the default lazy, so a `--claude-dir`
        // / `--codex-dir` / `--agy-dir` override on the CLI still works even
        // when `dirs::home_dir()` can't resolve (sandbox / no `$HOME` /
        // `%USERPROFILE%`). Eagerly calling `default_*_dir()?` would short-
        // circuit before the CLI override ever got a chance.
        claude_dir: args.claude_dir.map_or_else(default_claude_dir, Ok)?,
        codex_dir: args.codex_dir.map_or_else(default_codex_dir, Ok)?,
        // Antigravity is always probed (no opt-in flag): an explicit --agy-dir
        // wins, else fall back to the default location. A resolution failure is
        // swallowed to None instead of fatal — agy is optional, so a sandbox
        // without a home dir should still start and just omit the agy tab.
        agy_dir: args.agy_dir.or_else(|| default_agy_dir().ok()),
        // Same treatment as agy: auto-detected, resolution failure swallowed.
        opencode_dir: args.opencode_dir.or_else(|| default_opencode_dir().ok()),
        // Cursor is auto-detected (a signed-in state.vscdb) but is the one
        // collector that reaches the network; `None` when signed out / disabled.
        cursor,
        days: args.days,
        use_cache: !args.no_cache,
        local_offset,
    };

    if let Some(path) = &args.share {
        let report = load_report(&config)?;
        let card = crate::share::ShareCard::from_summary(&report.combined);
        let png = crate::share::render_png(&card)?;
        std::fs::write(path, png)
            .with_context(|| format!("write share card to {}", path.display()))?;
        println!("{}", card.caption());
        eprintln!("\nwrote {}", path.display());
        return Ok(());
    }

    if args.snapshot {
        let report = load_report(&config)?;
        println!("{}", snapshot_app(&report));
        return Ok(());
    }

    if let Some(width) = args.render {
        // Load once and render every visible tab (present providers + Total) from
        // the same report — the tab set is now data-dependent, not a fixed four.
        let report = load_report(&config)?;
        for tab_index in 0..=report.providers.len() {
            println!(
                "{}",
                ui::render_report_tab(&config, &report, width.max(40), 44, tab_index)?
            );
        }
        return Ok(());
    }

    ui::run(config)
}

pub fn load_report(config: &Config) -> Result<AppSummary> {
    // Pricing feeds the COST panels the UI renders later, so finish the refresh
    // before handing back the report.
    let pricing_refresh = crate::cost::spawn_pricing_refresh();
    let result = load_report_inner(config);
    let _ = pricing_refresh.join();
    result.map(|mut report| {
        // Only surface a provider tab when that provider actually has data, then
        // order what's left by how much it's used (heaviest first).
        report.providers.retain(provider_has_data);
        sort_providers_by_usage(&mut report.providers);
        report
    })
}

/// A provider earns a tab only if it has real activity. Token volume catches
/// most agents; sessions, tools, and completions are the fallback for a session
/// that logged activity but no usage tokens. Everything false ⇒ the directory
/// was missing or empty, so the tab is dropped instead of showing a blank tab.
fn provider_has_data(summary: &crate::model::Summary) -> bool {
    summary.total_usage.token_volume() > 0
        || summary.sessions > 0
        || !summary.tools.is_empty()
        || summary.completion_duration.is_some()
}

/// Order the provider tabs by how much each is used — token volume, descending —
/// so the heaviest provider lands at `tab_index 0` (the startup tab). Ties break
/// on provider identity for a stable order. Antigravity carries no tokens, so it
/// naturally sorts last. The Total tab is not in `providers`; it stays appended
/// at the end of the tab strip.
fn sort_providers_by_usage(providers: &mut [crate::model::Summary]) {
    providers.sort_by(|left, right| {
        right
            .total_usage
            .token_volume()
            .cmp(&left.total_usage.token_volume())
            .then_with(|| left.provider.label().cmp(right.provider.label()))
    });
}

fn load_report_inner(config: &Config) -> Result<AppSummary> {
    if config.demo {
        return Ok(crate::demo::demo_report(config));
    }

    let started = Instant::now();
    let now = OffsetDateTime::now_utc().to_offset(config.local_offset);

    // Read the larger of the delta window (2× the display window plus a day of
    // timezone slack) and the codename's fixed 30-day window, so the title stays
    // window-stable even for a short `--days`. Files older than this cannot hold
    // relevant events.
    let history_days = (u64::from(config.days.max(1)) * 2 + 1)
        .max(u64::try_from(crate::codename::CODENAME_WINDOW_DAYS).unwrap_or(30) + 1);
    let mtime_floor = SystemTime::now().checked_sub(StdDuration::from_secs(history_days * 86_400));

    let (codex_result, agy_result, opencode_result, cursor_result, claude_collection) =
        std::thread::scope(|scope| {
            let codex_handle = scope.spawn(|| {
                codex::collect(
                    &config.codex_dir,
                    mtime_floor,
                    config.use_cache,
                    config.local_offset,
                )
            });
            // Cursor is auto-detected (disable with --no-cursor) and the only
            // collector that hits the network, so it runs in its own thread
            // alongside the local ones.
            let cursor_handle = scope.spawn(|| {
                config.cursor.as_ref().map(|cursor| {
                    cursor::collect(
                        &cursor.state_db,
                        &cursor.cli_config,
                        cursor.token.as_deref(),
                        mtime_floor,
                        config.local_offset,
                    )
                })
            });
            // Antigravity and OpenCode are probed whenever their directory
            // resolved; the collector returns an empty collection for a missing
            // dir / DB, and an empty provider is filtered out before it ever
            // becomes a tab. (Antigravity logs carry no token usage; OpenCode's
            // SQLite store does.)
            let agy_handle = scope.spawn(|| {
                config.agy_dir.as_ref().map(|dir| {
                    agy::collect(dir, mtime_floor, config.use_cache, config.local_offset)
                })
            });
            let opencode_handle = scope.spawn(|| {
                config.opencode_dir.as_ref().map(|dir| {
                    opencode::collect(dir, mtime_floor, config.use_cache, config.local_offset)
                })
            });
            let claude_collection = claude::collect(
                &config.claude_dir,
                mtime_floor,
                config.use_cache,
                config.local_offset,
            );
            (
                codex_handle.join(),
                agy_handle.join(),
                opencode_handle.join(),
                cursor_handle.join(),
                claude_collection,
            )
        });

    let mut collections = vec![
        claude_collection,
        codex_result.map_err(|_| anyhow!("Codex collector thread panicked"))?,
    ];
    if let Some(agy) = agy_result.map_err(|_| anyhow!("Antigravity collector thread panicked"))? {
        collections.push(agy);
    }
    if let Some(oc) = opencode_result.map_err(|_| anyhow!("OpenCode collector thread panicked"))? {
        collections.push(oc);
    }
    if let Some(cursor) = cursor_result.map_err(|_| anyhow!("Cursor collector thread panicked"))? {
        collections.push(cursor);
    }

    let providers = collections
        .iter()
        .map(|collection| summarize(collection, now, config.days, config.local_offset))
        .collect::<Vec<_>>();
    let combined = summarize(
        &Collection::combined(PathBuf::from("combined local agent logs"), &collections),
        now,
        config.days,
        config.local_offset,
    );

    Ok(AppSummary {
        generated_at: now,
        period_days: config.days.max(1),
        load_duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        combined,
        providers,
    })
}

fn default_claude_dir() -> Result<PathBuf> {
    Ok(crate::paths::claude_home()?.join("projects"))
}

fn default_codex_dir() -> Result<PathBuf> {
    Ok(crate::paths::codex_home()?.join("sessions"))
}

fn default_agy_dir() -> Result<PathBuf> {
    crate::paths::agy_home()
}

fn default_opencode_dir() -> Result<PathBuf> {
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
fn cursor_config(args: &Args) -> Option<CursorConfig> {
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

fn demo_enabled() -> bool {
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
    use time::macros::date;

    use super::*;
    use crate::model::{Orchestration, Provider, ScanStats, Summary, TokenUsage};

    /// Minimal provider summary carrying just the fields the cost sort reads:
    /// the provider label and a single-day `model_daily` block whose token
    /// volume determines the fallback ordering when pricing is unloaded.
    fn provider_summary(provider: Provider, model: &str, volume: u64) -> Summary {
        let usage = TokenUsage {
            input_tokens: volume,
            ..TokenUsage::default()
        };
        Summary {
            provider,
            period_days: 30,
            period_start: date!(2026 - 05 - 14),
            period_end: date!(2026 - 06 - 12),
            root: PathBuf::new(),
            scan_stats: ScanStats::default(),
            total_usage: usage.clone(),
            recent_window_volume: usage.token_volume(),
            recent_window_active_days: 1,
            daily: Vec::new(),
            daily_sessions: Vec::new(),
            model_daily: vec![crate::model::ModelDailyStat {
                date: date!(2026 - 06 - 12),
                model: model.to_owned(),
                usage: usage.clone(),
                unreported_usage: usage,
                reported_cost_usd: None,
            }],
            models: Vec::new(),
            agents: Vec::new(),
            tools: Vec::new(),
            projects: Vec::new(),
            sessions: 0,
            active_days: 0,
            previous_total_volume: 0,
            longest_streak_days: 0,
            current_streak_days: 0,
            most_active_day: None,
            hourly_usage: [0; 24],
            busiest_hour: None,
            favorite_model: None,
            longest_session: None,
            completion_duration: None,
            orchestration: Orchestration::default(),
        }
    }

    #[test]
    fn providers_sort_most_used_first() {
        // The lighter provider is listed first on input to prove it is reordered
        // to the back by descending token volume.
        let mut providers = vec![
            provider_summary(Provider::Codex, "gpt-5.5", 1_000_000),
            provider_summary(Provider::Claude, "claude-opus-4-8", 9_000_000),
        ];

        sort_providers_by_usage(&mut providers);

        // Heaviest provider lands at tab_index 0 (startup tab).
        assert_eq!(providers[0].provider, Provider::Claude);
        assert_eq!(providers[1].provider, Provider::Codex);
        assert!(
            providers[0].total_usage.token_volume() >= providers[1].total_usage.token_volume(),
            "providers must be ordered by descending token volume"
        );
    }

    #[test]
    fn empty_provider_has_no_tab() {
        // A provider with no tokens, sessions, tools, or completions (a missing
        // or empty log dir) must not earn a tab.
        let empty = provider_summary(Provider::Codex, "gpt-5.5", 0);
        assert!(!provider_has_data(&empty));

        let used = provider_summary(Provider::Claude, "claude-opus-4-8", 1);
        assert!(provider_has_data(&used));
    }
}
