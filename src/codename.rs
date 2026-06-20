//! Codename: the MGS-styled vanity title derived from a usage `Summary`.
//!
//! Shown as `[OPS] [ANIMAL]`, e.g. "Eclipse Hawk". The ANIMAL encodes a 6×4
//! grid — **ROW = token throughput (the level), COLUMN = working style**
//! (parallel / heavy / research / all-rounder) — so one word carries both how
//! much and what kind. OPS is the dominant time-of-day. "Chick" is the no-data
//! floor.
//!
//! Everything is a RATE or RATIO, never an absolute cumulative count, so the
//! title does not drift just because the window changes. The level is tokens
//! per day over the most recent 30 days — computed over a fixed window
//! regardless of the display `--days`, so 7-, 30-, or 90-day views all pin to
//! the same level; the style axes are rates/ratios over the analysis window.
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
    /// Research — reading/exploring more than building.
    Scout,
    /// High on both parallel AND heavy at once (gated on those two style axes
    /// only; the displayed row still comes from token throughput, so a
    /// low-throughput all-rounder lands at a low row). Rare by construction.
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
// the level stays pinned even while graphs span 90 days). Calibrated so the
// heaviest real users sit at R2, leaving R1 as aspirational headroom nobody
// reaches yet. Retune here only; nothing else hard-codes a number.

/// The level always reflects the most recent N days of token throughput.
/// The analyzer fills `Summary::recent_window_volume` over this same window.
pub(crate) const CODENAME_WINDOW_DAYS: i64 = 30;

/// Row (R1 top .. R6 entry) by tokens per day over the window.
const TOKENS_PER_DAY: [f64; 6] = [
    400_000_000.0, // R1
    200_000_000.0, // R2
    80_000_000.0,  // R3
    25_000_000.0,  // R4
    6_000_000.0,   // R5
    500_000.0,     // R6
];

/// Per-style strength bands, descending R1..R6 — all rates/ratios. Used to pick
/// the dominant style and to gate the all-rounder.
const CONTROL_AVG: [f64; 6] = [4.0, 2.5, 1.8, 1.4, 1.15, 1.02]; // weighted avg simultaneous sessions (parallel)
const HEAVY_PER_DAY: [f64; 6] = [8.0, 3.5, 1.8, 0.8, 0.3, 0.05]; // 20m+ unattended runs per active day (heavy)
const SCOUT_RESEARCH: [f64; 6] = [70.0, 50.0, 35.0, 22.0, 12.0, 3.0]; // research % of (research+build) tools

/// All-rounder = parallel AND heavy both reach at least this band (research is
/// NOT required). Rare by construction — you must clear an absolute bar on both.
const ALLROUND_MIN_ROW: usize = 3;

/// Below either floor the user is "Chick" (no real data yet).
const CHICK_MIN_TOKENS_PER_DAY: f64 = 500_000.0;
const CHICK_MIN_DAYS: usize = 3;

/// OPS is decided when the top time-band leads the second by this many points;
/// otherwise the day is "mixed" → Eclipse.
const OPS_DOMINANCE_PT: f64 = 15.0;

/// 6×4 animal grid: `[row R1..R6][parallel, heavy, research, all-rounder]`. The
/// 24 codenames are exactly Metal Gear Solid: Peace Walker's 24 ranks
/// (agent-WALKER ← Peace Walker), ordered per column strongest (R1) → humblest.
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
const RESEARCH_TOOLS: [&str; 26] = [
    "Read",
    "Grep",
    "Glob",
    "WebFetch",
    "WebSearch",
    "view_image",
    // Codex shell commands decomposed by the collector (read/inspect side).
    "cat",
    "less",
    "head",
    "tail",
    "grep",
    "rg",
    "egrep",
    "fgrep",
    "ls",
    "find",
    "fd",
    "tree",
    "wc",
    "stat",
    "file",
    "jq",
    "diff",
    "sed",
    "awk",
    "cut",
];
const BUILD_TOOLS: [&str; 28] = [
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "apply_patch",
    // Codex shell wrappers, kept as the fallback bucket for commands the
    // collector could not decompose.
    "exec_command",
    "write_stdin",
    "Bash",
    // Codex shell commands decomposed by the collector (build/mutate side).
    "cargo",
    "npm",
    "npx",
    "bun",
    "pnpm",
    "yarn",
    "make",
    "python",
    "python3",
    "node",
    "go",
    "rustc",
    "mkdir",
    "rm",
    "mv",
    "cp",
    "touch",
    "chmod",
    "docker",
    "pip",
];

// ===========================================================================

struct Metrics {
    control: f64,        // weighted avg simultaneous sessions (parallel)
    heavy_per_day: f64,  // 20m+ unattended runs per active day (heavy)
    scout: f64,          // research % of (research + build) tools
    tokens_per_day: f64, // tokens/day over the most recent window (level)
    active_days: usize,  // active days (Chick floor + heavy rate)
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

    let research = research_calls(summary);
    let build = tool_calls(summary, &BUILD_TOOLS);
    let scout = if research + build > 0 {
        research as f64 / (research + build) as f64 * 100.0
    } else {
        0.0
    };

