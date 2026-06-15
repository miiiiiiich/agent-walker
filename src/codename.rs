//! Codename: the MGS-styled vanity title derived from a usage `Summary`.
//!
//! Shown as `[OPS] [ANIMAL]`, e.g. "Eclipse Ocelot". The ANIMAL encodes a 6×4
//! grid — level (R1 top → R6 entry) × style (CONTROL / SOLO / MASS / SCOUT) —
//! so a single word carries both how strong and what kind. OPS is the dominant
//! time-of-day. "Chick" is the no-data floor.
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
    Control,
    Solo,
    Mass,
    Scout,
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
// Calibrated against real 90-day dashboards: a heavy orchestrator (parallel
// sessions), a night-owl power user (huge scale + 23h runs), and a light
// multi-model user. R1 cut-offs sit deliberately ABOVE the heaviest observed
// user, so the apex row (Fox Hound / Fox / Doberman / Hound) stays aspirational
// headroom — nobody real reaches it yet, leaving a visible ceiling to climb
// toward. All values assume the default ~90-day window. Retune here only;
// nothing else hard-codes a number.

/// Per-style band cut-offs, descending R1..R6: a raw metric reaches the first
/// row whose threshold it clears. R1 is intentionally unreached by current data.
const CONTROL_PCT: [f64; 6] = [85.0, 55.0, 35.0, 18.0, 7.0, 1.0]; // parallel-rate %
const SOLO_RUNS: [f64; 6] = [250.0, 60.0, 25.0, 8.0, 2.0, 1.0]; // unattended runs >=20m
const MASS_TPD: [f64; 6] = [
    250_000_000.0,
    50_000_000.0,
    15_000_000.0,
    5_000_000.0,
    1_000_000.0,
    100_000.0,
]; // tokens per active day
const SCOUT_BREADTH: [f64; 6] = [18.0, 9.0, 6.0, 4.0, 2.0, 1.0]; // models x projects + read bonus

/// Substance cap (total tokens, active days) needed to *hold* each row,
/// regardless of style — stops a single-axis spike claiming a high rank.
const SUBSTANCE: [(u64, usize); 6] = [
    (2_000_000_000, 30), // R1
    (700_000_000, 18),   // R2
    (400_000_000, 10),   // R3
    (30_000_000, 6),     // R4
    (8_000_000, 3),      // R5
    (5_000_000, 3),      // R6
];

/// Below either floor the user is "Chick" (no real data yet).
const CHICK_MIN_TOKENS: u64 = 5_000_000;
const CHICK_MIN_DAYS: usize = 3;

/// R1 is compound / near-legendary: the winning style at R1 *and* every other
/// style at R2 or better *and* at least this many active days. A true
/// all-rounder elite on all four axes, not a single-axis monster.
const R1_OTHER_AXES_MAX_ROW: usize = 2;
const R1_MIN_ACTIVE_DAYS: usize = 30;

/// Breadth bonus when the toolset skews review/plan-heavy (reads > writes).
const SCOUT_READ_BONUS: u64 = 3;

/// OPS is decided when the top time-band leads the second by this many points;
/// otherwise the day is "mixed" → Eclipse.
const OPS_DOMINANCE_PT: f64 = 15.0;

/// 6×4 animal grid: `[row R1..R6][Control, Solo, Mass, Scout]`.
const GRID: [[&str; 4]; 6] = [
    ["Fox Hound", "Fox", "Doberman", "Hound"], // R1
    ["Ocelot", "Wolf", "Raven", "Mantis"],     // R2
    ["Jaguar", "Cobra", "Orca", "Octopus"],    // R3
    ["Panther", "Hawk", "Shark", "Eagle"],     // R4
    ["Leopard", "Mongoose", "Whale", "Owl"],   // R5
    ["Puma", "Hyena", "Pig", "Bat"],           // R6
];

const READ_TOOLS: [&str; 6] = ["Read", "Grep", "Glob", "view_image", "get_app_state", "WebFetch"];
const WRITE_TOOLS: [&str; 6] = ["Edit", "Write", "apply_patch", "exec_command", "write_stdin", "Bash"];

// ===========================================================================

struct Metrics {
    control_pct: f64,
    solo_runs: f64,
    mass_tpd: f64,
    scout_breadth: f64,
    total_tokens: u64,
    active_days: usize,
}

