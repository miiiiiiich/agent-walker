//! Absolute token throughput needs no accounts or population data.
//! Steps subdivide their rank's band on a log scale, so progress feels even
//! within a rank. More steps in higher ranks make the climb longer.
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
    pub ops: &'static str,
    /// The animal already encodes the step, so surfaces show rank letters without a step counter.
    pub animal: &'static str,
    pub rank: Rank,
}

impl Codename {
    pub fn title(&self) -> String {
        format!("{} {}", self.ops, self.animal)
    }
}

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

const SS_LION_MIN: f64 = 1_000_000_000.0;

const FLOOR_ANIMAL: &str = "Ant";

const FLOOR_MIN_DAYS: usize = 3;

const OPS_DOMINANCE_PT: f64 = 15.0;

/// Public entry: derive the codename for a summary. Computed on demand at
/// display time, never stored, so the analyzer stays free of vanity logic.
pub fn for_summary(summary: &Summary) -> Codename {
    let ops = ops(&summary.hourly_usage);
    let tokens_per_day =
        summary.recent_window_volume as f64 / f64::from(summary.period_days.max(1));
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

fn step_index(tokens_per_day: f64, band_min: f64, band_max: f64, steps: usize) -> usize {
    debug_assert!(steps > 0, "every rank band must hold at least one animal");
    let position = (tokens_per_day / band_min).ln() / (band_max / band_min).ln();
    let index = (position * steps as f64).floor() as usize;
    index.min(steps - 1)
}

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

    fn summary_at(tokens_per_day: u64) -> Summary {
        let mut summary = crate::share::fixtures::sample_summary();
        summary.recent_window_volume = tokens_per_day * u64::from(summary.period_days);
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
        assert_eq!(codename_at(130_000_000).animal, "Eel");
        let swallow = codename_at(150_000_000);
        assert_eq!(swallow.animal, "Swallow");
        assert_eq!(swallow.rank, Rank::B);
        assert_eq!(codename_at(170_000_000).animal, "Deer");
        assert_eq!(codename_at(200_000_000).animal, "Hound");
    }

    #[test]
    fn ss_band_is_anchored_so_lion_begins_at_1b_per_day() {
        assert_eq!(codename_at(800_000_000).animal, "Orca");
        assert_eq!(codename_at(850_000_000).animal, "Hawk");
        assert_eq!(codename_at(950_000_000).animal, "Puma");
        assert_eq!(codename_at(999_999_999).animal, "Puma");
        let lion = codename_at(1_000_000_000);
        assert_eq!(lion.animal, "Lion");
        assert_eq!(lion.rank, Rank::SS);
        assert_eq!(codename_at(100_000_000_000).animal, "Lion");
    }

    #[test]
    fn every_animal_is_reachable() {
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
        let combined = summary_at(800_000_000);
        let mut tab = combined.clone();
        tab.provider = crate::model::Provider::Claude;
        tab.recent_window_volume = 250_000_000 * u64::from(tab.period_days);
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
