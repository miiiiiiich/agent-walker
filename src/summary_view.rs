//! `agent-walker summary`: the share card's codename and numbers, printed for a
//! person, or with `--json` handed to a program as data.

use std::env;
use std::io::{self, ErrorKind, IsTerminal, Write};

use anyhow::{Context, Result};
use time::UtcOffset;

use crate::model::{AppSummary, Summary};
use crate::share::{ShareCard, card_svg};

mod lines;
#[cfg(test)]
mod tests;
mod text;

/// Narrower terminals draw at this width: below it a row cannot hold an ordinary value.
const MIN_WIDTH: u16 = 24;
const DEFAULT_WIDTH: u16 = 80;

#[derive(Debug, clap::Args)]
pub struct SummaryArgs {
    /// Output as JSON: the codename, the numbers and the card's SVG (experimental).
    #[arg(long)]
    pub json: bool,
}

pub(crate) fn run(args: &SummaryArgs, report: &AppSummary, offset: UtcOffset) -> Result<()> {
    let stdout = io::stdout();
    if args.json {
        let svg = card_svg(&ShareCard::from_summary(&report.combined));
        let mut out = io::BufWriter::new(stdout.lock());
        return crate::format::write_json_summary(&mut out, report, offset, &svg);
    }

    let is_terminal = stdout.is_terminal();
    let width = terminal_width(is_terminal).max(MIN_WIDTH);
    let color = text::color_allowed(
        is_terminal,
        env::var_os("NO_COLOR").as_deref(),
        env::var_os("TERM").as_deref(),
    );
    let bytes = render(&report.combined, width, color)?;
    print_bytes(&mut stdout.lock(), &bytes)
}

fn print_bytes(out: &mut impl Write, bytes: &[u8]) -> Result<()> {
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
        other => other.context("write summary"),
    }
}

fn render(summary: &Summary, width: u16, color: bool) -> Result<Vec<u8>> {
    let mut bytes = vec![b'\n'];
    bytes.extend(text::lines(&lines::lines(summary, width), color)?);
    Ok(bytes)
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
