use std::path::PathBuf;
use std::time::{Duration as StdDuration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use clap::CommandFactory;
use time::{OffsetDateTime, UtcOffset};

use crate::analyzer::summarize;
use crate::collector::{agy, claude, codex, copilot, cursor, grok, opencode};
use crate::format::snapshot_app;
use crate::model::{AppSummary, Collection};
use crate::ui;

mod config;

pub use config::{Args, Config};
use config::{
    cursor_config, default_agy_dir, default_claude_dir, default_codex_dir, default_opencode_dir,
    demo_enabled,
};

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
        copilot_dir: args
            .copilot_dir
            .or_else(|| crate::paths::copilot_home().ok()),
        grok_dir: args.grok_dir.or_else(|| crate::paths::grok_home().ok()),
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

/// Run every collector (threaded where independent) and keep the providers
/// that produced anything. Split out of `load_report_inner` so the report
/// assembly stays readable as providers accumulate.
fn collect_all(config: &Config, mtime_floor: Option<SystemTime>) -> Result<Vec<Collection>> {
    let (
        codex_result,
        agy_result,
        opencode_result,
        copilot_result,
        grok_result,
        cursor_result,
        claude_collection,
    ) =
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
            // becomes a tab.
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
            let copilot_handle = scope.spawn(|| {
                config.copilot_dir.as_ref().map(|dir| {
                    copilot::collect(dir, mtime_floor, config.use_cache, config.local_offset)
                })
            });
            let grok_handle = scope.spawn(|| {
                config.grok_dir.as_ref().map(|dir| {
                    grok::collect(dir, mtime_floor, config.use_cache, config.local_offset)
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
                copilot_handle.join(),
                grok_handle.join(),
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
    if let Some(cp) = copilot_result.map_err(|_| anyhow!("Copilot collector thread panicked"))? {
        collections.push(cp);
    }
    if let Some(gk) = grok_result.map_err(|_| anyhow!("Grok collector thread panicked"))? {
        collections.push(gk);
    }
    if let Some(cursor) = cursor_result.map_err(|_| anyhow!("Cursor collector thread panicked"))? {
        collections.push(cursor);
    }
    Ok(collections)
}

/// A provider earns a tab only if it has real activity. Token volume catches
/// most agents; sessions, tools, completions, interruptions, and the
/// fixed-window credit ledger are the fallback for a session that logged
/// activity but no usage tokens (Copilot credits cut on the fixed 30-day
/// window, so they can exist even when a short `--days` display window is
/// empty). Everything false ⇒ the directory was missing or empty, so the tab
/// is dropped instead of showing a blank tab.
fn provider_has_data(summary: &crate::model::Summary) -> bool {
    summary.total_usage.token_volume() > 0
        || summary.sessions > 0
        || !summary.tools.is_empty()
        || summary.completion_duration.is_some()
        || summary.interrupted > 0
        || summary.credits.is_some()
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

    let collections = collect_all(config, mtime_floor)?;

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
            skills: Vec::new(),
            limits: None,
            credits: None,
            modes: crate::model::ModesSummary::default(),
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
            interrupted: 0,
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
        // A provider with no tokens, sessions, tools, completions, or
        // interruptions (a missing or empty log dir) must not earn a tab.
        let empty = provider_summary(Provider::Codex, "gpt-5.5", 0);
        assert!(!provider_has_data(&empty));

        let used = provider_summary(Provider::Claude, "claude-opus-4-8", 1);
        assert!(provider_has_data(&used));

        // Interruptions alone are activity: an all-aborted window keeps the tab.
        let mut interrupted_only = provider_summary(Provider::Codex, "gpt-5.5", 0);
        interrupted_only.interrupted = 2;
        assert!(provider_has_data(&interrupted_only));
    }
}
