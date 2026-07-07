//! Codename: a playful vanity title derived from a usage `Summary`.
//!
//! Shown as `[OPS] [ANIMAL]`, e.g. "Eclipse Doberman". The ANIMAL encodes a
//! trapezoid grid — **ROW = monthly token volume (how much), COLUMN = an
//! ordered orchestration tier (how well you drive your agents)**. The four
//! ascending columns are `Scout` (特徴なし) → `Tools` (機能活用) → `Parallel`
//! (並列) → `Apex` (頂点). OPS is the dominant time-of-day; "Ant" is the floor
//! (R8 and anything below it).
//!
//! The grid is an inverted pyramid: high orchestration is only reachable at high
//! volume (you can't run many agents in parallel on a trickle of tokens), so the
//! right-hand columns close off as the row drops — the apex (`Lion`) sits at the
//! single top-right cell. A tier above the row's reach is clamped back to the
//! row's widest column rather than left as a dead cell.
//!
//! The two orchestration signals are volume-independent so the column reads "how"
//! not "how much":
//! - **parallel** — share of active time spent at 2+ concurrent sessions,
//!   measured across *every* agent at once (Claude + Codex + Antigravity), so
//!   running several agents together counts even if no single tool was driven in
//!   parallel.
//! - **機能活用 (tooling)** — share of tool calls that delegate or invoke a
//!   capability beyond raw file/shell ops: subagents, skills, and MCP.
//!
//! Columns: both high → `Apex`; both low → `Scout`; otherwise the dominant axis
//! (`Parallel` if parallel ≥ tooling, else `Tools`). The exact cut-offs live in
//! the one block below, are easy to retune, and are never surfaced in the UI —
//! only the final title is — so the formula stays opaque even though the source
//! is public.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Scores are display-only thresholds; approximate float math never feeds back into integer state."
)]

use crate::model::{Orchestration, Summary, ToolStat};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// 特徴なし — neither parallel nor tooling-led (column 0, lowest).
    Scout,
    /// 機能活用 — tooling-dominant: leans on subagents / skills / MCP (column 1).
    Tools,
    /// 並列 — parallel-dominant: runs many agents at once (column 2).
    Parallel,
    /// 頂点 — both parallel AND tooling high: the full orchestrator (column 3,
    /// highest). The apex cell (R1 × this column) is `Lion`.
    Apex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codename {
    /// Time-of-day word: "Aurora" / "Sol" / "Luna" / "Eclipse".
    pub ops: &'static str,
    /// Grid animal; `FLOOR_ANIMAL` ("Ant") for anything below the grid floor.
    pub animal: &'static str,
}

impl Codename {
    /// The displayed title, always `"<OPS> <animal>"`.
    pub fn title(&self) -> String {
        format!("{} {}", self.ops, self.animal)
    }
}

/// The lowest rank — used both for R8 in the grid and for anything below the
/// floor, so the bottom of the ladder is always "Ant".
const FLOOR_ANIMAL: &str = "Ant";

// ===== Tunable thresholds — single source of truth =========================
// ROW is volume; COLUMN is the orchestration tier. They are deliberately
// correlated (high orchestration needs volume), which the trapezoid embraces.
// All numbers are provisional and meant to be recalibrated against a real
// population — only the author's own data is available so far. Retune here only.

/// The level always reflects the most recent N days of token throughput.
/// The analyzer fills `Summary::recent_window_volume` over this same window.
pub(crate) const CODENAME_WINDOW_DAYS: i64 = 30;

/// ROW (R1 top .. R8 entry) by tokens per day over the window. Volume only.
/// (Monthly equivalents: R1 ≥22.5B, R2 ≥12B, R3 ≥6.6B, R4 ≥3.6B, R5 ≥1.35B,
/// R6 ≥360M, R7 ≥90M, R8 ≥15M — these ÷30.)
const TOKENS_PER_DAY: [f64; 8] = [
    750_000_000.0, // R1
    400_000_000.0, // R2
    220_000_000.0, // R3
    120_000_000.0, // R4
    45_000_000.0,  // R5
    12_000_000.0,  // R6
    3_000_000.0,   // R7
    // R8's threshold IS the floor: anything below maps to the same "Ant" rank,
    // so `band_row`'s "below grid" (9) is unreachable on the real path. Keep them
    // tied so a future retune can't open a gap between sub-floor and R8.
    FLOOR_MIN_TOKENS_PER_DAY, // R8
];

