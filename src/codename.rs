//! Codename: a playful vanity title derived from a usage `Summary`.
//!
//! Shown as `[OPS] [ANIMAL]`, e.g. "Eclipse Puma". The title is earned on a
//! single absolute axis — token throughput over the most recent 30 days — so
//! it needs no accounts and no population data. RANK is the letter tier (SS at
//! the top, then S/A/B/C/D/E; below E is unranked); STEP is the position
//! inside the rank's token band, and each step is one animal. The 24 animals
//! form one ladder from Ant (the unranked floor) to Lion (the final SS step):
//! keep using your agents and you pass through every animal on the way up.
//! OPS is the dominant time-of-day prefix.
//!
//! Steps subdivide their rank's band on a log scale, so progress feels even
//! within a rank. Higher ranks hold more steps (4 at the top, 1 at the floor),
//! so the climb gets longer as the air gets thinner. SS has no hard upper
//! edge; the band is anchored so its final step (Lion) begins at
//! [`SS_LION_MIN`] — 1B tokens/day, 30B over the 30-day window — and anything
//! past the extrapolated edge clamps to Lion.
//!
//! The exact thresholds live in the one block below, are easy to retune, and
//! are never surfaced in the UI — only the rank and the title are — so the
//! formula stays opaque even though the source is public.

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Scores are display-only thresholds; approximate float math never feeds back into integer state."
)]

use crate::model::Summary;

/// Letter tier of the ladder. `Unranked` is everything below the E band (or a
/// sample too thin to rank) — always shown as the floor animal with no letter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rank {
    SS,
    S,
    A,
    B,
    C,
    D,
    E,
    Unranked,
}

