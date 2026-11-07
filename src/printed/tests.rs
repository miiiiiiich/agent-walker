use std::ffi::OsStr;
use std::io::{self, ErrorKind, Write};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{print_bytes, render, text};
use crate::ui::{MUTED, TEXT};

#[test]
fn the_output_starts_with_a_blank_line() {
    let printed = String::from_utf8(render(&[Line::raw("first")], false).unwrap()).unwrap();
    assert_eq!(printed, "\nfirst\n");
}

#[test]
fn text_is_plain_without_color_and_ansi_with_it() {
    crossterm::style::force_color_output(true);
    let line = Line::from(Span::styled("label", Style::default().fg(MUTED)));
    let plain = text::lines(std::slice::from_ref(&line), false).unwrap();
    assert!(!plain.contains(&0x1b));
    let colored = String::from_utf8(text::lines(&[line], true).unwrap()).unwrap();
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
fn control_and_invisible_characters_never_reach_the_terminal() {
    let line = Line::raw("a\u{1b}[31m\u{9b}\u{202e}\u{200b}\u{e0041}b");
    let printed = String::from_utf8(text::lines(&[line], true).unwrap()).unwrap();
    assert!(printed.contains("a[31mb"), "{printed:?}");
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
