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
const CONTROL_AVG: [f64; 6] = [4.0, 2.5, 1.8, 1.4, 1.15, 1.02]; // weighted avg simultaneous sessions
const SOLO_RUNS: [f64; 6] = [250.0, 60.0, 25.0, 8.0, 2.0, 1.0]; // unattended runs >=20m
const MASS_TPD: [f64; 6] = [
    250_000_000.0,
    50_000_000.0,
    15_000_000.0,
    5_000_000.0,
    1_000_000.0,
    100_000.0,
]; // tokens per active day
const SCOUT_RESEARCH: [f64; 6] = [70.0, 50.0, 35.0, 22.0, 12.0, 3.0]; // research % of (research+build) tools

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

/// OPS is decided when the top time-band leads the second by this many points;
/// otherwise the day is "mixed" → Eclipse.
const OPS_DOMINANCE_PT: f64 = 15.0;

/// 6×4 animal grid: `[row R1..R6][Control, Solo, Mass, Scout]`. The 24 codenames
/// are exactly Metal Gear Solid: Peace Walker's 24 ranks (agent-WALKER ← Peace
/// Walker), ordered per column strongest (R1) → humblest (R6).
const GRID: [[&str; 4]; 6] = [
    ["Foxhound", "Fox", "Doberman", "Hound"], // R1
    ["Octopus", "Wolf", "Orca", "Hawk"],      // R2
    ["Raven", "Eel", "Whale", "Swallow"],     // R3
    ["Scorpion", "Piranha", "Bear", "Gull"],  // R4
    ["Cat", "Kangaroo", "Puma", "Deer"],      // R5
    ["Ant", "Firefly", "Butterfly", "Bee"],   // R6
];

/// SCOUT = "explore / research" share of (research + build) tool calls. High =
/// investigating & reading, low = constructing. Any `mcp__*` tool also counts as
/// research (matched by prefix in `research_calls`).
const RESEARCH_TOOLS: [&str; 6] = ["Read", "Grep", "Glob", "WebFetch", "WebSearch", "view_image"];
const BUILD_TOOLS: [&str; 8] = [
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "apply_patch",
    "exec_command",
    "write_stdin",
    "Bash",
];

// ===========================================================================

struct Metrics {
    control: f64, // weighted avg simultaneous sessions
    solo_runs: f64,
    mass_tpd: f64,
    scout: f64, // research % of (research + build) tools
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

    let research = research_calls(summary);
    let build = tool_calls(summary, &BUILD_TOOLS);
    let scout = if research + build > 0 {
        research as f64 / (research + build) as f64 * 100.0
    } else {
        0.0
    };

    Metrics {
        control: summary.orchestration.avg_concurrency,
        solo_runs: solo_runs as f64,
        mass_tpd,
        scout,
        total_tokens: total,
        active_days: active,
    }
}

/// Research/explore tool calls: named research tools plus any `mcp__*` tool.
fn research_calls(summary: &Summary) -> usize {
    summary
        .tools
        .iter()
        .filter(|tool| {
            RESEARCH_TOOLS.contains(&tool.name.as_str()) || tool.name.starts_with("mcp__")
        })
        .map(|tool| tool.calls)
        .sum()
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
        (Style::Control, band_row(m.control, &CONTROL_AVG)),
        (Style::Solo, band_row(m.solo_runs, &SOLO_RUNS)),
        (Style::Mass, band_row(m.mass_tpd, &MASS_TPD)),
        (Style::Scout, band_row(m.scout, &SCOUT_RESEARCH)),
    ];

    // Column = strongest style by raw/R1-cutoff. Ties favour the earlier axis
    // (CONTROL > SOLO > MASS > SCOUT) via strict `>`.
    let strengths = [
        (Style::Control, m.control / CONTROL_AVG[0]),
        (Style::Solo, m.solo_runs / SOLO_RUNS[0]),
        (Style::Mass, m.mass_tpd / MASS_TPD[0]),
        (Style::Scout, m.scout / SCOUT_RESEARCH[0]),
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
            control: 1.0,
            solo_runs: 0.0,
            mass_tpd: 0.0,
            scout: 0.0,
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
    fn control_dominant_lands_octopus() {
        // Sustained parallelism (avg 3 concurrent), weak elsewhere.
        let m = Metrics {
            control: 3.0,
            solo_runs: 5.0,
            mass_tpd: 10_000_000.0,
            scout: 10.0,
            total_tokens: 1_000_000_000,
            active_days: 30,
        };
        assert_eq!(classify(&m), Some((Style::Control, 2)));
        assert_eq!(animal_for(Style::Control, 2), "Octopus");
    }

    #[test]
    fn night_owl_power_user_lands_solo_wolf() {
        // Mostly solo (avg ~1.1) but 198 unattended runs over 9.4B/52d.
        let m = Metrics {
            control: 1.1,
            solo_runs: 198.0,
            mass_tpd: 9_400_000_000.0 / 52.0,
            scout: 5.0,
            total_tokens: 9_400_000_000,
            active_days: 52,
        };
        assert_eq!(classify(&m), Some((Style::Solo, 2)));
        assert_eq!(animal_for(Style::Solo, 2), "Wolf");
    }

    #[test]
    fn research_dominant_lands_hawk() {
        // Reads/explores far more than it builds — single repo is fine.
        let m = Metrics {
            control: 1.2,
            solo_runs: 4.0,
            mass_tpd: 10_000_000.0,
            scout: 60.0,
            total_tokens: 1_000_000_000,
            active_days: 20,
        };
        assert_eq!(classify(&m), Some((Style::Scout, 2)));
        assert_eq!(animal_for(Style::Scout, 2), "Hawk");
    }

    #[test]
    fn light_user_is_capped_by_substance() {
        // Research-leaning but only 320M over 28 days → substance caps at R4.
        let m = Metrics {
            control: 1.0,
            solo_runs: 0.0,
            mass_tpd: 320_000_000.0 / 28.0,
            scout: 50.0,
            total_tokens: 320_000_000,
            active_days: 28,
        };
        assert_eq!(classify(&m), Some((Style::Scout, 4)));
        assert_eq!(animal_for(Style::Scout, 4), "Gull");
    }

    #[test]
    fn r1_demotes_to_r2_when_one_other_axis_is_only_r3() {
        // R1 control (avg 6), R2 solo/mass, but SCOUT only R3 (research 36%):
        // the compound gate needs R2+ on every other axis → lands Ocelot.
        let m = Metrics {
            control: 6.0,
            solo_runs: 70.0,
            mass_tpd: 60_000_000.0,
            scout: 36.0,
            total_tokens: 8_000_000_000,
            active_days: 67,
        };
        assert_eq!(classify(&m), Some((Style::Control, 2)));
        assert_eq!(animal_for(Style::Control, 2), "Octopus");
    }

    #[test]
    fn fox_hound_requires_r2_or_better_on_every_axis() {
        // A genuine all-rounder: R1 control, R2+ on the other three, 30+ days.
        let m = Metrics {
            control: 6.0,
            solo_runs: 70.0,
            mass_tpd: 60_000_000.0,
            scout: 55.0,
            total_tokens: 3_000_000_000,
            active_days: 40,
        };
        assert_eq!(classify(&m), Some((Style::Control, 1)));
        assert_eq!(animal_for(Style::Control, 1), "Foxhound");
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
