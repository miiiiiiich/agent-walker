use super::lines;
use crate::codename::{Codename, Rank, all_animals};
use crate::share::ShareCard;
use crate::share::fixtures::sample_summary;

fn plain(width: u16) -> Vec<String> {
    lines::lines(&sample_summary(), width)
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn a_wide_terminal_puts_the_numbers_beside_the_codename() {
    let title = crate::codename::for_summary(&sample_summary()).title();
    let wide = plain(100);
    assert!(wide[0].starts_with(&title), "{wide:#?}");
    assert!(wide[0].contains("Tokens"), "{wide:#?}");
    assert!(wide[1].contains("Working time"), "{wide:#?}");

    let narrow = plain(60);
    assert_eq!(narrow[0], title, "{narrow:#?}");
    assert!(
        narrow.iter().skip(1).any(|line| line.starts_with("Tokens")),
        "{narrow:#?}"
    );
}

#[test]
fn nothing_passes_the_width_with_any_codename() {
    let card = ShareCard::from_summary(&sample_summary());
    for animal in all_animals() {
        for ops in ["Aurora", "Sol", "Luna", "Eclipse"] {
            let codename = Codename {
                ops,
                animal,
                rank: Rank::SS,
            };
            for width in [24, 30, 45, 60, 70, 71, 72, 73, 74, 75, 80, 100, 140, 200] {
                for line in lines::arrange(&codename, &card, width) {
                    assert!(
                        line.width() <= width,
                        "{ops} {animal} at {width}: {:?} is {} wide",
                        line.to_string(),
                        line.width()
                    );
                }
            }
        }
    }
}

#[test]
fn a_narrow_terminal_moves_the_detail_below_the_value() {
    let card = ShareCard::from_summary(&sample_summary());
    let render = |width| -> Vec<String> {
        lines::card_lines(&card, width)
            .iter()
            .map(ToString::to_string)
            .collect()
    };
    let wide = render(80);
    let tokens = wide.iter().find(|line| line.starts_with("Tokens")).unwrap();
    assert!(tokens.contains("/day"), "{wide:#?}");

    let narrow = render(24);
    let tokens = narrow
        .iter()
        .position(|line| line.starts_with("Tokens"))
        .unwrap();
    assert!(!narrow[tokens].contains("/day"), "{narrow:#?}");
    assert!(narrow[tokens + 1].contains("/day"), "{narrow:#?}");
}

#[test]
fn long_durations_stay_inside_the_narrowest_width() {
    let mut card = ShareCard::from_summary(&sample_summary());
    card.completion = Some((
        Vec::new(),
        0,
        3,
        "1h 30m".to_owned(),
        "2h 10m".to_owned(),
        "4h".to_owned(),
    ));
    for line in lines::card_lines(&card, 24) {
        assert!(line.width() <= 24, "{:?}", line.to_string());
    }
}

#[test]
fn a_saturated_value_keeps_a_gap_before_its_detail() {
    let mut card = ShareCard::from_summary(&sample_summary());
    card.cost = Some("$101,330,991,616".to_owned());
    card.cost_per_day = Some("$3,377,699,721".to_owned());
    let rendered: Vec<String> = lines::card_lines(&card, 120)
        .iter()
        .map(ToString::to_string)
        .collect();
    let cost = rendered
        .iter()
        .find(|line| line.starts_with("Cost"))
        .unwrap();
    assert!(
        cost.contains("$101,330,991,616 $3,377,699,721/day"),
        "{cost}"
    );
}

#[test]
fn unknown_values_show_a_dash_and_never_a_zero() {
    let mut card = ShareCard::from_summary(&sample_summary());
    card.cost = None;
    card.cost_per_day = None;
    card.working = None;
    card.completion = None;
    card.tokens_per_min = None;
    card.cached = None;
    let rendered: Vec<String> = lines::card_lines(&card, 60)
        .iter()
        .map(ToString::to_string)
        .collect();
    for label in ["Cost", "Working time", "Turns", "Context"] {
        let line = rendered
            .iter()
            .find(|line| line.starts_with(label))
            .unwrap_or_else(|| panic!("{label} missing: {rendered:#?}"));
        assert!(line.trim_end().ends_with('—'), "{line}");
    }
}
