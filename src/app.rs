use std::env;
use std::path::PathBuf;
use std::time::{Duration as StdDuration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use time::{OffsetDateTime, UtcOffset};

use crate::analyzer::summarize;
use crate::collector::{agy, claude, codex};
use crate::format::snapshot_app;
use crate::model::{AppSummary, Collection, Summary};
use crate::ui;

#[derive(Debug, Parser)]
#[command(
    name = "agentwalker",
    bin_name = "agentwalker",
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

    #[arg(long, value_name = "DIR")]
    pub agy_dir: Option<PathBuf>,

    #[arg(long, default_value_t = 90, value_name = "DAYS")]
    pub days: u16,

    /// Ignore the per-file parse cache and rescan everything.
    #[arg(long)]
    pub no_cache: bool,

    /// Never touch the network (skips the daily model-pricing refresh).
    #[arg(long)]
    pub offline: bool,

    /// Render a synthetic demo dashboard; no logs are read.
    #[arg(long)]
    pub demo: bool,

    /// Print shell completions for the given shell and exit.
    #[arg(long, value_enum, value_name = "SHELL")]
    pub completions: Option<Shell>,

    /// Refresh the vendored pricing snapshot from `LiteLLM` and exit
    /// (release tooling).
    #[arg(long, hide = true, value_name = "PATH")]
    pub update_pricing_snapshot: Option<PathBuf>,

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
    pub agy_dir: PathBuf,
    pub days: u16,
    pub use_cache: bool,
    /// Local UTC offset captured at startup (single-threaded moment), used to
    /// bucket all timestamps into the user's local days and hours.
    pub local_offset: UtcOffset,
}

pub fn run(args: Args) -> Result<()> {
    if let Some(path) = args.update_pricing_snapshot {
        crate::cost::update_snapshot_file(&path)?;
        println!("wrote {}", path.display());
        return Ok(());
    }

    if let Some(shell) = args.completions {
        clap_complete::generate(
            shell,
            &mut Args::command(),
            "agentwalker",
            &mut std::io::stdout(),
        );
        return Ok(());
    }

    // Must be read before any worker threads exist; `time` refuses to probe
    // the environment for the local offset once the process is multithreaded.
    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let config = Config {
        demo: args.demo,
        claude_dir: args.claude_dir.unwrap_or(default_claude_dir()?),
        codex_dir: args.codex_dir.unwrap_or(default_codex_dir()?),
        agy_dir: args.agy_dir.unwrap_or(default_agy_dir()?),
        days: args.days,
        use_cache: !args.no_cache,
        local_offset,
    };

    crate::cost::refresh_pricing_in_background(args.offline);

    if args.snapshot {
        let report = load_report(&config)?;
        println!("{}", snapshot_app(&report));
        return Ok(());
    }

    if let Some(width) = args.render {
        for tab_index in 0..4 {
            println!(
                "{}",
                ui::render_text(&config, width.max(40), 44, tab_index)?
            );
        }
        return Ok(());
    }

    ui::run(config)
}

pub fn load_report(config: &Config) -> Result<AppSummary> {
    if config.demo {
        return Ok(crate::demo::demo_report(config));
    }
    let started = Instant::now();
    let now = OffsetDateTime::now_utc().to_offset(config.local_offset);

    // Files whose last write predates the previous-period window (used for
    // deltas; minus a day of slack for timezone skew) cannot contain
    // relevant events.
    let mtime_floor = SystemTime::now().checked_sub(StdDuration::from_secs(
        (u64::from(config.days.max(1)) * 2 + 1) * 86_400,
    ));

    let (codex_result, (agy_result, claude_result)) = std::thread::scope(|scope| {
        let codex_handle = scope.spawn(|| {
            codex::collect(&config.codex_dir, mtime_floor, config.use_cache)
                .with_context(|| format!("collect Codex logs from {}", config.codex_dir.display()))
        });
        let agy_handle = scope.spawn(|| {
            agy::collect(
                &config.agy_dir,
                mtime_floor,
                config.use_cache,
                config.local_offset,
            )
            .with_context(|| format!("collect Antigravity logs from {}", config.agy_dir.display()))
        });
        let claude_result = claude::collect(&config.claude_dir, mtime_floor, config.use_cache)
            .with_context(|| {
                format!(
                    "collect Claude Code logs from {}",
                    config.claude_dir.display()
                )
            });
        (codex_handle.join(), (agy_handle.join(), claude_result))
    });

    let collections = vec![
        claude_result?,
        codex_result.map_err(|_| anyhow!("Codex collector thread panicked"))??,
        agy_result.map_err(|_| anyhow!("Antigravity collector thread panicked"))??,
    ];

    let providers = collections
        .iter()
        .cloned()
        .map(|collection| summarize(collection, now, config.days, config.local_offset))
        .collect::<Vec<_>>();
    let combined = summarize(
        Collection::combined(PathBuf::from("combined local agent logs"), &collections),
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

pub fn load_summary(config: &Config) -> Result<Summary> {
    Ok(load_report(config)?.combined)
}

fn default_claude_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude").join("projects"))
}

fn default_codex_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".codex").join("sessions"))
}

fn default_agy_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".gemini").join("antigravity-cli"))
}

fn home_dir() -> Result<PathBuf> {
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home))
}
