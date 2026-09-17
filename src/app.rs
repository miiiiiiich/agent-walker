use std::path::PathBuf;
use std::time::{Duration as StdDuration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow};
use clap::CommandFactory;
use time::{OffsetDateTime, UtcOffset};

use crate::analyzer::summarize;
use crate::collector::{agy, claude, codex, copilot, cursor, grok, opencode};
use crate::model::{AppSummary, Collection};
use crate::ui;

mod config;

pub use config::{ANALYSIS_WINDOW_DAYS, Args, Config};
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
        agy_dir: args.agy_dir.or_else(|| default_agy_dir().ok()),
        copilot_dir: args
            .copilot_dir
            .or_else(|| crate::paths::copilot_home().ok()),
        grok_dir: args.grok_dir.or_else(|| crate::paths::grok_home().ok()),
        opencode_dir: args.opencode_dir.or_else(|| default_opencode_dir().ok()),
        cursor,
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

    if args.json {
        let (mut report, collections) = load_report_with_collections(&config, args.days)?;
        finish_providers(&mut report);
        let stdout = std::io::stdout();
        return crate::format::write_json(
            &mut std::io::BufWriter::new(stdout.lock()),
            &report,
            &collections,
            config.local_offset,
        );
    }

    if let Some(width) = args.render {
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
    let result = load_report_with_collections(config, ANALYSIS_WINDOW_DAYS);
    result.map(|(mut report, _)| {
        finish_providers(&mut report);
        report
    })
}

fn finish_providers(report: &mut AppSummary) {
    report.providers.retain(provider_has_data);
    sort_providers_by_usage(&mut report.providers);
}

fn load_report_with_collections(
    config: &Config,
    days: u16,
) -> Result<(AppSummary, Vec<Collection>)> {
    // CONTEXT uses prices during aggregation, so collection overlaps refresh
    // but every summary waits for the same completed pricing refresh.
    let pricing_refresh = crate::cost::spawn_pricing_refresh();
    load_report_inner(config, days, pricing_refresh)
}

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

/// Fill in the Total-tab fields that are sums over the provider summaries
/// rather than re-derivations from the combined collection. Cache reuse is
/// one: the expiry threshold differs per provider, so the combined
/// collection cannot classify calls itself. Working time is another: only
/// providers with turn durations may contribute tokens to the rate.
pub(crate) fn finish_combined(
    mut combined: crate::model::Summary,
    providers: &[crate::model::Summary],
) -> crate::model::Summary {
    combined.context = crate::model::ContextSummary::merged(
        providers
            .iter()
            .filter_map(|summary| summary.context.as_ref()),
    );
    // Working time is likewise a provider sum: the combined collection
    // would divide every provider's tokens by only the providers that
    // record turn durations.
    combined.active_time = crate::model::ActiveTimeSummary::merged(
        providers
            .iter()
            .filter_map(|summary| summary.active_time.as_ref()),
    );
    combined
}

fn provider_has_data(summary: &crate::model::Summary) -> bool {
    summary.total_usage.token_volume() > 0
        || summary.sessions > 0
        || !summary.tools.is_empty()
        || summary.completion_duration.is_some()
        || summary.interrupted > 0
        || summary
            .context
            .as_ref()
            .is_some_and(|context| context.context_tokens > 0)
        || summary.credits.is_some()
}

fn sort_providers_by_usage(providers: &mut [crate::model::Summary]) {
    providers.sort_by(|left, right| {
        right
            .total_usage
            .token_volume()
            .cmp(&left.total_usage.token_volume())
            .then_with(|| left.provider.label().cmp(right.provider.label()))
    });
}

fn history_days(days: u16) -> u64 {
    u64::from(days) * 2 + 1
}

fn load_report_inner(
    config: &Config,
    days: u16,
    pricing_refresh: std::thread::JoinHandle<()>,
) -> Result<(AppSummary, Vec<Collection>)> {
    if config.demo {
        let _ = pricing_refresh.join();
        return Ok(crate::demo::demo_report_with_collections(config, days));
    }

    let started = Instant::now();
    let now = OffsetDateTime::now_utc().to_offset(config.local_offset);

    let mtime_floor =
        SystemTime::now().checked_sub(StdDuration::from_secs(history_days(days) * 86_400));

    if config.use_cache {
        crate::collector::sweep_cache_dir();
    }
    let collections = collect_all(config, mtime_floor)?;
    // Join the pricing refresh so every summary below prices the same way.
    let _ = pricing_refresh.join();

    let providers = collections
        .iter()
        .map(|collection| summarize(collection, now, days, config.local_offset))
        .collect::<Vec<_>>();
    let combined = finish_combined(
        summarize(
            &Collection::combined(PathBuf::from("combined local agent logs"), &collections),
            now,
            days,
            config.local_offset,
        ),
        &providers,
    );

    Ok((
        AppSummary {
            generated_at: now,
            period_days: days,
            load_duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            combined,
            providers,
        },
        collections,
    ))
}

#[cfg(test)]
mod tests {
    use time::macros::date;

    use super::*;
    use crate::model::{Orchestration, Provider, ScanStats, Summary, TokenUsage};

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
            context: None,
            active_time: None,
            orchestration: Orchestration::default(),
        }
    }

    #[test]
    fn scan_history_tracks_the_requested_window() {
        assert_eq!(history_days(1), 3);
        assert_eq!(history_days(30), 61);
        assert_eq!(history_days(90), 181);
        assert_eq!(history_days(u16::MAX), 131_071);
    }

    #[test]
    fn providers_sort_most_used_first() {
        let mut providers = vec![
            provider_summary(Provider::Codex, "gpt-5.5", 1_000_000),
            provider_summary(Provider::Claude, "claude-opus-4-8", 9_000_000),
        ];

        sort_providers_by_usage(&mut providers);

        assert_eq!(providers[0].provider, Provider::Claude);
        assert_eq!(providers[1].provider, Provider::Codex);
        assert!(
            providers[0].total_usage.token_volume() >= providers[1].total_usage.token_volume(),
            "providers must be ordered by descending token volume"
        );
    }

    #[test]
    fn empty_provider_has_no_tab() {
        let empty = provider_summary(Provider::Codex, "gpt-5.5", 0);
        assert!(!provider_has_data(&empty));

        let used = provider_summary(Provider::Claude, "claude-opus-4-8", 1);
        assert!(provider_has_data(&used));

        let mut interrupted_only = provider_summary(Provider::Codex, "gpt-5.5", 0);
        interrupted_only.interrupted = 2;
        assert!(provider_has_data(&interrupted_only));

        let mut context_only = provider_summary(Provider::Codex, "gpt-5.5", 0);
        context_only.context = Some(crate::model::ContextSummary {
            calls: 0,
            context_tokens: 3,
            ..crate::model::ContextSummary::default()
        });
        assert!(provider_has_data(&context_only));
    }
}
