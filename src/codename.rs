//! Codename: a playful vanity title derived from a usage `Summary`.
//!
//! Shown as `[OPS] [ANIMAL]`, e.g. "Eclipse Hawk". The ANIMAL encodes a 6×4
//! grid — **ROW = token throughput (how much), COLUMN = AI-mastery tier (how
//! well)**. The four ascending tiers are `Scout` → `Control` → `Solo` →
//! `AllRounder`, so one word carries both how much and how skilfully. OPS is the dominant
//! time-of-day. "Chick" is the no-data floor.
//!
//! ROW and the mastery score are independent: ROW is volume (tokens/day over the
//! most recent 30 days, fixed regardless of the display `--days`). The mastery
//! score is a blend of *ratios* (share of time at 2+ concurrent, share of
//! completions that ran long, provider balance), not magnitudes — magnitudes
//! rise with volume and would collapse the top rows into one tier, whereas ratios
//! measure *how* you work independently of *how much*. One ordered score (rather
//! than a 2×2 of binary axes) keeps people spread across tiers instead of piling
//! at the "casual" and "does-everything" corners. The score is also whole-person:
//! measured across every agent combined, so running several agents at once counts
//! as parallel.
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

use crate::model::{DurationSummary, Orchestration, Summary};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    // The four variants are ascending AI-mastery tiers, not independent
    // archetypes: Scout < Control < Solo < AllRounder. They name the cell column;
    // the row stays token volume.
    /// Mastery tier 2 of 4.
    Control,
    /// Mastery tier 3 of 4.
    Solo,
    /// Mastery tier 1 of 4 (lowest) — a plain / general way of working.
    Scout,
    /// Mastery tier 4 of 4 (highest) — drives agents most skilfully. The row
    /// still comes from token throughput, so a low-throughput master lands at a
    /// low row.
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
//
// Crucially the STYLE axes are *ratios / intensities*, not magnitudes. Magnitude
// axes (avg concurrency, long-runs-per-day) rise mechanically with volume — to
// reach a high row you must orchestrate, so a count-based axis would mark every
// heavy user parallel+heavy and collapse the top rows into one style. Ratios
// (share of time at 2+ concurrent, share of completions that ran long) measure
// *how* you work regardless of *how much*, so the column stays discriminative at
// every row.

/// ROW (R1 top .. R6 entry) by tokens per day over the window. Volume only.
const TOKENS_PER_DAY: [f64; 6] = [
    750_000_000.0, // R1
    400_000_000.0, // R2
    150_000_000.0, // R3
    45_000_000.0,  // R4
    12_000_000.0,  // R5
    500_000.0,     // R6
];

// STYLE is one ordered "AI-mastery" score in 0..1 — a blend of volume-independent
// ratios for *how skilfully* the agents are driven — split into 4 ascending tiers
// (Scout < Control < Solo < AllRounder). One ordered score (not a 2×2 of binary
// axes) is what keeps the distribution from collapsing into just "casual" and
// "does-everything": people spread along the blend instead of piling at two
// corners. Each component is scaled so its "fully maxed" value clamps to 1.0.

/// 60%+ of active time at 2+ concurrent sessions reads as fully parallel.
const PARALLEL_FULL: f64 = 0.60;
/// 30%+ of completions running 20m+ reads as fully autonomy-led.
const AUTONOMY_FULL: f64 = 0.30;
/// 25%+ minority provider share (≈ an even Claude/Codex split) reads as fully
/// multi-model. The share itself maxes at 0.5 (a perfect split).
const MULTI_FULL: f64 = 0.25;
/// Tier cut-offs on the 0..1 mastery score: below `[0]` = `Scout`, then
/// `Control`, `Solo`, and `[2]`+ = `AllRounder`.
const MASTERY_TIERS: [f64; 3] = [0.20, 0.45, 0.70];

/// Low-sample guards: a ratio is only trusted with enough underneath it, so a
/// thin sample (one long run, a few minutes of activity) can't read as a
/// full-blown style. Below these the axis reads as 0.
const PARALLEL_MIN_ACTIVE_SECS: u64 = 2 * 60 * 60; // 2h of measured active time
const AUTONOMY_MIN_COMPLETIONS: usize = 20; // total completions to trust the ratio
const AUTONOMY_MIN_LONG: usize = 3; // and at least this many long ones

