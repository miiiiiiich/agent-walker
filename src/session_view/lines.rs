use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use time::{OffsetDateTime, UtcOffset};

use crate::analyzer::{LONG_TURN_MS, SessionReport};
use crate::format::{
    format_count, format_duration_ms, format_hours, format_percent, format_tokens,
};
use crate::model::{ActiveTimeSummary, ContextReason, Summary};
use crate::printed::clean;
use crate::share::ShareCard;
use crate::timestamp::to_local;
use crate::ui::{BLUE, DIM, GOLD, MUTED, TEXT, join_columns, month_abbrev, section_title};

const LABEL_WIDTH: usize = 14;
const VALUE_WIDTH: usize = 11;
const DETAIL_SEPARATOR: &str = " · ";
const TABLE_WIDTH: usize = 52;
const COLUMN_GAP: usize = 2;
const GRAPH_WIDTH: usize = 38;
const GRAPH_ROWS: usize = 5;
const AXIS_WIDTH: usize = 6;
const AXIS: &str = " ┤";
const EIGHTHS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// An unknown value renders as "—", never "0" or "$0", as on the card.
const DASH: &str = "—";
const TOP_ITEMS: usize = 3;

pub(super) fn lines(report: &SessionReport, width: usize, offset: UtcOffset) -> Vec<Line<'static>> {
    let mut left = heading(report, width, offset);
    let context = context_row(report);
    if width >= TABLE_WIDTH + COLUMN_GAP + GRAPH_WIDTH {
        left.extend(table(rows(report), TABLE_WIDTH));
        // Under the graph, where the left column is still running: one more row
        // there would push the pane above the prompt past what it shows.
        let mut right = graph(report, GRAPH_WIDTH);
        if !right.is_empty() && context.is_some() {
            right.push(Line::default());
        }
        right.extend(table(context, GRAPH_WIDTH));
        return join_columns(&left, &right, TABLE_WIDTH + COLUMN_GAP);
    }
    left.extend(table(rows(report).into_iter().chain(context), width));
    let graph = graph(report, width);
    if !graph.is_empty() {
        left.push(Line::default());
        left.extend(graph);
    }
    left
}

fn heading(report: &SessionReport, width: usize, offset: UtcOffset) -> Vec<Line<'static>> {
    let short_id: String = report.id.chars().take(8).collect();
    let first = moment(report.first_at, offset);
    let last = moment(report.last_at, offset);
    let span = format!("{first} – {last}");
    let together = format!("{short_id}  {span}");
    if width >= "▍ SESSION  ".chars().count() + together.chars().count() {
        return vec![section_title("SESSION", &together)];
    }
    let mut lines = vec![section_title("SESSION", &short_id)];
    let style = Style::default().fg(DIM);
    if width >= span.chars().count() {
        lines.push(Line::from(Span::styled(span, style)));
    } else {
        lines.push(Line::from(Span::styled(format!("{first} –"), style)));
        lines.push(Line::from(Span::styled(last, style)));
    }
    lines
}

fn moment(at: OffsetDateTime, offset: UtcOffset) -> String {
    let at = to_local(at, offset).unwrap_or(at);
    format!(
        "{} {} {:02}:{:02}",
        month_abbrev(at.month()),
        at.day(),
        at.hour(),
        at.minute()
    )
}

pub(super) struct Row {
    pub(super) label: &'static str,
    value: String,
    details: Vec<String>,
}

fn row(label: &'static str, value: impl Into<String>, details: Vec<String>) -> Row {
    Row {
        label,
        value: value.into(),
        details,
    }
}

pub(super) fn rows(report: &SessionReport) -> Vec<Row> {
    let mut rows = time_rows(&report.summary);
    rows.extend(used_rows(report));
    rows
}

