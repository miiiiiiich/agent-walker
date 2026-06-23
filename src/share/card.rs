use std::collections::BTreeMap;
use std::fmt::Write as _;

use time::{Date, Duration};

use crate::cost::usage_cost_usd;
use crate::format::{
    format_duration_ms, format_percent, format_tokens, format_usd, short_model_name,
};
use crate::model::Summary;

use super::REPO_URL;

/// Values rendered on the card, extracted once so the SVG and caption stay
/// in sync.
pub struct ShareCard {
    /// Earned vanity title, e.g. "Eclipse Hawk".
    pub(crate) codename: String,
    /// Time-of-day word ("Aurora"/"Sol"/"Luna"/"Eclipse") — drives the watermark tint.
    pub(crate) ops: String,
    /// Grid animal ("Octopus", …) — selects the watermark silhouette.
    pub(crate) animal: String,
    pub(crate) period_days: u16,
    pub(crate) active_days: usize,
    pub(crate) tokens: String,
    pub(crate) cost: String,
    pub(crate) sessions: usize,
    /// Top models: (short name, share%, ratio-to-largest, `formatted_tokens`).
    pub(crate) models: Vec<(String, String, f64, String)>,
    /// Hour-of-day profile: heights normalized to 0..=1 plus the peak hour.
    pub(crate) hourly: Option<(Vec<f64>, usize, String)>,
    /// Turn-duration buckets (7 counts), (unattended, total), and formatted (p50, p90, max).
    pub(crate) completion: Option<(Vec<usize>, usize, usize, String, String, String)>,
    /// PARALLEL AGENTS: (% of active time at 4+ concurrent, peak concurrency).
    pub(crate) parallel: Option<(u64, usize)>,
    /// Time-weighted average simultaneous sessions (the CONTROL metric).
    pub(crate) avg_concurrency: f64,
    pub(crate) grass: Grass,
}

pub(crate) struct Grass {
    pub(crate) cells: Vec<Vec<Option<usize>>>,
}

impl ShareCard {
    /// Build a card for `summary`. The codename uses `summary` as its own style
    /// source — correct for the combined/Total summary, which is what the CLI
    /// `--share` path passes.
    pub fn from_summary(summary: &Summary) -> Self {
        Self::from_summary_styled(summary, summary)
    }

