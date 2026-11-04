use std::ffi::OsStr;
use std::io::{self, ErrorKind, Write};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{lines, print_bytes, render, text};
use crate::codename::{Codename, Rank, all_animals};
use crate::share::ShareCard;
use crate::share::fixtures::sample_summary;
use crate::ui::TEXT;

fn plain(width: u16) -> Vec<String> {
    lines::lines(&sample_summary(), width)
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn the_output_starts_with_a_blank_line() {
    let printed = String::from_utf8(render(&sample_summary(), 100, false).unwrap()).unwrap();
    let title = crate::codename::for_summary(&sample_summary()).title();
    assert!(printed.starts_with(&format!("\n{title}")), "{printed:?}");
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

#[test]
fn text_is_plain_without_color_and_ansi_with_it() {
    crossterm::style::force_color_output(true);
    let lines = lines::lines(&sample_summary(), 60);
    let plain = text::lines(&lines, false).unwrap();
    assert!(!plain.contains(&0x1b));
    let colored = String::from_utf8(text::lines(&lines, true).unwrap()).unwrap();
    assert!(colored.contains("38;2;"), "{colored:?}");
}

#[test]
fn body_text_keeps_the_terminals_own_color() {
    crossterm::style::force_color_output(true);
    let style = Style::default().fg(TEXT).add_modifier(Modifier::BOLD);
    let line = Line::from(Span::styled("23.2B", style));
    let printed = String::from_utf8(text::lines(&[line], true).unwrap()).unwrap();
    assert!(!printed.contains("38;2;"), "{printed:?}");
    assert!(printed.contains("\u{1b}[1m"), "{printed:?}");
}

#[test]
fn a_reader_that_closed_early_is_not_an_error() {
    struct Failing(ErrorKind);
    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert!(print_bytes(&mut Failing(ErrorKind::BrokenPipe), b"x").is_ok());
    assert!(print_bytes(&mut Failing(ErrorKind::PermissionDenied), b"x").is_err());
}

#[test]
fn color_follows_the_terminal_and_the_conventions() {
    let empty = OsStr::new("");
    let set = OsStr::new("1");
    assert!(text::color_allowed(true, None, None));
    assert!(text::color_allowed(
        true,
        Some(empty),
        Some(OsStr::new("xterm"))
    ));
    assert!(!text::color_allowed(false, None, None));
    assert!(!text::color_allowed(true, Some(set), None));
    assert!(!text::color_allowed(true, None, Some(OsStr::new("dumb"))));
}