/// Public entry: derive the codename for a summary. Computed on demand at
/// display time, never stored, so the analyzer stays free of vanity logic.
pub fn for_summary(summary: &Summary) -> Codename {
    let metrics = metrics(summary);
    match classify(&metrics) {
        None => Codename {
            ops: "Eclipse",
            animal: CHICK,
        },
        Some((style, row)) => Codename {
            ops: ops(&summary.hourly_usage),
            animal: animal_for(style, row),
        },
    }
}

fn metrics(summary: &Summary) -> Metrics {
    let total = summary.total_usage.token_volume();
    let active = summary.active_days;

    let solo_runs = summary.completion_duration.as_ref().map_or(0, |duration| {
        duration
            .buckets
            .iter()
            .filter(|bucket| matches!(bucket.label.as_str(), "20-30m" | "30-60m" | "1h+"))
            .map(|bucket| bucket.count)
            .sum::<usize>()
    });

    let mass_tpd = if active > 0 {
        total as f64 / active as f64
    } else {
        0.0
    };

    let model_breadth = share_count(total, summary.models.iter().map(|m| m.usage.token_volume()));
    let project_breadth =
        share_count(total, summary.projects.iter().map(|p| p.usage.token_volume()));
    let reads = tool_calls(summary, &READ_TOOLS);
    let writes = tool_calls(summary, &WRITE_TOOLS);
    let bonus = if reads > writes { SCOUT_READ_BONUS } else { 0 };
    let scout_breadth = model_breadth.saturating_mul(project_breadth).saturating_add(bonus);

    Metrics {
        control_pct: summary.orchestration.parallel_rate * 100.0,
        solo_runs: solo_runs as f64,
        mass_tpd,
        scout_breadth: scout_breadth as f64,
        total_tokens: total,
        active_days: active,
    }
}

/// Count contributors holding at least a 10% token share of the total.
fn share_count(total: u64, parts: impl Iterator<Item = u64>) -> u64 {
    if total == 0 {
        return 0;
    }
    parts
        .filter(|value| *value as f64 / total as f64 >= 0.10)
        .count() as u64
}

fn tool_calls(summary: &Summary, names: &[&str]) -> usize {
    summary
        .tools
        .iter()
        .filter(|tool| names.contains(&tool.name.as_str()))
        .map(|tool| tool.calls)
        .sum()
}