    Metrics {
        control: summary.orchestration.avg_concurrency,
        heavy_per_day,
        scout,
        tokens_per_day: tokens_per_day(summary),
        active_days,
    }
}

/// Tokens per day over the fixed codename window. `recent_window_volume` is the
/// analyzer's last-30-day token sum, computed independently of the display
/// `--days`, so the level is the same whether the user views 7, 30, or 90 days.
fn tokens_per_day(summary: &Summary) -> f64 {
    summary.recent_window_volume as f64 / CODENAME_WINDOW_DAYS as f64
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

/// `None` => Chick. Otherwise `(style column, row 1..=6)`. The row is the token
/// level; the style is the column — they are independent, so growing any axis
/// can only raise the row (via throughput) or switch the column, never demote.
fn classify(m: &Metrics) -> Option<(Style, usize)> {
    if m.tokens_per_day < CHICK_MIN_TOKENS_PER_DAY || m.active_days < CHICK_MIN_DAYS {
        return None;
    }

    let row = band_row(m.tokens_per_day, &TOKENS_PER_DAY).clamp(1, 6);

    let parallel_row = band_row(m.control, &CONTROL_AVG);
    let heavy_row = band_row(m.heavy_per_day, &HEAVY_PER_DAY);

    let style = if parallel_row <= ALLROUND_MIN_ROW && heavy_row <= ALLROUND_MIN_ROW {
        Style::AllRounder
    } else {
        // Strongest single style by R1-normalized strength. Ties favour the
        // earlier axis (parallel > heavy > research) via strict `>`.
        let strengths = [
            (Style::Control, m.control / CONTROL_AVG[0]),
            (Style::Solo, m.heavy_per_day / HEAVY_PER_DAY[0]),
            (Style::Scout, m.scout / SCOUT_RESEARCH[0]),
        ];
        strengths
            .iter()
            .copied()
            .reduce(|best, next| if next.1 > best.1 { next } else { best })
            .map_or(Style::Control, |(style, _)| style)
    };

    Some((style, row))
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
            scout: 0.0,
            tokens_per_day: 220_000_000.0, // R2
            active_days: 20,
        }
    }

    #[test]
    fn no_data_is_chick() {
        let m = Metrics {
            tokens_per_day: 100_000.0,
            active_days: 2,
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
    fn parallel_dominant_lands_octopus() {
        let m = Metrics {
            control: 3.0,
            heavy_per_day: 0.5,
            scout: 10.0,
            tokens_per_day: 250_000_000.0, // R2
            active_days: 25,
        };
        assert_eq!(classify(&m), Some((Style::Control, 2)));
        assert_eq!(animal_for(Style::Control, 2), "Octopus");
    }

    #[test]
    fn heavy_dominant_lands_wolf() {
        // Many long unattended runs per day, low concurrency.
        let m = Metrics {
            control: 1.0,
            heavy_per_day: 4.0,
            scout: 5.0,
            tokens_per_day: 250_000_000.0,
            active_days: 25,
        };
        assert_eq!(classify(&m), Some((Style::Solo, 2)));
        assert_eq!(animal_for(Style::Solo, 2), "Wolf");
    }

    #[test]
    fn research_dominant_lands_orca() {
        let m = Metrics {
            control: 1.2,
            heavy_per_day: 0.2,
            scout: 60.0,
            tokens_per_day: 250_000_000.0,
            active_days: 18,
        };
        assert_eq!(classify(&m), Some((Style::Scout, 2)));
        assert_eq!(animal_for(Style::Scout, 2), "Orca");
    }

    #[test]
    fn parallel_and_heavy_both_high_is_all_rounder() {
        // Clears the bar on parallel AND heavy → all-rounder (research
        // irrelevant). This is the heavy-orchestrator profile.
        let m = Metrics {
            control: 3.35,
            heavy_per_day: 4.17,
            scout: 19.0,
            tokens_per_day: 220_000_000.0, // R2
            active_days: 29,
        };
        assert_eq!(classify(&m), Some((Style::AllRounder, 2)));
        assert_eq!(animal_for(Style::AllRounder, 2), "Hawk");
    }

    #[test]
    fn apex_all_rounder_is_hound() {
        let m = Metrics {
            control: 5.0,
            heavy_per_day: 10.0,
            scout: 80.0,
            tokens_per_day: 500_000_000.0, // R1
            active_days: 28,
        };
        assert_eq!(classify(&m), Some((Style::AllRounder, 1)));
        assert_eq!(animal_for(Style::AllRounder, 1), "Hound");
    }

    #[test]
    fn parallel_favoured_over_heavy_on_tie() {
        // Equal normalized strength, neither reaching the all-rounder bar. The
        // values give exactly equal ratios (0.30) — re-tune if bands change.
        let m = Metrics {
            control: 1.2,       // 1.2 / 4.0 = 0.30, parallel_row R5
            heavy_per_day: 2.4, // 2.4 / 8.0 = 0.30, heavy_row R3
            scout: 0.0,
            tokens_per_day: 220_000_000.0,
            active_days: 20,
        };
        // Heavy reaches R3 but parallel only R5, so not an all-rounder; the tie
        // then resolves to the earlier axis.
        assert_eq!(classify(&m).map(|(style, _)| style), Some(Style::Control));
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