/// Below either floor the user is "Chick" (no real data yet).
const CHICK_MIN_TOKENS_PER_DAY: f64 = 500_000.0;
const CHICK_MIN_DAYS: usize = 3;

/// OPS is decided when the top time-band leads the second by this many points;
/// otherwise the day is "mixed" → Eclipse.
const OPS_DOMINANCE_PT: f64 = 15.0;

/// 6×4 animal grid: `[row R1..R6][Control, Solo, Scout, AllRounder]`. The row is
/// volume; the column is the mastery tier. The array column order is historical
/// (`Control, Solo, Scout, AllRounder`); `animal_for` maps each ascending tier
/// to its column, so the *ascending* mastery order is Scout → Control → Solo →
/// AllRounder (the apex, e.g. Lion at R1). Animal placements are unchanged.
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
    parallel_share: f64,       // share of active time at 2+ concurrent (CONTROL)
    autonomy_ratio: f64,       // share of completions that ran 20m+ (SOLO)
    multi_model_share: f64,    // smaller provider's share of Claude+Codex volume
    tokens_per_day: f64,       // tokens/day over the most recent window (level)
    window_active_days: usize, // active days over the fixed 30-day window (Chick floor)
}

/// Public entry: derive the codename for a summary. Computed on demand at
/// display time, never stored, so the analyzer stays free of vanity logic.
pub fn for_summary(summary: &Summary) -> Codename {
    for_summary_styled(summary, summary)
}

/// Like [`for_summary`], but the working-style word is taken from `style_src`
/// while the row, the OPS prefix, and the Chick floor come from `summary`.
///
/// The UI passes the combined summary as `style_src`. STYLE is a whole-person
/// trait: parallel, autonomy, and multi-model are all measured across *every*
/// agent at once (Claude + Codex + Antigravity + whatever's added later), since
/// running several agents in parallel is itself orchestration and shouldn't be
/// invisible just because no single tool was driven in parallel. So a provider
/// tab shows your overall working style at that tab's own volume row — the animal
/// changes by tier, the style word stays your identity. When `summary` is the
/// combined one this is identical to [`for_summary`].
pub fn for_summary_styled(summary: &Summary, style_src: &Summary) -> Codename {
    let m = metrics(summary);
    if is_chick(&m) {
        return Codename {
            ops: "Eclipse",
            animal: CHICK,
        };
    }
    let row = band_row(m.tokens_per_day, &TOKENS_PER_DAY).clamp(1, 6);
    let style = style_of(&metrics(style_src));
    Codename {
        ops: ops(&summary.hourly_usage),
        animal: animal_for(style, row),
    }
}

fn metrics(summary: &Summary) -> Metrics {
    Metrics {
        parallel_share: parallel_share(&summary.orchestration),
        autonomy_ratio: autonomy_ratio(summary.completion_duration.as_ref()),
        multi_model_share: summary.recent_window_provider_min_share,
        tokens_per_day: tokens_per_day(summary),
        window_active_days: summary.recent_window_active_days,
    }
}

/// Share of active wall-time spent at 2+ concurrent sessions — the parallel
/// (CONTROL) axis. Volume-normalised: a hand-driver sits at concurrency 1, a
/// fan-out operator spends most of their time at 2+, regardless of total tokens.
/// Guarded: under a couple of hours of measured activity the ratio is too noisy
/// to trust, so it reads as 0 (not parallel). (The `u64 as f64` casts are covered
/// by the module-level `cast_precision_loss` allow; seconds stay far below 2^53.)
fn parallel_share(orch: &Orchestration) -> f64 {
    let active = orch
        .time_by_level
        .iter()
        .copied()
        .fold(0u64, u64::saturating_add);
    if active < PARALLEL_MIN_ACTIVE_SECS {
        return 0.0;
    }
    // Bucket 0 is concurrency-1 (solo); everything after it is 2+ concurrent.
    let parallel = orch
        .time_by_level
        .iter()
        .skip(1)
        .copied()
        .fold(0u64, u64::saturating_add);
    parallel as f64 / active as f64
}

