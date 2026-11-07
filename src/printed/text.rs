use std::ffi::OsStr;
use std::io::{self, Write};

use anyhow::{Context, Result};
use crossterm::queue;
use crossterm::style::{Attribute, Color as TermColor, Print, SetAttribute, SetForegroundColor};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

use super::clean::clean;
use crate::ui::TEXT;

pub(super) fn lines(lines: &[Line<'_>], color: bool) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for line in lines {
        for span in &line.spans {
            write_span(
                &mut out,
                &clean(&span.content),
                line.style.patch(span.style),
                color,
            )
            .context("format the text")?;
        }
        out.push(b'\n');
    }
    Ok(out)
}

fn write_span(out: &mut Vec<u8>, text: &str, style: Style, color: bool) -> io::Result<()> {
    if !color {
        return out.write_all(text.as_bytes());
    }
    // `TEXT` assumes the TUI's black background and all but vanishes on a light
    // terminal, so body text keeps the terminal's own color.
    if let Some(Color::Rgb(r, g, b)) = style.fg.filter(|&fg| fg != TEXT) {
        queue!(out, SetForegroundColor(TermColor::Rgb { r, g, b }))?;
    }
    if style.add_modifier.contains(Modifier::BOLD) {
        queue!(out, SetAttribute(Attribute::Bold))?;
    }
    queue!(out, Print(text), SetAttribute(Attribute::Reset))
}

pub(super) fn color_allowed(
    is_terminal: bool,
    no_color: Option<&OsStr>,
    term: Option<&OsStr>,
) -> bool {
    is_terminal
        && no_color.is_none_or(OsStr::is_empty)
        && term.is_none_or(|term| term != OsStr::new("dumb"))
}
