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

    #[arg(long, value_name = "DIR")]
    pub agy_dir: Option<PathBuf>,

    /// Also collect Antigravity (agy) logs. Off by default: Antigravity's logs
    /// expose no token usage — it lives in an unlabeled protobuf store
    /// (`conversations/*.db`), so counts would be misleading. The parser is kept
    /// for when that store becomes readable.
    #[arg(long)]
    pub agy: bool,

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
    pub agy_dir: PathBuf,
    /// Antigravity collection is opt-in (`--agy`); off by default because the
    /// logs carry no token usage.
    pub agy: bool,
    pub days: u16,
    pub use_cache: bool,
    /// Local UTC offset captured at startup (single-threaded moment), used to
    /// bucket all timestamps into the user's local days and hours.
    pub local_offset: UtcOffset,
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
    let config = Config {
        demo: demo_enabled(),
        // `map_or_else(default, Ok)` keeps the default lazy, so a `--claude-dir`
        // / `--codex-dir` / `--agy-dir` override on the CLI still works even
        // when `dirs::home_dir()` can't resolve (sandbox / no `$HOME` /
        // `%USERPROFILE%`). Eagerly calling `default_*_dir()?` would short-
        // circuit before the CLI override ever got a chance.
        claude_dir: args.claude_dir.map_or_else(default_claude_dir, Ok)?,
        codex_dir: args.codex_dir.map_or_else(default_codex_dir, Ok)?,
        agy_dir: args.agy_dir.map_or_else(default_agy_dir, Ok)?,
        agy: args.agy,
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
    let pricing_refresh = crate::cost::spawn_pricing_refresh();
    let result = load_report_inner(config);
    // Pricing must be loaded before sorting providers by cost, so join first.
    let _ = pricing_refresh.join();
    result.map(|mut report| {
        sort_providers_by_cost(&mut report.providers);
        report
    })
}

/// Order the provider tabs by API-equivalent cost, descending, so the
/// heaviest-spend provider lands at `tab_index 0` (the startup tab). Ties break
/// on token volume. With pricing unloaded every cost is 0, so this degrades to
/// a stable token-volume ordering. The Combined tab is not in `providers`; it
/// stays appended at the end of the tab strip.
fn sort_providers_by_cost(providers: &mut [crate::model::Summary]) {
    providers.sort_by(|left, right| {
        provider_cost(right)
            .partial_cmp(&provider_cost(left))
            .unwrap_or(core::cmp::Ordering::Equal)
            .then_with(|| {
                right
                    .total_usage
                    .token_volume()
                    .cmp(&left.total_usage.token_volume())
            })
    });
}

/// Summed API-equivalent cost of a provider over the display window, from its
/// per-model-per-day usage. Unpriced models contribute nothing.
fn provider_cost(summary: &crate::model::Summary) -> f64 {
    summary
        .model_daily
        .iter()
        .filter_map(|entry| crate::cost::usage_cost_usd(&entry.model, &entry.usage))
        .sum::<f64>()
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

    let (codex_result, (agy_result, claude_collection)) = std::thread::scope(|scope| {
        let codex_handle = scope.spawn(|| {
            codex::collect(
                &config.codex_dir,
                mtime_floor,
                config.use_cache,
                config.local_offset,
            )
        });
        // Antigravity is opt-in (`--agy`): its logs carry no token usage, so it
        // is left out of the default report rather than skewing the totals.
        let agy_handle = scope.spawn(|| {
            config.agy.then(|| {
                agy::collect(
                    &config.agy_dir,
                    mtime_floor,
                    config.use_cache,
                    config.local_offset,
                )
            })
        });
        let claude_collection = claude::collect(
            &config.claude_dir,
            mtime_floor,
            config.use_cache,
            config.local_offset,
        );
        (codex_handle.join(), (agy_handle.join(), claude_collection))
    });

    let mut collections = vec![
        claude_collection,
        codex_result.map_err(|_| anyhow!("Codex collector thread panicked"))?,
    ];
    if let Some(agy) = agy_result.map_err(|_| anyhow!("Antigravity collector thread panicked"))? {
        collections.push(agy);
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
                usage,
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
    fn providers_sort_highest_cost_first() {
        // No pricing is loaded in the test harness, so every provider cost is 0
        // and the sort falls back to descending token volume. The lighter
        // provider is listed first on input to prove it is reordered to the back.
        let mut providers = vec![
            provider_summary(Provider::Codex, "gpt-5.5", 1_000_000),
            provider_summary(Provider::Claude, "claude-opus-4-8", 9_000_000),
        ];

        sort_providers_by_cost(&mut providers);

        // Heaviest provider lands at tab_index 0 (startup tab).
        assert_eq!(providers[0].provider, Provider::Claude);
        assert_eq!(providers[1].provider, Provider::Codex);
        assert!(
            providers[0].total_usage.token_volume() >= providers[1].total_usage.token_volume(),
            "providers must be ordered by descending volume in the no-pricing fallback"
        );
    }
}
