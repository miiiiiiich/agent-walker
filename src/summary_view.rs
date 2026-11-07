//! `agent-walker summary`: the share card's codename and numbers, printed for a
//! person, or with `--json` handed to a program as data.

use std::io;

use anyhow::Result;
use time::UtcOffset;

use crate::model::AppSummary;
use crate::printed;
use crate::share::{ShareCard, card_svg};

mod lines;
#[cfg(test)]
mod tests;

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

    let output = printed::output();
    printed::print(&lines::lines(&report.combined, output.width), output.color)
}
