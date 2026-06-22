//! Codename: a playful vanity title derived from a usage `Summary`.
//!
//! Shown as `[OPS] [ANIMAL]`, e.g. "Eclipse Hawk". The ANIMAL encodes a 6×4
//! grid — **ROW = token throughput (the level), COLUMN = working style**
//! (`Control` = parallel / `Solo` = heavy / `Scout` = neither / `AllRounder`
//! = parallel + heavy + multi-model) — so one word carries both how much and
//! what kind. OPS is the dominant time-of-day. "Chick" is the no-data floor.
//!
//! ROW and STYLE are independent: ROW is volume (tokens/day over the most
//! recent 30 days, fixed regardless of the display `--days`), while STYLE uses
//! flat per-axis thresholds that don't change with the row. The style axes are
//! rates/ratios over the analysis window.
//!
//! All numeric cut-offs live in the one block below and are meant to be easy to
//! retune. They are never surfaced in the UI — only the final title is — so the
//! formula stays opaque to ordinary users even though the source is public.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Scores are display-only thresholds; approximate float math never feeds back into integer state."
)]

use crate::model::Summary;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Parallel — many sessions running at once.
    Control,
    /// Heavy — runs many long, unattended tasks.
    Solo,
    /// Neither parallel nor heavy — a plain / general way of working.
    Scout,
    /// Parallel AND heavy AND multi-model at once — the orchestration profile.
    /// The displayed row still comes from token throughput, so a low-throughput
    /// all-rounder lands at a low row. Rare by construction.
    AllRounder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codename {
    /// Time-of-day word: "Aurora" / "Sol" / "Luna" / "Eclipse".
    pub ops: &'static str,
    /// Grid animal, or "Chick" for the no-data floor.
    pub animal: &'static str,
}

impl Codename {
    /// The displayed title. Chick (no data) carries no time prefix.
    pub fn title(&self) -> String {
        if self.animal == CHICK {
            CHICK.to_owned()
        } else {
            format!("{} {}", self.ops, self.animal)
        }
    }
}

const CHICK: &str = "Chick";

// ===== Tunable thresholds — single source of truth =========================
// Every axis is a rate or ratio, so the title is window-robust. The level is
// tokens/day over the most recent 30 days (longer `--days` is clipped back, so
// the level stays pinned even while graphs span 90 days). Token counts are
// cache-inclusive and ~90%+ cache reads for nearly everyone, so the bar is set
// on gross throughput rather than pricing cache separately. Calibrated so a
// heavy power user (top low-single-digit %) lands around R3, leaving R2 and R1
// as headroom for the rare documented extremes. Retune here only; nothing else
// hard-codes a number.

/// The level always reflects the most recent N days of token throughput.
/// The analyzer fills `Summary::recent_window_volume` over this same window.
pub(crate) const CODENAME_WINDOW_DAYS: i64 = 30;

// ROW is volume; STYLE is how you work. They are independent — STYLE uses a
// single flat threshold per axis that does NOT change with the row, so "what
// kind of user" reads the same whether you're R1 or R6.

/// ROW (R1 top .. R6 entry) by tokens per day over the window. Volume only.
const TOKENS_PER_DAY: [f64; 6] = [
    750_000_000.0, // R1
    400_000_000.0, // R2
    150_000_000.0, // R3
    45_000_000.0,  // R4
    12_000_000.0,  // R5
    500_000.0,     // R6
];

/// STYLE thresholds — single flat cut-offs, row-independent.
/// Parallel: weighted-average simultaneous sessions. At/above this you "run
/// things in parallel".
const PARALLEL_MIN: f64 = 1.8;
/// Solo: 20m+ unattended completions per active day. At/above this you "leave
/// long tasks running".
const SOLO_MIN: f64 = 1.8;
/// Multi-model: the smaller provider's share of `Claude`+`Codex` volume.
/// At/above this you're not leaning on a single model. Required for `AllRounder`.
const MULTI_MIN: f64 = 0.05;

/// Below either floor the user is "Chick" (no real data yet).
const CHICK_MIN_TOKENS_PER_DAY: f64 = 500_000.0;
const CHICK_MIN_DAYS: usize = 3;