/// Share of task completions that ran 20m+ unattended — the autonomy (SOLO)
/// axis. A ratio, not a per-day count, so heavy interactive use doesn't read as
/// autonomous just because the absolute number of long runs grows with volume.
/// Guarded: needs a minimum number of completions (and a few long ones) before
/// the ratio is trusted, so a single long run on a thin sample can't read as
/// fully autonomous. (Casts covered by the module-level `cast_precision_loss`
/// allow; completion counts stay far below 2^53.)
fn autonomy_ratio(duration: Option<&DurationSummary>) -> f64 {
    let Some(duration) = duration else {
        return 0.0;
    };
    // The histogram is three sub-20m buckets then three 20m+ buckets, so skipping
    // the first three isolates the long tail without coupling to label strings.
    let long: usize = duration
        .buckets
        .iter()
        .skip(3)
        .map(|bucket| bucket.count)
        .sum();
    if duration.count < AUTONOMY_MIN_COMPLETIONS || long < AUTONOMY_MIN_LONG {
        return 0.0;
    }
    long as f64 / duration.count as f64
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

/// The AI-mastery score in `0..1`: how skilfully the agents are driven, blended
/// from volume-independent ratios so it reads "how well" not "how much". v1 is an
/// even average of three components, each scaled so its `*_FULL` value maxes at
/// 1.0: parallelism, autonomy, and model balance. (Harness leverage — skill /
/// MCP / subagent density — is a candidate fourth component but needs a
/// non-brittle tool classification first, so it's deliberately out of v1.)
fn mastery(m: &Metrics) -> f64 {
    let norm = |value: f64, full: f64| (value / full).clamp(0.0, 1.0);
    let parallel = norm(m.parallel_share, PARALLEL_FULL);
    let autonomy = norm(m.autonomy_ratio, AUTONOMY_FULL);
    let multi = norm(m.multi_model_share, MULTI_FULL);
    (parallel + autonomy + multi) / 3.0
}

/// The mastery score banded into the 4 ascending style tiers. The column word is
/// a tier label, not a working-style archetype: `Scout` (lowest) → `Control` →
/// `Solo` → `AllRounder` (highest). The displayed row still comes from token
/// throughput, so a low-throughput master lands at a low row.
fn style_of(m: &Metrics) -> Style {
    let score = mastery(m);
    if score >= MASTERY_TIERS[2] {
        Style::AllRounder
    } else if score >= MASTERY_TIERS[1] {
        Style::Solo
    } else if score >= MASTERY_TIERS[0] {
        Style::Control
    } else {
        Style::Scout
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
            parallel_share: 0.0,
            autonomy_ratio: 0.0,
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
            parallel_share: 0.8,
            tokens_per_day: 1_000_000.0, // R6
            ..base()
        };
        assert_eq!(classify(&m), Some((Style::Control, 6)));
        assert_eq!(animal_for(Style::Control, 6), "Ant");
    }

    #[test]
    fn low_mastery_lands_control_tier() {
        // One component moderately high (parallel 0.5 → 0.83 of full), the rest
        // zero → mastery ≈ 0.28, the Control tier (tier 2 of 4).
        let m = Metrics {
            parallel_share: 0.5,
            autonomy_ratio: 0.0,
            multi_model_share: 0.0,
            tokens_per_day: 450_000_000.0, // R2
            window_active_days: 25,
        };
        assert_eq!(classify(&m), Some((Style::Control, 2)));
        assert_eq!(animal_for(Style::Control, 2), "Octopus");
    }

    #[test]
    fn two_components_high_lands_solo_tier() {
        // Parallel + autonomy both maxed, single-model → mastery ≈ 0.67, the Solo
        // tier (tier 3) — just short of the top.
        let m = Metrics {
            parallel_share: 0.6,
            autonomy_ratio: 0.3,
            multi_model_share: 0.0,
            tokens_per_day: 450_000_000.0, // R2
            window_active_days: 25,
        };
        assert_eq!(classify(&m), Some((Style::Solo, 2)));
        assert_eq!(animal_for(Style::Solo, 2), "Wolf");
    }

    #[test]
    fn nearly_nothing_lands_scout_tier() {
        // A trickle on every component → mastery below the first cut-off → Scout
        // (lowest tier). A single high component does not by itself escape Scout.
        let m = Metrics {
            parallel_share: 0.1,
            autonomy_ratio: 0.02,
            multi_model_share: 0.05,
            tokens_per_day: 450_000_000.0, // R2
            window_active_days: 18,
        };
        assert_eq!(classify(&m), Some((Style::Scout, 2)));
        assert_eq!(animal_for(Style::Scout, 2), "Orca");
    }

    #[test]
    fn all_components_high_is_all_rounder() {
        // Every component at/above its full mark → mastery ≈ 1.0 → AllRounder.
        let m = Metrics {
            parallel_share: 0.6,
            autonomy_ratio: 0.3,
            multi_model_share: 0.25,
            tokens_per_day: 450_000_000.0, // R2
            window_active_days: 29,
        };
        assert_eq!(classify(&m), Some((Style::AllRounder, 2)));
        assert_eq!(animal_for(Style::AllRounder, 2), "Hawk");
    }

    #[test]
    fn single_model_caps_below_all_rounder() {
        // Parallel + autonomy maxed but one provider only (multi 0) → mastery
        // 0.67, capped at the Solo tier. Model balance is needed for the apex.
        let m = Metrics {
            parallel_share: 0.9,
            autonomy_ratio: 0.5,
            multi_model_share: 0.0,
            tokens_per_day: 450_000_000.0, // R2
            window_active_days: 29,
        };
        assert_eq!(classify(&m).map(|(style, _)| style), Some(Style::Solo));
    }

    #[test]
    fn apex_all_rounder_is_lion() {
        let m = Metrics {
            parallel_share: 0.8,
            autonomy_ratio: 0.5,
            multi_model_share: 0.3,
            tokens_per_day: 800_000_000.0, // R1
            window_active_days: 28,
        };
        assert_eq!(classify(&m), Some((Style::AllRounder, 1)));
        assert_eq!(animal_for(Style::AllRounder, 1), "Lion");
    }

    #[test]
    fn mastery_is_monotonic_in_each_component() {
        // Raising any one component never lowers the score — sanity on the blend.
        let lo = mastery(&base());
        let up_parallel = mastery(&Metrics {
            parallel_share: 0.6,
            ..base()
        });
        let up_autonomy = mastery(&Metrics {
            autonomy_ratio: 0.3,
            ..base()
        });
        let up_multi = mastery(&Metrics {
            multi_model_share: 0.25,
            ..base()
        });
        assert!(up_parallel > lo && up_autonomy > lo && up_multi > lo);
        // Each maxed component contributes a third of the score.
        assert!((up_parallel - 1.0 / 3.0).abs() < 1e-9);
    }

    /// A combined summary that lands `AllRounder` R1 (`Lion`). `sample_summary`
    /// already carries a parallel (≈0.61 of time at 2+) and autonomous (≈0.15 of
    /// completions 20m+) profile; add the multi-model balance and top-row volume.
    fn all_rounder_combined() -> Summary {
        let mut s = crate::share::fixtures::sample_summary();
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
    fn tab_shows_combined_style_at_own_row() {
        // STYLE is whole-person (from the combined summary); only the row is
        // per-tab. A lower-volume tab of an AllRounder shows the AllRounder column
        // at its own row → R3 = Swallow.
        let combined = all_rounder_combined();
        let mut tab = combined.clone();
        tab.provider = crate::model::Provider::Claude;
        tab.recent_window_volume = 200_000_000 * CODENAME_WINDOW_DAYS as u64; // R3

        assert_eq!(for_summary_styled(&tab, &combined).animal, "Swallow");
    }

    #[test]
    fn non_parallel_tab_still_inherits_combined_style() {
        // Even a tab you never parallelise inherits the combined style word —
        // parallelism is measured across all agents at once, so the per-agent
        // breakdown doesn't override your whole-person identity. R3 AllRounder =
        // Swallow, regardless of this tab's own (flat) concurrency.
        let combined = all_rounder_combined();
        let mut tab = combined.clone();
        tab.orchestration.time_by_level = [10_000, 0, 0, 0, 0, 0]; // 0% at 2+ alone
        if let Some(duration) = tab.completion_duration.as_mut() {
            duration.buckets[3].count = 0; // and no long runs alone
            duration.buckets[4].count = 0;
            duration.buckets[5].count = 0;
        }
        tab.recent_window_volume = 200_000_000 * CODENAME_WINDOW_DAYS as u64; // R3

        assert_eq!(for_summary_styled(&tab, &combined).animal, "Swallow");
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