/// Orchestration normalisation: the value at which each signal reads "fully
/// maxed" (clamped to 1.0 of its axis).
/// 60%+ of active time at 2+ concurrent sessions = fully parallel.
const PARALLEL_FULL: f64 = 0.60;
/// 2%+ of tool calls being subagent / skill / MCP = fully tooling. (Calibrated
/// against the author as the current high-water mark; recalibrate with more
/// data.)
const TOOLING_FULL: f64 = 0.02;

/// Column cut-offs on each normalised axis (0..1).
/// Both axes ≥ HIGH → Apex (頂点). Both axes < LOW → Scout (特徴なし).
const TIER_HIGH: f64 = 0.50;
const TIER_LOW: f64 = 0.25;

/// Low-sample guards: a ratio is only trusted with enough underneath it.
const PARALLEL_MIN_ACTIVE_SECS: u64 = 2 * 60 * 60; // 2h of measured active time
const TOOLING_MIN_CALLS: usize = 50; // total tool calls before trusting the share

/// Below either floor the sample is too thin for a real rank → "Ant".
const FLOOR_MIN_TOKENS_PER_DAY: f64 = 500_000.0;
const FLOOR_MIN_DAYS: usize = 3;

/// OPS is decided when the top time-band leads the second by this many points;
/// otherwise the day is "mixed" → Eclipse.
const OPS_DOMINANCE_PT: f64 = 15.0;

/// Tooling tool names: a short, stable rule (not a long classifier list).
/// A call counts as 機能活用 when it delegates to a subagent, invokes a skill,
/// or reaches an MCP server. New ordinary tools don't change this; new harness
/// features just under-count slightly until added.
fn is_tooling(name: &str) -> bool {
    name.starts_with("mcp__") || matches!(name, "Skill" | "Agent" | "Task" | "spawn_agent")
}

/// Trapezoid: the widest column index reachable at each row (R1..R8). The right
/// columns close off as volume drops, so the grid is an inverted pyramid.
const ROW_MAX_COL: [usize; 8] = [3, 3, 3, 3, 2, 1, 1, 0];

/// 8×4 animal grid: `[row R1..R8][Scout, Tools, Parallel, Apex]` (columns left→
/// right = ascending orchestration). Empty strings are the closed trapezoid
/// cells — never indexed because the column is clamped to `ROW_MAX_COL`. Lion is
/// the apex at R1 × Apex; families run down the columns (aquatic, birds, …).
const GRID: [[&str; 4]; 8] = [
    ["Orca", "Hawk", "Puma", "Lion"],            // R1
    ["Whale", "Raven", "Bear", "Wolf"],          // R2
    ["Octopus", "Gull", "Kangaroo", "Doberman"], // R3
    ["Eel", "Swallow", "Deer", "Hound"],         // R4
    ["Piranha", "Cat", "Fox", ""],               // R5
    ["Bee", "Scorpion", "", ""],                 // R6
    ["Firefly", "Butterfly", "", ""],            // R7
    ["Ant", "", "", ""],                         // R8
];

// ===========================================================================

struct Metrics {
    parallel_share: f64,       // share of active time at 2+ concurrent
    tooling_ratio: f64,        // share of tool calls that are subagent / skill / MCP
    tokens_per_day: f64,       // tokens/day over the most recent window (level)
    window_active_days: usize, // active days over the fixed 30-day window (floor gate)
}

/// Public entry: derive the codename for a summary. Computed on demand at
/// display time, never stored, so the analyzer stays free of vanity logic.
pub fn for_summary(summary: &Summary) -> Codename {
    for_summary_styled(summary, summary)
}

