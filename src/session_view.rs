//! `agent-walker session <id>`: how one session went, printed for a person, or
//! with `--json` handed to a program as data.

use std::io;

use anyhow::{Result, anyhow};
use time::{OffsetDateTime, UtcOffset};

use crate::analyzer::session_report;
use crate::model::Collection;
use crate::printed;

mod lines;
#[cfg(test)]
mod tests;

#[derive(Debug, clap::Args)]
pub struct SessionArgs {
    /// The session's id, as the agent's own logs name it.
    pub id: String,
    /// Output as JSON: what this command shows, as raw numbers (experimental).
    #[arg(long)]
    pub json: bool,
}

pub(crate) fn run(
    args: &SessionArgs,
    collections: &[Collection],
    now: OffsetDateTime,
    days: u16,
    offset: UtcOffset,
) -> Result<()> {
    let report = session_report(collections, &args.id, now, days, offset)
        .ok_or_else(|| anyhow!("no session {} in the last {days} days", args.id))?;
    if args.json {
        let stdout = io::stdout();
        let mut out = io::BufWriter::new(stdout.lock());
        return crate::format::write_json_session(&mut out, &report, offset);
    }
    let output = printed::output();
    printed::print(
        &lines::lines(&report, usize::from(output.width), offset),
        output.color,
    )
}