fn time_rows(summary: &Summary) -> Vec<Row> {
    let time = summary.active_time.as_ref();
    let duration = summary.completion_duration.as_ref();
    let card = ShareCard::from_summary(summary);
    let unpriced = summary.models.iter().fold(0_u64, |sum, model| {
        sum.saturating_add(model.estimated.unpriced_volume)
    });
    let mut rows = vec![
        row(
            "Cost",
            card.cost.as_deref().unwrap_or(DASH),
            if unpriced > 0 {
                vec![format!("{} tokens unpriced", format_tokens(unpriced))]
            } else {
                Vec::new()
            },
        ),
        row(
            "Working time",
            time.map_or_else(|| DASH.to_owned(), |t| format_hours(t.active_ms)),
            time.map(made_of).unwrap_or_default(),
        ),
    ];
    if let Some(time) = time.filter(|time| time.human_wait_ms > 0) {
        rows.push(row(
            "On you",
            format_hours(time.human_wait_ms),
            vec!["answering questions".to_owned()],
        ));
    }
    rows.push(row(
        "Turns",
        duration.map_or_else(|| DASH.to_owned(), |d| format_count(d.count)),
        duration
            .map(|d| {
                vec![
                    format!("p50 {}", format_duration_ms(d.p50_ms)),
                    format!("p90 {}", format_duration_ms(d.p90_ms)),
                    format!("max {}", format_duration_ms(d.max_ms)),
                ]
            })
            .unwrap_or_default(),
    ));
    if let Some(((p50, p90), mean)) = time.and_then(|t| t.pace_percentiles().zip(t.pace_mean_ms()))
    {
        rows.push(row(
            "Your pace",
            format!("p50 {}", format_duration_ms(p50)),
            vec![
                format!("p90 {}", format_duration_ms(p90)),
                format!("avg {}", format_duration_ms(mean)),
            ],
        ));
    }
    rows
}

/// When only part of the time was measured, the shares describe that part, as
/// the TUI says: a model-only hour must not read as all of a ten-hour session.
fn made_of(time: &ActiveTimeSummary) -> Vec<String> {
    let Some(share) = time.model_share() else {
        return Vec::new();
    };
    let model = share * 100.0;
    if time.measured_ms < time.active_ms {
        return vec![format!(
            "model {model:.0}% of {}",
            format_hours(time.measured_ms)
        )];
    }
    vec![
        format!("model {model:.0}%"),
        format!("tools {:.0}%", 100.0 - model),
    ]
}

fn calls_total(calls: impl Iterator<Item = usize>) -> String {
    let total = calls.fold(0_usize, usize::saturating_add);
    format!("{} calls", format_count(total))
}

/// Codex attributes usage to a subagent without any call to count: a zero there
/// would say none ran, so such a row names them and leaves the calls out.
fn subagents_row(summary: &Summary) -> Row {
    let agents = &summary.agents;
    let has_calls = agents.iter().any(|agent| agent.calls > 0);
    let value = if has_calls {
        calls_total(agents.iter().map(|agent| agent.calls))
    } else {
        format_count(agents.len())
    };
    let details = agents
        .iter()
        .take(TOP_ITEMS)
        .map(|agent| {
            if agent.calls > 0 {
                format!("{} {}", agent.name, format_count(agent.calls))
            } else {
                agent.name.clone()
            }
        })
        .collect();
    row("Subagents", value, details)
}

fn used_rows(report: &SessionReport) -> Vec<Row> {
    let summary = &report.summary;
    let mut rows = vec![row(
        "Tools",
        calls_total(summary.tools.iter().map(|tool| tool.calls)),
        summary
            .tools
            .iter()
            .take(TOP_ITEMS)
            .map(|tool| format!("{} {}", tool.name, format_count(tool.calls)))
            .collect(),
    )];
    if !summary.agents.is_empty() {
        rows.push(subagents_row(summary));
    }
    if !summary.skills.is_empty() {
        rows.push(row(
            "Skills",
            format_count(summary.skills.len()),
            summary
                .skills
                .iter()
                .take(TOP_ITEMS)
                .map(|skill| skill.name.clone())
                .collect(),
        ));
    }
    rows.push(row(
        "Models",
        format_count(report.models.len()),
        report
            .models
            .iter()
            .take(TOP_ITEMS)
            .map(|model| {
                let share = format_percent(model.tokens, summary.total_usage.token_volume().max(1));
                format!("{} {share}", model.label)
            })
            .collect(),
    ));
    rows
}

fn context_row(report: &SessionReport) -> Option<Row> {
    let context = report.summary.context.as_ref()?;
    let calls = |label: &str, reason: Option<&ContextReason>| {
        reason
            .filter(|reason| reason.calls > 0)
            .map(|reason| format!("{label} {}", format_count(reason.calls)))
    };
    Some(row(
        "Context",
        format!("{:.0}% cached", context.cached_share() * 100.0),
        [
            calls("expired", context.expired.as_ref()),
            calls("cold start", context.cold_start.as_ref()),
        ]
        .into_iter()
        .flatten()
        .collect(),
    ))
}