/// Like [`for_summary`], but the orchestration column is taken from `style_src`
/// while the row, the OPS prefix, and the floor check come from `summary`.
///
/// The UI passes the combined summary as `style_src`. Orchestration is a
/// whole-person trait measured across every agent at once, so a provider tab
/// shows your overall column at that tab's own volume row — the animal changes by
/// tier, the column word stays your identity. When `summary` is the combined one
/// this is identical to [`for_summary`].
pub fn for_summary_styled(summary: &Summary, style_src: &Summary) -> Codename {
    let m = metrics(summary);
    // Below the grid floor everyone lands on the lowest rank, "Ant" — the same
    // animal as R8, so the very bottom is always Ant (with a normal OPS prefix).
    if is_below_floor(&m) {
        return Codename {
            ops: ops(&summary.hourly_usage),
            animal: FLOOR_ANIMAL,
        };
    }
    let row = band_row(m.tokens_per_day, &TOKENS_PER_DAY).clamp(1, 8);
    let style = style_of(&metrics(style_src));
    Codename {
        ops: ops(&summary.hourly_usage),
        animal: animal_for(style, row),
    }
}

fn metrics(summary: &Summary) -> Metrics {
    Metrics {
        parallel_share: parallel_share(&summary.orchestration),
        tooling_ratio: tooling_ratio(&summary.tools),
        tokens_per_day: tokens_per_day(summary),
        window_active_days: summary.recent_window_active_days,
    }
}

/// Share of active wall-time spent at 2+ concurrent sessions — the parallel
/// axis. Volume-normalised: a hand-driver sits at concurrency 1, a fan-out
/// operator spends most of their time at 2+, regardless of total tokens. Guarded:
/// under a couple of hours of measured activity the ratio is too noisy to trust,
/// so it reads as 0. (Casts covered by the module-level `cast_precision_loss`
/// allow; seconds stay far below 2^53.)
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

/// Share of tool calls that delegate or invoke a capability beyond raw file /
/// shell ops (subagent / skill / MCP) — the 機能活用 axis. Volume-normalised
/// (a ratio, not a count). Guarded by a minimum total so a thin sample can't
/// swing it. (Casts covered by the module-level `cast_precision_loss` allow;
/// call counts stay far below 2^53.)
fn tooling_ratio(tools: &[ToolStat]) -> f64 {
    let total: usize = tools.iter().map(|tool| tool.calls).sum();
    if total < TOOLING_MIN_CALLS {
        return 0.0;
    }
    let tooling: usize = tools
        .iter()
        .filter(|tool| is_tooling(&tool.name))
        .map(|tool| tool.calls)
        .sum();
    tooling as f64 / total as f64
}

/// Tokens per day over the fixed codename window. `recent_window_volume` is the
/// analyzer's last-30-day token sum, computed independently of the display
/// `--days`, so the level is the same whether the user views 7, 30, or 90 days.
fn tokens_per_day(summary: &Summary) -> f64 {
    summary.recent_window_volume as f64 / CODENAME_WINDOW_DAYS as f64
}

/// Below either floor (too few tokens/day, or too few active days) the sample is
/// too thin for a real rank, so the user sits at the floor ("Ant").
fn is_below_floor(m: &Metrics) -> bool {
    m.tokens_per_day < FLOOR_MIN_TOKENS_PER_DAY || m.window_active_days < FLOOR_MIN_DAYS
}

/// The orchestration column from the two normalised axes. Apex needs both high
/// (a strict gate, so the top is meaningful); Scout is both low; otherwise the
/// dominant axis wins, and `Parallel` ranks above `Tools` so parallel-dominant
/// users land in a higher column than tooling-dominant ones.
fn style_of(m: &Metrics) -> Style {
    let parallel = (m.parallel_share / PARALLEL_FULL).clamp(0.0, 1.0);
    let tooling = (m.tooling_ratio / TOOLING_FULL).clamp(0.0, 1.0);

    if parallel >= TIER_HIGH && tooling >= TIER_HIGH {
        Style::Apex
    } else if parallel < TIER_LOW && tooling < TIER_LOW {
        Style::Scout
    } else if parallel >= tooling {
        Style::Parallel
    } else {
        Style::Tools
    }
}

/// Combined view used by the tests: `None` => below floor, otherwise `(style, row
/// 1..=8)`.
#[cfg(test)]
fn classify(m: &Metrics) -> Option<(Style, usize)> {
    if is_below_floor(m) {
        return None;
    }
    let row = band_row(m.tokens_per_day, &TOKENS_PER_DAY).clamp(1, 8);
    Some((style_of(m), row))
}

