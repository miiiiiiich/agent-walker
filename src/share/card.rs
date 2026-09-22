use std::collections::BTreeMap;
use std::fmt::Write as _;

use time::{Date, Duration};

use crate::cost::CostTally;
use crate::format::{
    format_date, format_duration_ms, format_hours, format_percent, format_tokens, format_usd,
    short_model_name,
};
use crate::model::Summary;

use super::REPO_URL;

/// Values rendered on the card, extracted once so the SVG and caption stay
/// in sync.
pub struct ShareCard {
    pub(crate) codename: String,
    pub(crate) ops: String,
    pub(crate) animal: String,
    pub(crate) rank: crate::codename::Rank,
    pub(crate) period_days: u16,
    pub(crate) active_days: usize,
    pub(crate) tokens: String,
    pub(crate) tokens_per_day: String,
    pub(crate) working: Option<(String, String)>,
    /// Formatted API-equivalent cost, or `None` when some of the window's
    /// tokens had no known price (LiteLLM table unreachable / model id
    /// missing) — the card then shows "—" rather than an undercounted figure.
    pub(crate) cost: Option<String>,
    pub(crate) cost_per_day: Option<String>,
    /// True when the window contains a provider-reported cost (Cursor) — an
    /// actual charge fetched over the network, not an API-equivalent estimate
    /// from local logs. Independent of whether `cost` could be shown (other
    /// usage may be unpriced); softens the caption's "API-equivalent / 100%
    /// local" copy either way.
    pub(crate) has_reported_cost: bool,
    pub(crate) cached: Option<String>,
    pub(crate) tokens_per_min: Option<String>,
    pub(crate) sessions: usize,
    pub(crate) model_count: usize,
    pub(crate) period: (String, String),
    pub(crate) models: Vec<(String, String, f64, String)>,
    pub(crate) hourly: Option<(Vec<f64>, usize, String)>,
    pub(crate) completion: Option<(Vec<usize>, usize, usize, String, String, String)>,
    pub(crate) parallel: Option<(u64, usize)>,
    pub(crate) avg_concurrency: f64,
    pub(crate) grass: Grass,
}

pub(crate) struct Grass {
    pub(crate) cells: Vec<Vec<Option<usize>>>,
}