/// OPS is decided when the top time-band leads the second by this many points;
/// otherwise the day is "mixed" → Eclipse.
const OPS_DOMINANCE_PT: f64 = 15.0;

/// 6×4 animal grid: `[row R1..R6][Control, Solo, Scout, AllRounder]`. The row
/// is volume; the column is working style. Scout is the no-specialization
/// column (neither parallel nor heavy), so it reads as the "general" use.
const GRID: [[&str; 4]; 6] = [
    ["Hound", "Fox", "Doberman", "Lion"],    // R1
    ["Octopus", "Wolf", "Orca", "Hawk"],     // R2
    ["Raven", "Puma", "Whale", "Swallow"],   // R3
    ["Scorpion", "Piranha", "Bear", "Gull"], // R4
    ["Cat", "Kangaroo", "Eel", "Deer"],      // R5
    ["Ant", "Firefly", "Butterfly", "Bee"],  // R6
];

// ===========================================================================

struct Metrics {
    control: f64,              // weighted avg simultaneous sessions (parallel)
    heavy_per_day: f64,        // 20m+ unattended runs per active day (heavy)
    multi_model_share: f64,    // smaller provider's share of Claude+Codex volume
    tokens_per_day: f64,       // tokens/day over the most recent window (level)
    window_active_days: usize, // active days over the fixed 30-day window (Chick floor)
}

/// Public entry: derive the codename for a summary. Computed on demand at
/// display time, never stored, so the analyzer stays free of vanity logic.
pub fn for_summary(summary: &Summary) -> Codename {
    for_summary_styled(summary, summary)
}

/// Like [`for_summary`], but the multi-model signal is taken from `style_src`
/// while everything else — parallel, heavy, the row, the OPS prefix, the Chick
/// floor — comes from `summary` itself.
///
/// The UI passes the combined summary as `style_src`. Parallel and heavy stay
/// per-tab so a tab reflects how that agent is actually run (a tool you never
/// parallelise won't read as `AllRounder`). Only the multi-model axis is a
/// cross-tool trait the per-provider summaries can't see — they leave it at 0 —
/// so it's sourced from the combined summary. A tab thus earns `AllRounder` only
/// when you genuinely run *that* agent in parallel for long stretches and you're
/// multi-model overall. When `summary` is the combined one this is identical to
/// [`for_summary`].
pub fn for_summary_styled(summary: &Summary, style_src: &Summary) -> Codename {
    let mut m = metrics(summary);
    if is_chick(&m) {
        return Codename {
            ops: "Eclipse",
            animal: CHICK,
        };
    }
    let row = band_row(m.tokens_per_day, &TOKENS_PER_DAY).clamp(1, 6);
    // Multi-model is the only cross-tool axis; borrow it from the combined view.
    m.multi_model_share = style_src.recent_window_provider_min_share;
    let style = style_of(&m);
    Codename {
        ops: ops(&summary.hourly_usage),
        animal: animal_for(style, row),
    }
}

fn metrics(summary: &Summary) -> Metrics {
    let active_days = summary.active_days;

    let unattended = summary.completion_duration.as_ref().map_or(0, |duration| {
        // The duration histogram is laid out as three sub-20m buckets followed
        // by the three 20m+ "unattended" buckets, so skipping the first three
        // isolates the tail without coupling scoring to display label strings.
        duration
            .buckets
            .iter()
            .skip(3)
            .map(|bucket| bucket.count)
            .sum::<usize>()
    });
    let heavy_per_day = if active_days > 0 {
        unattended as f64 / active_days as f64
    } else {
        0.0
    };

    Metrics {
        control: summary.orchestration.avg_concurrency,
        heavy_per_day,
        multi_model_share: summary.recent_window_provider_min_share,
        tokens_per_day: tokens_per_day(summary),
        window_active_days: summary.recent_window_active_days,
    }
}

/// Tokens per day over the fixed codename window. `recent_window_volume` is the
/// analyzer's last-30-day token sum, computed independently of the display
/// `--days`, so the level is the same whether the user views 7, 30, or 90 days.
fn tokens_per_day(summary: &Summary) -> f64 {
    summary.recent_window_volume as f64 / CODENAME_WINDOW_DAYS as f64
}

