use std::io::{self, ErrorKind, Write};

use anyhow::{Context, Result};
use serde::Serialize;
use time::{Date, OffsetDateTime, UtcOffset};

use crate::model::{AppSummary, Collection, Provider, TokenUsage};

mod events;
mod histories;
mod panels;
mod summary;
#[cfg(test)]
mod tests;

use events::EventsDto;
use summary::SummaryDto;

time::serde::format_description!(ymd, Date, "[year]-[month]-[day]");

pub(crate) fn write_json(
    writer: &mut impl Write,
    report: &AppSummary,
    collections: &[Collection],
    offset: UtcOffset,
) -> Result<()> {
    let dto = ReportDto::new(report, collections, offset)?;
    let written = serde_json::to_writer_pretty(&mut *writer, &dto)
        .map_err(io::Error::from)
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush());
    match written {
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
        other => other.context("write JSON report"),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ReportDto {
    schema_version: u32,
    #[serde(with = "time::serde::rfc3339")]
    generated_at: OffsetDateTime,
    window: WindowDto,
    time_basis: TimeBasisDto,
    total: SummaryDto,
    providers: Vec<ProviderDto>,
    scan: ScanDto,
}

impl ReportDto {
    fn new(report: &AppSummary, collections: &[Collection], offset: UtcOffset) -> Result<Self> {
        let window = WindowDto {
            days: report.period_days,
            start: report.combined.period_start,
            end: report.combined.period_end,
        };
        let event_window = EventWindow {
            start: window.start,
            end: window.end,
            offset,
        };
        let providers = report
            .providers
            .iter()
            .map(|summary| {
                let collection = collections
                    .iter()
                    .find(|collection| collection.provider == summary.provider)
                    .with_context(|| {
                        format!(
                            "missing collection for {} ({})",
                            summary.provider.label(),
                            summary.root.display(),
                        )
                    })?;
                Ok(ProviderDto {
                    provider: provider_name(summary.provider),
                    summary: SummaryDto::new(summary, offset),
                    events: EventsDto::new(collection, event_window),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let stats = &report.combined.scan_stats;
        Ok(Self {
            schema_version: 1,
            generated_at: report.generated_at,
            window,
            time_basis: TimeBasisDto {
                kind: "fixed_offset",
                offset_seconds: offset.whole_seconds(),
            },
            total: SummaryDto::new(&report.combined, offset),
            providers,
            scan: ScanDto {
                files: stats.files_seen,
                lines: stats.lines_seen,
                parse_errors: stats.parse_errors,
                unreadable_files: stats.unreadable_files,
                load_ms: report.load_duration_ms,
            },
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct WindowDto {
    days: u16,
    #[serde(with = "ymd")]
    start: Date,
    #[serde(with = "ymd")]
    end: Date,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct TimeBasisDto {
    kind: &'static str,
    offset_seconds: i32,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ScanDto {
    files: usize,
    lines: usize,
    parse_errors: usize,
    unreadable_files: usize,
    load_ms: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct ProviderDto {
    provider: &'static str,
    summary: SummaryDto,
    #[serde(flatten)]
    events: EventsDto,
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Combined => "total",
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        Provider::Agy => "agy",
        Provider::OpenCode => "opencode",
        Provider::Copilot => "copilot",
        Provider::Grok => "grok",
        Provider::Cursor => "cursor",
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(
    clippy::struct_field_names,
    reason = "Token units are explicit in the wire contract."
)]
struct TokensDto {
    input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_ephemeral_1h_input_tokens: u64,
    cache_creation_ephemeral_5m_input_tokens: u64,
}

impl From<&TokenUsage> for TokensDto {
    fn from(usage: &TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: usage.reasoning_output_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_ephemeral_1h_input_tokens: usage
                .cache_creation_ephemeral_1h_input_tokens,
            cache_creation_ephemeral_5m_input_tokens: usage
                .cache_creation_ephemeral_5m_input_tokens,
        }
    }
}

#[derive(Clone, Copy)]
struct EventWindow {
    start: Date,
    end: Date,
    offset: UtcOffset,
}

impl EventWindow {
    fn at(self, timestamp: Option<OffsetDateTime>) -> Option<OffsetDateTime> {
        let at = timestamp?.to_offset(self.offset);
        (self.start..=self.end).contains(&at.date()).then_some(at)
    }

    fn rows<T, D>(
        self,
        events: &[T],
        timestamp: impl Fn(&T) -> Option<OffsetDateTime>,
        row: impl Fn(&T, OffsetDateTime) -> D,
    ) -> Vec<D> {
        let mut dated = events
            .iter()
            .filter_map(|event| self.at(timestamp(event)).map(|at| (at, event)))
            .collect::<Vec<_>>();
        dated.sort_by_key(|(at, _)| *at);
        dated
            .into_iter()
            .map(|(at, event)| row(event, at))
            .collect()
    }
}