impl ShareCard {
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
    pub fn from_summary(summary: &Summary) -> Self {
        let total = summary.total_usage.token_volume();
        let mut tally = CostTally::default();
        for entry in &summary.model_daily {
            tally.add(
                &entry.model,
                &entry.unreported_usage,
                entry.reported_cost_usd,
            );
        }
        let has_reported_cost = summary
            .model_daily
            .iter()
            .any(|entry| entry.reported_cost_usd.is_some());
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

        // Rows merge by printed label: dated and undated ids of one model
        // would otherwise draw the same name twice next to a count of one.
        let mut by_label: BTreeMap<String, u64> = BTreeMap::new();
        for model in &summary.models {
            let vol = model.usage.token_volume();
            if vol > 0 {
                let entry = by_label.entry(short_model_name(&model.name)).or_default();
                *entry = entry.saturating_add(vol);
            }
        }
        let model_count = by_label.len();
        let mut by_label: Vec<(String, u64)> = by_label.into_iter().collect();
        by_label.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let max_model = by_label.first().map_or(1, |(_, vol)| *vol).max(1);
        let models = by_label
            .into_iter()
            .take(4)
            .map(|(name, vol)| {
                (
                    name,
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

        let days = u64::from(summary.period_days.max(1));
        let complete_usd = tally.complete_usd();

        let codename = crate::codename::for_summary(summary);
        Self {
            codename: codename.title(),
            ops: codename.ops.to_owned(),
            animal: codename.animal.to_owned(),
            rank: codename.rank,
            period_days: summary.period_days,
            active_days: summary.active_days,
            tokens: format_tokens(total),
            tokens_per_day: format_tokens(total / days),
            working: summary.active_time.as_ref().map(|time| {
                (
                    format_hours(time.active_ms),
                    format_hours(time.active_per_day_ms()),
                )
            }),
            cost: complete_usd.map(format_usd),
            cost_per_day: complete_usd.map(|usd| format_usd(usd / days as f64)),
            has_reported_cost,
            // Every section reads the same window, so the cache share is on the
            // same footing as the rest of the card.
            cached: summary
                .context
                .as_ref()
                .filter(|context| context.context_tokens > 0)
                .map(|context| format!("{:.0}%", context.cached_share() * 100.0)),
            tokens_per_min: summary
                .active_time
                .as_ref()
                .and_then(crate::model::ActiveTimeSummary::tokens_per_minute)
                .map(format_tokens),
            sessions: summary.sessions,
            model_count,
            period: (
                format_date(Grass::start(summary)),
                format_date(summary.period_end),
            ),
            models,
            hourly,
            completion,
            parallel,
            avg_concurrency: summary.orchestration.avg_concurrency,
            grass: Grass::from_summary(summary),
        }
    }

    pub fn caption(&self) -> String {
        const MAX_WEIGHT: usize = 280;
        // Cursor's reported cost is an actual charge, not an API-equivalent
        // estimate, so don't label it as one. An unknown cost is simply
        // absent — a "$0" would misread as free.
        let cost = self.cost.as_ref().map(|cost| {
            if self.has_reported_cost {
                format!("{cost} cost")
            } else {
                format!("{cost} API-equivalent")
            }
        });
        let parallel = self
            .parallel
            .as_ref()
            .filter(|(four_plus_pct, _)| *four_plus_pct > 0)
            .map(|(four_plus_pct, peak)| {
                format!("{four_plus_pct}% with 4+ agents in parallel (peak {peak})")
            });
        let unattended = self
            .completion
            .as_ref()
            .map(|(_, unattended, ..)| *unattended)
            .filter(|unattended| *unattended > 0);

        let build = |cached: bool, parallel_on: bool, unattended_on: bool| {
            let mut stats = vec![format!("{} tokens", self.tokens)];
            stats.extend(cost.clone());
            if cached {
                stats.extend(self.cached.as_ref().map(|share| format!("{share} cached")));
            }
            if parallel_on {
                stats.extend(parallel.clone());
            }
            let rank_tag = self
                .rank
                .letters()
                .map(|letters| format!(" — Rank {letters}"))
                .unwrap_or_default();
            let mut caption = format!(
                "Codename: {}{rank_tag}\nMy last {} days with AI coding agents:\n{}.",
                self.codename,
                self.period_days,
                stats.join(" · ")
            );
            if unattended_on && let Some(unattended) = unattended {
                let _ = write!(caption, "\n{unattended} turns ran 20m+.");
            }
            // The "100% local / logs never leave" claim only holds without
            // Cursor, whose usage is fetched from its dashboard over the network.
            let provenance = if self.has_reported_cost {
                "Tracked with agent-walker (Cursor usage read from its dashboard)."
            } else {
                "Tracked 100% locally with agent-walker — your logs never leave your machine."
            };
            let _ = write!(caption, "\n\n{provenance}\nhttps://{REPO_URL}");
            caption
        };
        [
            (true, true, true),
            (false, true, true),
            (false, false, true),
            (false, false, false),
        ]
        .into_iter()
        .map(|(cached, parallel_on, unattended_on)| build(cached, parallel_on, unattended_on))
        .find(|caption| x_weight(caption) <= MAX_WEIGHT)
        .unwrap_or_else(|| build(false, false, false))
    }
}

impl Grass {
    /// The activity grid fits the most recent 30 days; longer analysis
    /// windows are clipped here to avoid overflowing neighbouring charts.
    fn start(summary: &Summary) -> Date {
        summary
            .period_start
            .max(summary.period_end.saturating_sub(Duration::days(29)))
    }

    fn from_summary(summary: &Summary) -> Self {
        let value_by_date: BTreeMap<Date, u64> = summary
            .daily
            .iter()
            .map(|stat| (stat.date, stat.usage.token_volume()))
            .collect();
        let thresholds = quartiles(&value_by_date);

        let start = Self::start(summary);

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

// Weight-1 ranges from twitter-text config v3; everything else weighs 2.
pub(super) fn x_weight(text: &str) -> usize {
    const URL_WEIGHT: usize = 23;
    let single = |c: char| {
        let cp = u32::from(c);
        (0..=4351).contains(&cp)
            || (8192..=8205).contains(&cp)
            || (8208..=8223).contains(&cp)
            || (8242..=8247).contains(&cp)
    };
    let per_char = |t: &str| {
        t.chars()
            .map(|c| if single(c) { 1 } else { 2 })
            .sum::<usize>()
    };
    let mut weight = per_char(text);
    for word in text.split_whitespace() {
        if word.starts_with("https://") || word.starts_with("http://") {
            weight = weight - per_char(word) + URL_WEIGHT;
        }
    }
    weight
}