/// Below either floor the user has no real data yet → Chick.
fn is_chick(m: &Metrics) -> bool {
    m.tokens_per_day < CHICK_MIN_TOKENS_PER_DAY || m.window_active_days < CHICK_MIN_DAYS
}

/// The working-style column — how the agents are run, independent of volume.
/// The displayed row still comes from token throughput, so a low-throughput
/// all-rounder lands at a low row.
///
/// - `AllRounder` — parallel AND heavy AND multi-model. The orchestration
///   profile: many at once, long unattended runs, and not on a single model.
/// - `Control` — parallel but not heavy (or both, but single-model).
/// - `Solo` — heavy but not parallel.
/// - `Scout` — neither: a plain / general way of working.
fn style_of(m: &Metrics) -> Style {
    let parallel = m.control >= PARALLEL_MIN;
    let heavy = m.heavy_per_day >= SOLO_MIN;
    let multi = m.multi_model_share >= MULTI_MIN;

    match (parallel, heavy) {
        (true, true) if multi => Style::AllRounder,
        // Both specialised but single-model: fall to the stronger of the two.
        // Cross-multiplied form of `heavy/SOLO_MIN > control/PARALLEL_MIN` —
        // avoids float division and stays correct if the thresholds ever diverge
        // or go to zero. Ties favour Control.
        (true, true) => {
            if m.heavy_per_day * PARALLEL_MIN > m.control * SOLO_MIN {
                Style::Solo
            } else {
                Style::Control
            }
        }
        (true, false) => Style::Control,
        (false, true) => Style::Solo,
        (false, false) => Style::Scout,
    }
}

/// Combined view used by the tests: `None` => Chick, otherwise `(style, row
/// 1..=6)`. ROW is volume only; STYLE is how the agents are run. They are
/// independent.
#[cfg(test)]
fn classify(m: &Metrics) -> Option<(Style, usize)> {
    if is_chick(m) {
        return None;
    }
    let row = band_row(m.tokens_per_day, &TOKENS_PER_DAY).clamp(1, 6);
    Some((style_of(m), row))
}

/// First row (1..=6) whose threshold `raw` clears, or 7 if below the grid.
fn band_row(raw: f64, thresholds: &[f64; 6]) -> usize {
    for (index, threshold) in thresholds.iter().enumerate() {
        if raw >= *threshold {
            return index + 1;
        }
    }
    7
}

fn animal_for(style: Style, row: usize) -> &'static str {
    let column = match style {
        Style::Control => 0,
        Style::Solo => 1,
        Style::Scout => 2,
        Style::AllRounder => 3,
    };
    GRID[row.clamp(1, 6) - 1][column]
}