/// First row (1..=8) whose threshold `raw` clears, or 9 if below the grid.
fn band_row(raw: f64, thresholds: &[f64; 8]) -> usize {
    for (index, threshold) in thresholds.iter().enumerate() {
        if raw >= *threshold {
            return index + 1;
        }
    }
    9
}

/// The animal at `(style column, row)`, with the column clamped to the row's
/// widest reachable column (the trapezoid). So an orchestration tier above the
/// row's reach shows that row's strongest available animal, never a dead cell.
fn animal_for(style: Style, row: usize) -> &'static str {
    let tier = match style {
        Style::Scout => 0,
        Style::Tools => 1,
        Style::Parallel => 2,
        Style::Apex => 3,
    };
    let index = row.clamp(1, 8) - 1;
    let column = tier.min(ROW_MAX_COL[index]);
    GRID[index][column]
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
    bands.sort_by_key(|band| std::cmp::Reverse(band.1));
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
    use crate::model::ToolStat;

    fn base() -> Metrics {
        Metrics {
            parallel_share: 0.0,
            tooling_ratio: 0.0,
            tokens_per_day: 250_000_000.0, // R3
            window_active_days: 20,
        }
    }

    #[test]
    fn low_tokens_is_below_floor() {
        let m = Metrics {
            tokens_per_day: 100_000.0,
            ..base()
        };
        assert!(classify(&m).is_none());
    }

    #[test]
    fn short_window_active_days_is_below_floor() {
        let m = Metrics {
            window_active_days: 2,
            ..base()
        };
        assert!(classify(&m).is_none());
    }

    #[test]
    fn both_axes_low_is_scout() {
        // parallel norm 0.17, tooling norm 0.15 — both below LOW → Scout.
        let m = Metrics {
            parallel_share: 0.10,
            tooling_ratio: 0.003,
            ..base()
        };
        assert_eq!(classify(&m), Some((Style::Scout, 3)));
        assert_eq!(animal_for(Style::Scout, 3), "Octopus");
    }

    #[test]
    fn parallel_dominant_is_parallel() {
        // parallel norm 0.83 (≥LOW, <HIGH not required), tooling norm 0.10 →
        // dominant axis is parallel → Parallel column.
        let m = Metrics {
            parallel_share: 0.50,
            tooling_ratio: 0.002,
            ..base()
        };
        assert_eq!(classify(&m), Some((Style::Parallel, 3)));
        assert_eq!(animal_for(Style::Parallel, 3), "Kangaroo");
    }

    #[test]
    fn tooling_dominant_is_tools() {
        // parallel norm 0.08 (<LOW), tooling norm 0.75 (≥LOW) → tooling dominant.
        let m = Metrics {
            parallel_share: 0.05,
            tooling_ratio: 0.015,
            ..base()
        };
        assert_eq!(classify(&m), Some((Style::Tools, 3)));
        assert_eq!(animal_for(Style::Tools, 3), "Gull");
    }

    #[test]
    fn both_high_is_apex() {
        // parallel norm 1.0, tooling norm 1.0 → both ≥ HIGH → Apex.
        let m = Metrics {
            parallel_share: 0.60,
            tooling_ratio: 0.02,
            ..base()
        };
        assert_eq!(classify(&m), Some((Style::Apex, 3)));
        assert_eq!(animal_for(Style::Apex, 3), "Doberman");
    }

    #[test]
    fn apex_at_r1_is_lion() {
        let m = Metrics {
            parallel_share: 0.80,
            tooling_ratio: 0.03,
            tokens_per_day: 800_000_000.0, // R1
            window_active_days: 28,
        };
        assert_eq!(classify(&m), Some((Style::Apex, 1)));
        assert_eq!(animal_for(Style::Apex, 1), "Lion");
    }

    #[test]
    fn trapezoid_clamps_high_tier_at_low_row() {
        // Apex orchestration but only R7 volume: the row reaches column 1 at
        // most, so the apex tier is clamped back to the Tools column → Butterfly,
        // never a dead Apex cell.
        let m = Metrics {
            parallel_share: 0.80,
            tooling_ratio: 0.03,
            tokens_per_day: 5_000_000.0, // R7
            window_active_days: 20,
        };
        assert_eq!(classify(&m), Some((Style::Apex, 7)));
        assert_eq!(animal_for(Style::Apex, 7), "Butterfly");
    }

    #[test]
    fn tooling_ratio_share_and_guard() {
        // 100 tooling calls out of 1000 → 0.10.
        let tools = vec![
            ToolStat {
                name: "Bash".to_owned(),
                calls: 900,
            },
            ToolStat {
                name: "Agent".to_owned(),
                calls: 60,
            },
            ToolStat {
                name: "Skill".to_owned(),
                calls: 40,
            },
        ];
        assert!((tooling_ratio(&tools) - 0.10).abs() < 1e-9);
        // mcp__ prefix counts; below the min-call guard it reads as 0.
        let thin = vec![ToolStat {
            name: "mcp__notion__fetch".to_owned(),
            calls: 5,
        }];
        assert!(tooling_ratio(&thin).abs() < f64::EPSILON);
    }

    #[test]
    fn band_row_eight_bands() {
        assert_eq!(band_row(800_000_000.0, &TOKENS_PER_DAY), 1);
        assert_eq!(band_row(250_000_000.0, &TOKENS_PER_DAY), 3); // ≥220M
        assert_eq!(band_row(200_000_000.0, &TOKENS_PER_DAY), 4); // <220M
        assert_eq!(band_row(600_000.0, &TOKENS_PER_DAY), 8); // ≥500K
        assert_eq!(band_row(100_000.0, &TOKENS_PER_DAY), 9); // below grid
    }

    /// A combined summary that lands `Apex` R1 (`Lion`): parallel (sample's
    /// time-by-level gives ≈0.61 at 2+) plus a tooling-heavy tool mix and top-row
    /// volume.
    fn apex_combined() -> Summary {
        let mut s = crate::share::fixtures::sample_summary();
        s.tools = vec![
            ToolStat {
                name: "Bash".to_owned(),
                calls: 900,
            },
            ToolStat {
                name: "Agent".to_owned(),
                calls: 60,
            },
            ToolStat {
                name: "Skill".to_owned(),
                calls: 40,
            },
        ];
        s.recent_window_volume = 800_000_000 * CODENAME_WINDOW_DAYS as u64; // R1
        s.recent_window_active_days = 29;
        s
    }

    #[test]
    fn combined_apex_is_lion() {
        assert_eq!(for_summary(&apex_combined()).animal, "Lion");
    }

    #[test]
    fn tab_shows_combined_style_at_own_row() {
        // Column is whole-person (from the combined summary); only the row is
        // per-tab. A lower-volume tab of an Apex shows the Apex column at its own
        // row → R3 × Apex = Doberman.
        let combined = apex_combined();
        let mut tab = combined.clone();
        tab.provider = crate::model::Provider::Claude;
        tab.recent_window_volume = 250_000_000 * CODENAME_WINDOW_DAYS as u64; // R3
        assert_eq!(for_summary_styled(&tab, &combined).animal, "Doberman");
    }

    #[test]
    fn non_parallel_tab_still_inherits_combined_style() {
        // A tab you never parallelise (and with no tooling of its own) still
        // inherits the combined column — orchestration is measured whole-person.
        let combined = apex_combined();
        let mut tab = combined.clone();
        tab.orchestration.time_by_level = [10_000, 0, 0, 0, 0, 0];
        tab.tools = vec![ToolStat {
            name: "Bash".to_owned(),
            calls: 500,
        }];
        tab.recent_window_volume = 250_000_000 * CODENAME_WINDOW_DAYS as u64; // R3
        assert_eq!(for_summary_styled(&tab, &combined).animal, "Doberman");
    }

    #[test]
    fn tab_below_floor_is_ant_regardless_of_src_style() {
        let combined = apex_combined();
        let mut tab = combined.clone();
        tab.recent_window_volume = 100_000 * CODENAME_WINDOW_DAYS as u64; // below floor
        assert_eq!(for_summary_styled(&tab, &combined).animal, "Ant");
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