/// `None` => Chick. Otherwise `(style column, row 1..=6)`.
fn classify(m: &Metrics) -> Option<(Style, usize)> {
    if m.total_tokens < CHICK_MIN_TOKENS || m.active_days < CHICK_MIN_DAYS {
        return None;
    }

    let rows = [
        (Style::Control, band_row(m.control_pct, &CONTROL_PCT)),
        (Style::Solo, band_row(m.solo_runs, &SOLO_RUNS)),
        (Style::Mass, band_row(m.mass_tpd, &MASS_TPD)),
        (Style::Scout, band_row(m.scout_breadth, &SCOUT_BREADTH)),
    ];

    // Column = strongest style by raw/R1-cutoff. Ties favour the earlier axis
    // (CONTROL > SOLO > MASS > SCOUT) via strict `>`.
    let strengths = [
        (Style::Control, m.control_pct / CONTROL_PCT[0]),
        (Style::Solo, m.solo_runs / SOLO_RUNS[0]),
        (Style::Mass, m.mass_tpd / MASS_TPD[0]),
        (Style::Scout, m.scout_breadth / SCOUT_BREADTH[0]),
    ];
    let winner = strengths
        .iter()
        .copied()
        .reduce(|best, next| if next.1 > best.1 { next } else { best })
        .map_or(Style::Control, |(style, _)| style);

    let style_row = rows
        .iter()
        .find(|(style, _)| *style == winner)
        .map_or(6, |(_, row)| *row);
    let substance = substance_row(m.total_tokens, m.active_days);

    // The worse (numerically larger) of skill and substance binds; clamp to grid.
    let mut row = style_row.max(substance).clamp(1, 6);

    // R1 compound demotion: a one-axis monster stays at R2.
    if row == 1 {
        let well_rounded = rows
            .iter()
            .filter(|(style, _)| *style != winner)
            .all(|(_, other)| *other <= R1_OTHER_AXES_MAX_ROW);
        if !(well_rounded && m.active_days >= R1_MIN_ACTIVE_DAYS) {
            row = 2;
        }
    }

    Some((winner, row))
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

fn substance_row(total: u64, active_days: usize) -> usize {
    for (index, (min_tokens, min_days)) in SUBSTANCE.iter().enumerate() {
        if total >= *min_tokens && active_days >= *min_days {
            return index + 1;
        }
    }
    7
}

fn animal_for(style: Style, row: usize) -> &'static str {
    let column = match style {
        Style::Control => 0,
        Style::Solo => 1,
        Style::Mass => 2,
        Style::Scout => 3,
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
            control_pct: 0.0,
            solo_runs: 0.0,
            mass_tpd: 0.0,
            scout_breadth: 0.0,
            total_tokens: 6_000_000,
            active_days: 30,
        }
    }

    #[test]
    fn no_data_is_chick() {
        let m = Metrics {
            total_tokens: 1_000_000,
            active_days: 2,
            ..base()
        };
        assert!(classify(&m).is_none());
    }

    #[test]
    fn heavy_orchestrator_lands_control_r2() {
        // michisato: 86% parallel, low autonomy, 6.4B over 60 days.
        let m = Metrics {
            control_pct: 86.0,
            solo_runs: 10.0,
            mass_tpd: 6_400_000_000.0 / 60.0,
            scout_breadth: 2.0,
            total_tokens: 6_400_000_000,
            active_days: 60,
        };
        assert_eq!(classify(&m), Some((Style::Control, 2)));
        assert_eq!(animal_for(Style::Control, 2), "Ocelot");
    }

    #[test]
    fn night_owl_power_user_lands_solo_r2() {
        // suzuki: low parallel, 198 unattended runs, 9.4B over 52 days.
        let m = Metrics {
            control_pct: 2.0,
            solo_runs: 198.0,
            mass_tpd: 9_400_000_000.0 / 52.0,
            scout_breadth: 2.0,
            total_tokens: 9_400_000_000,
            active_days: 52,
        };
        assert_eq!(classify(&m), Some((Style::Solo, 2)));
        assert_eq!(animal_for(Style::Solo, 2), "Wolf");
    }

    #[test]
    fn light_multi_model_user_is_capped_by_substance() {
        // 部下: multi-model breadth but only 320M over 28 days.
        let m = Metrics {
            control_pct: 0.0,
            solo_runs: 0.0,
            mass_tpd: 320_000_000.0 / 28.0,
            scout_breadth: 9.0,
            total_tokens: 320_000_000,
            active_days: 28,
        };
        assert_eq!(classify(&m), Some((Style::Scout, 4)));
        assert_eq!(animal_for(Style::Scout, 4), "Eagle");
    }

    #[test]
    fn r1_demotes_to_r2_when_one_other_axis_is_only_r3() {
        // R1 control (>=85%), R2 solo/mass, but SCOUT only R3 (breadth 6): the
        // compound gate requires R2+ on every other axis, so this lands Ocelot.
        let m = Metrics {
            control_pct: 88.0,
            solo_runs: 136.0,
            mass_tpd: 119_000_000.0,
            scout_breadth: 6.0,
            total_tokens: 8_000_000_000,
            active_days: 67,
        };
        assert_eq!(classify(&m), Some((Style::Control, 2)));
        assert_eq!(animal_for(Style::Control, 2), "Ocelot");
    }

    #[test]
    fn current_ceiling_user_lands_ocelot_leaving_r1_aspirational() {
        // Real combined michisato: control 75.2%, 136 unattended runs, 119M
        // tpd, breadth 9, 8B over 67 days — elite on all four axes (R2+), yet
        // below the aspirational R1 cut-offs, so he sits at Ocelot (2nd) and
        // Fox Hound (R1) stays an empty, climbable ceiling.
        let m = Metrics {
            control_pct: 75.2,
            solo_runs: 136.0,
            mass_tpd: 119_000_000.0,
            scout_breadth: 9.0,
            total_tokens: 8_000_000_000,
            active_days: 67,
        };
        assert_eq!(classify(&m), Some((Style::Control, 2)));
        assert_eq!(animal_for(Style::Control, 2), "Ocelot");
    }

    #[test]
    fn fox_hound_requires_r2_or_better_on_every_axis() {
        // A genuine all-rounder: R1 control, R2+ on the other three, 30+ days.
        let m = Metrics {
            control_pct: 90.0,
            solo_runs: 70.0,
            mass_tpd: 60_000_000.0,
            scout_breadth: 10.0,
            total_tokens: 3_000_000_000,
            active_days: 40,
        };
        assert_eq!(classify(&m), Some((Style::Control, 1)));
        assert_eq!(animal_for(Style::Control, 1), "Fox Hound");
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