fn cells(text: &str) -> usize {
    Span::raw(text).width()
}

/// As many details as fit, whole: a row that wraps breaks both columns.
fn fit(details: &[String], room: usize) -> String {
    let mut text = String::new();
    for detail in details {
        // Measured as printed: stripping a joiner from an emoji widens it.
        let detail = clean(detail);
        let separator = if text.is_empty() {
            ""
        } else {
            DETAIL_SEPARATOR
        };
        let used = cells(&text) + cells(separator) + cells(&detail);
        if used > room {
            break;
        }
        text.push_str(separator);
        text.push_str(&detail);
    }
    text
}

fn table(rows: impl IntoIterator<Item = Row>, width: usize) -> Vec<Line<'static>> {
    rows.into_iter()
        .map(|row| {
            // Below 26 columns the label's padding gives way first, so a value as
            // wide as "100% cached" still fits on the row.
            let label_width = LABEL_WIDTH
                .min(width.saturating_sub(cells(&row.value)))
                .max(cells(row.label) + 1);
            let label = Span::styled(
                format!("{:label_width$}", row.label),
                Style::default().fg(MUTED),
            );
            let value_style = Style::default().fg(TEXT).add_modifier(Modifier::BOLD);
            let value_cells = cells(&row.value).max(VALUE_WIDTH);
            let room = width.saturating_sub(label_width + value_cells + 1);
            let detail = fit(&row.details, room);
            if detail.is_empty() {
                return Line::from(vec![label, Span::styled(row.value, value_style)]);
            }
            Line::from(vec![
                label,
                Span::styled(format!("{:VALUE_WIDTH$} ", row.value), value_style),
                Span::styled(detail, Style::default().fg(MUTED)),
            ])
        })
        .collect()
}

fn graph(report: &SessionReport, width: usize) -> Vec<Line<'static>> {
    let Some(duration) = report.summary.completion_duration.as_ref() else {
        return Vec::new();
    };
    let top = report.turn_lengths_ms.iter().copied().max().unwrap_or(0);
    if top == 0 {
        return Vec::new();
    }
    let top_label = format_duration_ms(top);
    let axis_width = cells(&top_label).max(AXIS_WIDTH);
    let body = width.saturating_sub(axis_width + cells(AXIS)).max(1);
    let columns = fit_columns(&report.turn_lengths_ms, body);
    let ran = format!("{} ran ≥20m", format_count(duration.long_turns()));
    let room = width.saturating_sub("▍ TURN LENGTH  ".chars().count());
    let annotation = [
        format!("{} turns · {ran}", format_count(duration.count)),
        ran,
    ]
    .into_iter()
    .find(|text| text.chars().count() <= room)
    .unwrap_or_default();
    let mut lines = vec![section_title("TURN LENGTH", &annotation)];
    for row in 0..GRAPH_ROWS {
        let floor = (GRAPH_ROWS - 1 - row) * 8;
        let label = match row {
            0 => top_label.clone(),
            _ if row == GRAPH_ROWS - 1 => "0".to_owned(),
            _ => String::new(),
        };
        let mut spans = vec![
            Span::styled(format!("{label:>axis_width$}"), Style::default().fg(MUTED)),
            Span::styled(AXIS, Style::default().fg(DIM)),
        ];
        spans.extend(columns.iter().map(|&length| {
            let filled = filled_eighths(length, top);
            let color = if length >= LONG_TURN_MS { GOLD } else { BLUE };
            Span::styled(
                EIGHTHS[filled.saturating_sub(floor).min(8)],
                Style::default().fg(color),
            )
        }));
        lines.push(Line::from(spans));
    }
    lines
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "A bar's height: the ratio is in 0..=1 and rounds to a small count."
)]
fn filled_eighths(length: u64, top: u64) -> usize {
    let eighths = (length as f64 / top as f64 * (GRAPH_ROWS * 8) as f64).round() as usize;
    eighths.max(1)
}

/// One column per turn; past the width each column shows its longest, so the
/// turn the graph is there to show does not vanish between two short ones.
pub(super) fn fit_columns(lengths: &[u64], width: usize) -> Vec<u64> {
    if lengths.len() <= width {
        return lengths.to_vec();
    }
    (0..width)
        .map(|column| {
            let start = column * lengths.len() / width;
            let end = ((column + 1) * lengths.len() / width).max(start + 1);
            lengths[start..end].iter().copied().max().unwrap_or(0)
        })
        .collect()
}