    /// Like [`Self::from_summary`], but the codename's orchestration tier is
    /// taken from `style_src` (the combined/Total summary) while the rest of the
    /// card describes `summary`. The interactive share path uses this so a
    /// provider tab's card shows the same codename as the tab's badge — the tier
    /// is a whole-person trait (parallelism and tooling across all agents).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "Ratios and percentages are display-only."
    )]
    #[allow(
        clippy::too_many_lines,
        reason = "Flat extraction of every card stat in one pass."
    )]
    pub fn from_summary_styled(summary: &Summary, style_src: &Summary) -> Self {
        let total = summary.total_usage.token_volume();
        let cost: f64 = summary
            .model_daily
            .iter()
            .filter_map(|entry| usage_cost_usd(&entry.model, &entry.usage))
            .sum();
        let parallel = {
            let levels = summary.orchestration.time_by_level;
            // Seconds come from untrusted logs; stay on the saturating discipline
            // the rest of the aggregation uses, even though wall-clock can't realistically overflow.
            let total = levels.iter().copied().fold(0u64, u64::saturating_add);
            (total > 0).then(|| {
                let four_plus = levels[3]
                    .saturating_add(levels[4])
                    .saturating_add(levels[5]);
                let four_plus_pct = (four_plus as f64 / total as f64 * 100.0).round() as u64;
                (four_plus_pct, summary.orchestration.peak_concurrency)
            })
        };

        let max_model = summary
            .models
            .iter()
            .map(|model| model.usage.token_volume())
            .max()
            .unwrap_or(0)
            .max(1);
        let models = summary
            .models
            .iter()
            .filter(|model| model.usage.token_volume() > 0)
            .take(4)
            .map(|model| {
                let vol = model.usage.token_volume();
                (
                    short_model_name(&model.name),
                    format_percent(vol, total.max(1)),
                    vol as f64 / max_model as f64,
                    format_tokens(vol),
                )
            })
            .collect();

        let hourly = summary.busiest_hour.map(|(peak_hour, peak_volume)| {
            let max = summary
                .hourly_usage
                .iter()
                .copied()
                .max()
                .unwrap_or(1)
                .max(1);
            let heights = summary
                .hourly_usage
                .iter()
                .map(|value| *value as f64 / max as f64)
                .collect();
            (
                heights,
                usize::from(peak_hour),
                format!("{peak_hour:02}:00 · {}", format_tokens(peak_volume)),
            )
        });

        let completion = summary.completion_duration.as_ref().map(|duration| {
            let counts: Vec<usize> = duration.buckets.iter().map(|b| b.count).collect();
            let unattended: usize = counts.iter().skip(3).sum();
            (
                counts,
                unattended,
                duration.count,
                format_duration_ms(duration.p50_ms),
                format_duration_ms(duration.p90_ms),
                format_duration_ms(duration.max_ms),
            )
        });

        let codename = crate::codename::for_summary_styled(summary, style_src);
        Self {
            codename: codename.title(),
            ops: codename.ops.to_owned(),
            animal: codename.animal.to_owned(),
            period_days: summary.period_days,
            active_days: summary.active_days,
            tokens: format_tokens(total),
            cost: format_usd(cost),
            sessions: summary.sessions,
            models,
            hourly,
            completion,
            parallel,
            avg_concurrency: summary.orchestration.avg_concurrency,
            grass: Grass::from_summary(summary),
        }
    }

    /// A ready-to-post caption (X body / clipboard text).
    pub fn caption(&self) -> String {
        let mut stats = vec![
            format!("{} tokens", self.tokens),
            format!("{} API-equivalent", self.cost),
        ];
        if let Some((four_plus_pct, peak)) = &self.parallel
            && *four_plus_pct > 0
        {
            stats.push(format!(
                "{four_plus_pct}% with 4+ agents in parallel (peak {peak})"
            ));
        }
        let mut caption = format!(
            "Codename: {}\nMy last {} days with AI coding agents:\n{}.",
            self.codename,
            self.period_days,
            stats.join(" · ")
        );
        if let Some((_, unattended, _, _, _, _)) = &self.completion
            && *unattended > 0
        {
            let _ = write!(caption, "\n{unattended} turns ran 20m+.");
        }
        let _ = write!(
            caption,
            "\n\nTracked 100% locally with agent-walker — your logs never leave your machine.\nhttps://{REPO_URL}"
        );
        caption
    }
}

impl Grass {
    fn from_summary(summary: &Summary) -> Self {
        let value_by_date: BTreeMap<Date, u64> = summary
            .daily
            .iter()
            .map(|stat| (stat.date, stat.usage.token_volume()))
            .collect();
        let thresholds = quartiles(&value_by_date);

        // The card's activity panel is sized for ~5 weeks, and the codename is a
        // 30-day signal — so render only the most recent 30 days. A longer
        // analysis window (e.g. --days 90) would otherwise overflow the grid into
        // the neighbouring charts.
        let start = summary
            .period_start
            .max(summary.period_end.saturating_sub(Duration::days(29)));

        let mut columns: Vec<Vec<Option<usize>>> = Vec::new();
        let mut column = vec![None; 7];
        let mut cursor = start;
        while cursor <= summary.period_end {
            let weekday = usize::from(cursor.weekday().number_days_from_sunday());
            let value = value_by_date.get(&cursor).copied().unwrap_or(0);
            column[weekday] = Some(heat_level(value, &thresholds));
            if weekday == 6 {
                columns.push(std::mem::replace(&mut column, vec![None; 7]));
            }
            cursor = cursor.saturating_add(Duration::days(1));
        }
        if column.iter().any(Option::is_some) {
            columns.push(column);
        }
        Self { cells: columns }
    }
}

fn quartiles(value_by_date: &BTreeMap<Date, u64>) -> [u64; 3] {
    let mut active: Vec<u64> = value_by_date
        .values()
        .copied()
        .filter(|value| *value > 0)
        .collect();
    active.sort_unstable();
    if active.is_empty() {
        return [0, 0, 0];
    }
    let at = |fraction: f64| -> u64 {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "Percentile index on a small sorted vec."
        )]
        let index = ((active.len() - 1) as f64 * fraction).round() as usize;
        active[index]
    };
    [at(0.25), at(0.5), at(0.75)]
}

fn heat_level(value: u64, thresholds: &[u64; 3]) -> usize {
    if value == 0 {
        return 0;
    }
    1 + thresholds.iter().filter(|t| value > **t).count()
}