impl Rank {
    /// Display letters; `None` for unranked (surfaces draw no rank badge).
    pub fn letters(self) -> Option<&'static str> {
        match self {
            Rank::SS => Some("SS"),
            Rank::S => Some("S"),
            Rank::A => Some("A"),
            Rank::B => Some("B"),
            Rank::C => Some("C"),
            Rank::D => Some("D"),
            Rank::E => Some("E"),
            Rank::Unranked => None,
        }
    }

    /// Canonical rank colour as RGB, following the 冠位十二階 ladder (603 AD —
    /// the oldest colour-coded rank system): deep purple at the top, then pale
    /// purple / blue / red / yellow / white down to ink-black, with hues
    /// lifted for the dark card. `None` for unranked. Renderers must go
    /// through [`Self::display_rgb`] instead — the raw ink-black E sinks on
    /// the dark surfaces.
    pub fn color_rgb(self) -> Option<(u8, u8, u8)> {
        match self {
            Rank::SS => Some((0xa6, 0x78, 0xf0)), // 濃紫 (大徳)
            Rank::S => Some((0xc9, 0xb3, 0xee)),  // 薄紫 (小徳)
            Rank::A => Some((0x6b, 0x9b, 0xd8)),  // 青 (仁)
            Rank::B => Some((0xd9, 0x70, 0x70)),  // 赤 (礼)
            Rank::C => Some((0xdf, 0xc1, 0x69)),  // 黄 (信)
            Rank::D => Some((0xd8, 0xd6, 0xcf)),  // 白 (義)
            Rank::E => Some((0x4d, 0x52, 0x5a)),  // 黒/墨 (智)
            Rank::Unranked => None,
        }
    }

    /// [`Self::color_rgb`] adjusted for rendering on the dark surfaces: the
    /// ink-black E sinks below the surrounding chrome there, so both the card
    /// badge and the TUI nameplate draw it with this lifted shade.
    pub fn display_rgb(self) -> Option<(u8, u8, u8)> {
        match self {
            Rank::E => Some((0x7a, 0x80, 0x88)),
            _ => self.color_rgb(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codename {
    /// Time-of-day word: "Aurora" / "Sol" / "Luna" / "Eclipse".
    pub ops: &'static str,
    /// Ladder animal; `FLOOR_ANIMAL` ("Ant") when unranked. The animal already
    /// encodes the step inside the rank, so surfaces show only rank letters —
    /// never a step counter.
    pub animal: &'static str,
    /// Letter tier.
    pub rank: Rank,
}

impl Codename {
    /// The displayed title, always `"<OPS> <animal>"`.
    pub fn title(&self) -> String {
        format!("{} {}", self.ops, self.animal)
    }
}

// ===== Tunable thresholds — single source of truth =========================
// One axis only: tokens/day over the window. All numbers are provisional and
// meant to be recalibrated against real-world reports. Retune here only.

/// The level always reflects the most recent N days of token throughput.
/// The analyzer fills `Summary::recent_window_volume` over this same window.
pub(crate) const CODENAME_WINDOW_DAYS: i64 = 30;

/// The ladder, top rank first: minimum tokens/day over the window, plus the
/// rank's animals as ascending steps toward the next rank. Steps split the
/// band log-uniformly, so each step is the same *ratio* of growth. Every
/// animal is a milestone on one climb — all 24 are reachable.
/// (Monthly equivalents: SS ≥22.5B, S ≥12B, A ≥6.6B, B ≥3.6B, C ≥1.35B,
/// D ≥360M, E ≥90M — these ÷30.)
const LADDER: [(Rank, f64, &[&str]); 7] = [
    (Rank::SS, 750_000_000.0, &["Orca", "Hawk", "Puma", "Lion"]),
    (Rank::S, 400_000_000.0, &["Whale", "Raven", "Bear", "Wolf"]),
    (
        Rank::A,
        220_000_000.0,
        &["Octopus", "Gull", "Kangaroo", "Doberman"],
    ),
    (Rank::B, 120_000_000.0, &["Eel", "Swallow", "Deer", "Hound"]),
    (Rank::C, 45_000_000.0, &["Piranha", "Cat", "Fox"]),
    (Rank::D, 12_000_000.0, &["Bee", "Scorpion"]),
    (Rank::E, 3_000_000.0, &["Firefly", "Butterfly"]),
];

/// Tokens/day where the top SS step (Lion) begins — 30B over the 30-day
/// window. Retuned 2026-07-13: the old band-ratio extrapolation put Lion near
/// 5B/day (148B monthly), which no real operator could reach.
const SS_LION_MIN: f64 = 1_000_000_000.0;

/// The floor below the ladder — the unranked animal.
const FLOOR_ANIMAL: &str = "Ant";

/// Under this many active days in the window the sample is too thin to rank.
const FLOOR_MIN_DAYS: usize = 3;

/// OPS is decided when the top time-band leads the second by this many points;
/// otherwise the day is "mixed" → Eclipse.
const OPS_DOMINANCE_PT: f64 = 15.0;

// ===========================================================================

/// Public entry: derive the codename for a summary. Computed on demand at
/// display time, never stored, so the analyzer stays free of vanity logic.
/// Every summary ranks on its own volume — a provider tab shows the rank that
/// tab's throughput earns by itself.
pub fn for_summary(summary: &Summary) -> Codename {
    let ops = ops(&summary.hourly_usage);
    let tokens_per_day = summary.recent_window_volume as f64 / CODENAME_WINDOW_DAYS as f64;
    if summary.recent_window_active_days < FLOOR_MIN_DAYS {
        return unranked(ops);
    }
    let Some(position) = LADDER.iter().position(|(_, min, _)| tokens_per_day >= *min) else {
        return unranked(ops);
    };
    let (rank, band_min, animals) = LADDER[position];
    // The SS anchor is a contract ("the last step begins at SS_LION_MIN"), so
    // enforce it by direct comparison — the log-position math below can land a
    // value sitting exactly on the anchor one step short through float
    // rounding.
    let step = if rank == Rank::SS && tokens_per_day >= SS_LION_MIN {
        animals.len() - 1
    } else {
        step_index(
            tokens_per_day,
            band_min,
            band_ceiling(position),
            animals.len(),
        )
    };
    Codename {
        ops,
        animal: animals[step],
        rank,
    }
}

/// Every ladder animal, floor first — badge assets must cover exactly this set.
#[cfg(test)]
pub(crate) fn all_animals() -> impl Iterator<Item = &'static str> {
    std::iter::once(FLOOR_ANIMAL).chain(
        LADDER
            .iter()
            .rev()
            .flat_map(|(_, _, animals)| animals.iter().copied()),
    )
}

fn unranked(ops: &'static str) -> Codename {
    Codename {
        ops,
        animal: FLOOR_ANIMAL,
        rank: Rank::Unranked,
    }
}

/// Upper edge of the band at `position`. The top band (SS) is open-ended, so
/// it is anchored to [`SS_LION_MIN`]: the log-uniform step ratio is chosen so
/// the last step (Lion) begins exactly there, and the ceiling sits one step
/// above it.
fn band_ceiling(position: usize) -> f64 {
    if position == 0 {
        let (_, min, animals) = LADDER[0];
        debug_assert!(
            animals.len() >= 2,
            "the SS band needs at least two animals to anchor its step ratio"
        );
        let per_step = (SS_LION_MIN / min).powf(1.0 / (animals.len() as f64 - 1.0));
        SS_LION_MIN * per_step
    } else {
        LADDER[position - 1].1
    }
}

/// 0-based step inside `[band_min, band_max)` split log-uniformly into
/// `steps` parts; values past the top edge clamp to the last step.
fn step_index(tokens_per_day: f64, band_min: f64, band_max: f64, steps: usize) -> usize {
    debug_assert!(steps > 0, "every rank band must hold at least one animal");
    let position = (tokens_per_day / band_min).ln() / (band_max / band_min).ln();
    let index = (position * steps as f64).floor() as usize;
    index.min(steps - 1)
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

    /// A summary with the given tokens/day over the codename window and a
    /// healthy number of active days.
    fn summary_at(tokens_per_day: u64) -> Summary {
        let mut summary = crate::share::fixtures::sample_summary();
        summary.recent_window_volume = tokens_per_day * CODENAME_WINDOW_DAYS as u64;
        summary.recent_window_active_days = 25;
        summary
    }

    fn codename_at(tokens_per_day: u64) -> Codename {
        for_summary(&summary_at(tokens_per_day))
    }

    #[test]
    fn below_e_band_is_unranked_ant() {
        let codename = codename_at(2_000_000);
        assert_eq!(codename.animal, "Ant");
        assert_eq!(codename.rank, Rank::Unranked);
        assert_eq!(codename.rank.letters(), None);
    }

    #[test]
    fn short_window_active_days_is_unranked() {
        let mut summary = summary_at(250_000_000);
        summary.recent_window_active_days = 2;
        let codename = for_summary(&summary);
        assert_eq!(codename.animal, "Ant");
        assert_eq!(codename.rank, Rank::Unranked);
    }

    #[test]
    fn each_band_floor_is_its_first_animal() {
        let expected = [
            (3_000_000, Rank::E, "Firefly"),
            (12_000_000, Rank::D, "Bee"),
            (45_000_000, Rank::C, "Piranha"),
            (120_000_000, Rank::B, "Eel"),
            (220_000_000, Rank::A, "Octopus"),
            (400_000_000, Rank::S, "Whale"),
            (750_000_000, Rank::SS, "Orca"),
        ];
        for (tokens, rank, animal) in expected {
            let codename = codename_at(tokens);
            assert_eq!(codename.rank, rank, "{tokens}/day");
            assert_eq!(codename.animal, animal, "{tokens}/day");
        }
    }

    #[test]
    fn steps_advance_log_uniformly_within_a_band() {
        // B band = 120M..220M with 4 steps; log-uniform boundaries land near
        // 140M / 163M / 189M.
        assert_eq!(codename_at(130_000_000).animal, "Eel");
        let swallow = codename_at(150_000_000);
        assert_eq!(swallow.animal, "Swallow");
        assert_eq!(swallow.rank, Rank::B);
        assert_eq!(codename_at(170_000_000).animal, "Deer");
        assert_eq!(codename_at(200_000_000).animal, "Hound");
    }

    #[test]
    fn ss_band_is_anchored_so_lion_begins_at_1b_per_day() {
        // SS is anchored to SS_LION_MIN (1B/day = 30B per 30-day window):
        // 750M / ~825M / ~909M / 1B.
        assert_eq!(codename_at(800_000_000).animal, "Orca");
        assert_eq!(codename_at(850_000_000).animal, "Hawk");
        assert_eq!(codename_at(950_000_000).animal, "Puma");
        // The anchor is inclusive: exactly on it is Lion, one below is not.
        assert_eq!(codename_at(999_999_999).animal, "Puma");
        let lion = codename_at(1_000_000_000);
        assert_eq!(lion.animal, "Lion");
        assert_eq!(lion.rank, Rank::SS);
        // Past the extrapolated edge the top step holds — Lion is the summit.
        assert_eq!(codename_at(100_000_000_000).animal, "Lion");
    }

    #[test]
    fn every_animal_is_reachable() {
        // 全名回収: sweep the volume axis and confirm the ladder passes through
        // all 24 animals — no dead cells.
        let mut reached: Vec<&str> = vec![codename_at(1_000_000).animal];
        let mut tokens_per_day = 3_000_000_f64;
        while tokens_per_day < 20_000_000_000.0 {
            reached.push(codename_at(tokens_per_day as u64).animal);
            tokens_per_day *= 1.02;
        }
        let mut expected: Vec<&str> = all_animals().collect();
        reached.sort_unstable();
        reached.dedup();
        expected.sort_unstable();
        assert_eq!(reached, expected);
        assert_eq!(all_animals().count(), 24);
    }

    #[test]
    fn tabs_rank_on_their_own_volume() {
        // No whole-person style inheritance anymore: a provider tab earns the
        // rank its own throughput clears.
        let combined = summary_at(800_000_000);
        let mut tab = combined.clone();
        tab.provider = crate::model::Provider::Claude;
        tab.recent_window_volume = 250_000_000 * CODENAME_WINDOW_DAYS as u64;
        assert_eq!(for_summary(&combined).animal, "Orca");
        assert_eq!(for_summary(&tab).animal, "Octopus");
        assert_eq!(for_summary(&tab).rank, Rank::A);
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