/// Dominant time-of-day word from the hourly token histogram.
fn ops(hourly: &[u64; 24]) -> &'static str {
    let mut aurora = 0u64; // 05–10
    let mut sol = 0u64; // 11–17
    let mut luna = 0u64; // 18–04
    for (hour, value) in hourly.iter().enumerate() {
        if (5..11).contains(&hour) {
            aurora = aurora.saturating_add(*value);
        } else if (11..18).contains(&hour) {
            sol = sol.saturating_add(*value);
        } else {
            luna = luna.saturating_add(*value);
        }
    }
    let total = aurora.saturating_add(sol).saturating_add(luna);
    if total == 0 {
        return "Eclipse";
    }
    let mut bands = [("Aurora", aurora), ("Sol", sol), ("Luna", luna)];
    bands.sort_by(|left, right| right.1.cmp(&left.1));
    let top = bands[0].1 as f64 / total as f64 * 100.0;
    let second = bands[1].1 as f64 / total as f64 * 100.0;
    if top - second >= OPS_DOMINANCE_PT {
        bands[0].0
    } else {
        "Eclipse"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Metrics {
        Metrics {
            control: 1.0,
            heavy_per_day: 0.0,
            multi_model_share: 0.0,
            tokens_per_day: 200_000_000.0, // R3
            window_active_days: 20,
        }
    }

    #[test]
    fn low_tokens_is_chick() {
        let m = Metrics {
            tokens_per_day: 100_000.0,
            ..base()
        };
        assert!(classify(&m).is_none());
    }

    #[test]
    fn short_window_active_days_is_chick() {
        // A real 30-day user viewed at --days 1 still has a window-stable rate,
        // but too few window-active days to score yet.
        let m = Metrics {
            window_active_days: 2,
            ..base()
        };
        assert!(classify(&m).is_none());
    }

    #[test]
    fn row_is_token_rate_not_style() {
        // High parallel but a trickle of tokens → entry row (R6), Control col.
        let m = Metrics {
            control: 3.0,
            tokens_per_day: 1_000_000.0, // R6
            ..base()
        };
        assert_eq!(classify(&m), Some((Style::Control, 6)));
        assert_eq!(animal_for(Style::Control, 6), "Ant");
    }

    #[test]
    fn parallel_only_lands_control() {
        // Parallel over the bar, heavy under it → Control regardless of model mix.
        let m = Metrics {
            control: 3.0,
            heavy_per_day: 0.5,
            multi_model_share: 0.4,
            tokens_per_day: 450_000_000.0, // R2
            window_active_days: 25,
        };
        assert_eq!(classify(&m), Some((Style::Control, 2)));
        assert_eq!(animal_for(Style::Control, 2), "Octopus");
    }

    #[test]
    fn heavy_only_lands_solo() {
        // Many long unattended runs per day, low concurrency → Solo.
        let m = Metrics {
            control: 1.0,
            heavy_per_day: 4.0,
            multi_model_share: 0.4,
            tokens_per_day: 450_000_000.0, // R2
            window_active_days: 25,
        };
        assert_eq!(classify(&m), Some((Style::Solo, 2)));
        assert_eq!(animal_for(Style::Solo, 2), "Wolf");
    }

    #[test]
    fn neither_axis_lands_scout() {
        // Plain / general use: not parallel, not heavy. Multi-model alone does
        // not promote out of Scout.
        let m = Metrics {
            control: 1.2,
            heavy_per_day: 0.2,
            multi_model_share: 0.4,
            tokens_per_day: 450_000_000.0, // R2
            window_active_days: 18,
        };
        assert_eq!(classify(&m), Some((Style::Scout, 2)));
        assert_eq!(animal_for(Style::Scout, 2), "Orca");
    }

    #[test]
    fn parallel_heavy_and_multi_is_all_rounder() {
        // Parallel AND heavy AND multi-model → the orchestration profile.
        let m = Metrics {
            control: 3.35,
            heavy_per_day: 4.17,
            multi_model_share: 0.10,
            tokens_per_day: 450_000_000.0, // R2
            window_active_days: 29,
        };
        assert_eq!(classify(&m), Some((Style::AllRounder, 2)));
        assert_eq!(animal_for(Style::AllRounder, 2), "Hawk");
    }

    #[test]
    fn parallel_and_heavy_but_single_model_is_not_all_rounder() {
        // Both axes clear the bar, but everything came from one provider → it
        // falls to the stronger single axis instead of AllRounder.
        let m = Metrics {
            control: 3.35,       // 3.35 / 1.8 = 1.86
            heavy_per_day: 4.17, // 4.17 / 1.8 = 2.32 (stronger)
            multi_model_share: 0.0,
            tokens_per_day: 450_000_000.0, // R2
            window_active_days: 29,
        };
        assert_eq!(classify(&m), Some((Style::Solo, 2)));
    }

    #[test]
    fn apex_all_rounder_is_lion() {
        let m = Metrics {
            control: 5.0,
            heavy_per_day: 10.0,
            multi_model_share: 0.3,
            tokens_per_day: 800_000_000.0, // R1
            window_active_days: 28,
        };
        assert_eq!(classify(&m), Some((Style::AllRounder, 1)));
        assert_eq!(animal_for(Style::AllRounder, 1), "Lion");
    }

    #[test]
    fn multi_model_just_under_threshold_is_not_all_rounder() {
        // 4% minority share is below the 5% multi bar → not AllRounder even
        // with both implementation axes cleared.
        let m = Metrics {
            control: 3.0,
            heavy_per_day: 3.0,
            multi_model_share: 0.04,
            tokens_per_day: 450_000_000.0,
            window_active_days: 25,
        };
        // Not AllRounder (multi under bar); with equal normalised parallel/heavy
        // strength the tie falls to Control.
        assert_eq!(classify(&m).map(|(style, _)| style), Some(Style::Control));
    }

    /// A combined summary that lands `AllRounder` R1 (`Lion`): parallel, heavy,
    /// multi-model, top-row volume.
    fn all_rounder_combined() -> Summary {
        let mut s = crate::share::fixtures::sample_summary();
        s.orchestration.avg_concurrency = 3.0; // parallel
        s.active_days = 30;
        if let Some(duration) = s.completion_duration.as_mut() {
            // tail (20m+) buckets → unattended 60 / 30 days = 2.0/day ≥ SOLO_MIN
            duration.buckets[3].count = 30;
            duration.buckets[4].count = 20;
            duration.buckets[5].count = 10;
        }
        s.recent_window_provider_min_share = 0.2; // multi-model
        s.recent_window_volume = 800_000_000 * CODENAME_WINDOW_DAYS as u64; // R1
        s.recent_window_active_days = 29;
        s
    }

    #[test]
    fn combined_all_rounder_is_lion() {
        assert_eq!(for_summary(&all_rounder_combined()).animal, "Lion");
    }

    #[test]
    fn parallel_heavy_tab_reaches_all_rounder_via_src_multi() {
        // A parallel + heavy single-provider tab has multi-model share 0 on its
        // own, so alone it can't be AllRounder. Borrowing the combined multi
        // signal lets it reach AllRounder at its own row → R3 = Swallow.
        let combined = all_rounder_combined();
        let mut tab = combined.clone(); // keeps parallel (3.0) + heavy (2.0/day)
        tab.provider = crate::model::Provider::Claude;
        tab.recent_window_provider_min_share = 0.0; // single provider in isolation
        tab.recent_window_volume = 200_000_000 * CODENAME_WINDOW_DAYS as u64; // R3

        assert_eq!(for_summary_styled(&tab, &combined).animal, "Swallow");
        // Scored alone it is not AllRounder (no multi-model share).
        assert_ne!(for_summary(&tab).animal, "Swallow");
    }

    #[test]
    fn non_parallel_tab_is_not_all_rounder_even_with_multi_src() {
        // The Codex-tab case: you barely parallelise that agent. Even though the
        // combined view is AllRounder, parallel/heavy are read per-tab, so a tab
        // that is neither stays Scout — it does not inherit AllRounder.
        let combined = all_rounder_combined();
        let mut tab = combined.clone();
        tab.orchestration.avg_concurrency = 1.0; // not parallel
        if let Some(duration) = tab.completion_duration.as_mut() {
            duration.buckets[3].count = 0; // and not heavy
            duration.buckets[4].count = 0;
            duration.buckets[5].count = 0;
        }
        tab.recent_window_provider_min_share = 0.0;
        tab.recent_window_volume = 200_000_000 * CODENAME_WINDOW_DAYS as u64; // R3

        assert_eq!(
            for_summary_styled(&tab, &combined).animal,
            animal_for(Style::Scout, 3),
        );
    }

    #[test]
    fn tab_below_floor_is_chick_regardless_of_src_style() {
        let combined = all_rounder_combined();
        let mut tab = combined.clone();
        tab.recent_window_volume = 100_000 * CODENAME_WINDOW_DAYS as u64; // below floor
        assert_eq!(for_summary_styled(&tab, &combined).animal, CHICK);
    }

    #[test]
    fn night_heavy_hours_pick_luna() {
        let mut hourly = [0u64; 24];
        hourly[23] = 800;
        hourly[0] = 400;
        hourly[1] = 300;
        assert_eq!(ops(&hourly), "Luna");
    }

    #[test]
    fn balanced_day_and_night_is_eclipse() {
        let mut hourly = [0u64; 24];
        hourly[13] = 500; // Sol
        hourly[20] = 480; // Luna
        assert_eq!(ops(&hourly), "Eclipse");
    }
}
