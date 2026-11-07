//! What the commands that print and exit write to the terminal.

use std::env;
use std::io::{self, ErrorKind, IsTerminal, Write};

use anyhow::{Context, Result};
use ratatui::text::Line;

mod clean;
#[cfg(test)]
mod tests;
mod text;

/// Narrower terminals draw at this width: below it a row cannot hold an ordinary value.
const MIN_WIDTH: u16 = 24;
const DEFAULT_WIDTH: u16 = 80;

pub(crate) use clean::clean;

pub(crate) struct Output {
    pub(crate) width: u16,
    pub(crate) color: bool,
}

pub(crate) fn output() -> Output {
    let is_terminal = io::stdout().is_terminal();
    Output {
        width: terminal_width(is_terminal).max(MIN_WIDTH),
        color: text::color_allowed(
            is_terminal,
            env::var_os("NO_COLOR").as_deref(),
            env::var_os("TERM").as_deref(),
        ),
    }
}

pub(crate) fn print(lines: &[Line<'_>], color: bool) -> Result<()> {
    print_bytes(&mut io::stdout().lock(), &render(lines, color)?)
}

fn render(lines: &[Line<'_>], color: bool) -> Result<Vec<u8>> {
    let mut bytes = vec![b'\n'];
    bytes.extend(text::lines(lines, color)?);
    Ok(bytes)
}

fn print_bytes(out: &mut impl Write, bytes: &[u8]) -> Result<()> {
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
        other => other.context("write to stdout"),
    }
}

fn terminal_width(is_terminal: bool) -> u16 {
    if !is_terminal {
        return DEFAULT_WIDTH;
    }
    // A pty nobody sized reports zero columns.
    crossterm::terminal::size()
        .ok()
        .map(|(columns, _)| columns)
        .filter(|&columns| columns > 0)
        .unwrap_or(DEFAULT_WIDTH)
}
