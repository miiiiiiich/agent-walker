use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::codename::{Codename, for_summary};
use crate::format::format_count;
use crate::model::Summary;
use crate::share::ShareCard;
use crate::ui::{DIM, MUTED, TEXT, codename_badge_lines, join_columns};

const LABEL_WIDTH: usize = 14;
const VALUE_WIDTH: usize = 11;
const NARROW_INDENT: usize = 2;
const DETAIL_SEPARATOR: &str = " · ";

const COLUMN_GAP: usize = 4;

/// An unknown value renders as "—", never "0" or "$0", as on the card.
const DASH: &str = "—";

pub(super) fn lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    arrange(
        &for_summary(summary),
        &ShareCard::from_summary(summary),
        usize::from(width),
    )
}

pub(super) fn arrange(codename: &Codename, card: &ShareCard, width: usize) -> Vec<Line<'static>> {
    let mut badge: Vec<Line<'static>> = codename_badge_lines(codename)
        .into_iter()
        .skip_while(|line| line.width() == 0)
        .collect();

    // Beside the badge only while every number keeps its one row: squeezed into
    // the second column they would take more rows than stacking does.
    let right_start = badge.iter().map(Line::width).max().unwrap_or(0) + COLUMN_GAP;
    let beside = width.saturating_sub(right_start);
    if matches!(layout_of(card, beside), Layout::Inline) {
        return join_columns(&badge, &card_lines(card, beside), right_start);
    }
    badge.push(Line::default());
    badge.extend(card_lines(card, width));
    badge
}

struct Row {
    label: &'static str,
    value: String,
    detail: String,
}

impl Row {
    fn new(label: &'static str, value: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label,
            value: value.into(),
            detail: detail.into(),
        }
    }
}

fn layout_of(card: &ShareCard, width: usize) -> Layout {
    let totals = totals(card);
    let activity = activity(card);
    Layout::for_rows(totals.iter().chain(&activity), width)
}

pub(super) fn card_lines(card: &ShareCard, width: usize) -> Vec<Line<'static>> {
    let layout = layout_of(card, width);
    let totals = totals(card);
    let activity = activity(card);

    let mut lines = Vec::new();
    for row in totals {
        push_row(&mut lines, row, layout);
    }
    lines.push(Line::default());
    for row in activity {
        push_row(&mut lines, row, layout);
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!("{} – {}", card.period.0, card.period.1),
        Style::default().fg(DIM),
    )));
    lines
}

/// One layout for the whole card: rows that each pick their own read as
/// misaligned once only the longest detail moves to its own line.
#[derive(Clone, Copy)]
enum Layout {
    Inline,
    Stacked { indent: usize },
}

impl Layout {
    fn for_rows<'a>(rows: impl Iterator<Item = &'a Row> + Clone, width: usize) -> Self {
        let inline = rows.clone().all(|row| {
            LABEL_WIDTH
                + row.value.chars().count().max(VALUE_WIDTH)
                + 1
                + row.detail.chars().count()
                <= width
        });
        if inline {
            return Self::Inline;
        }
        let under_value = rows
            .into_iter()
            .all(|row| LABEL_WIDTH + row.detail.chars().count() <= width);
        Self::Stacked {
            indent: if under_value {
                LABEL_WIDTH
            } else {
                NARROW_INDENT
            },
        }
    }
}

fn totals(card: &ShareCard) -> [Row; 3] {
    let per_day = |value: &str| format!("{value}/day");
    let (working, working_per_day) = match &card.working {
        Some((total, rate)) => (total.clone(), per_day(rate)),
        None => (DASH.to_owned(), String::new()),
    };
    [
        Row::new("Tokens", card.tokens.clone(), per_day(&card.tokens_per_day)),
        Row::new("Working time", working, working_per_day),
        Row::new(
            "Cost",
            card.cost.as_deref().unwrap_or(DASH),
            card.cost_per_day
                .as_deref()
                .map(per_day)
                .unwrap_or_default(),
        ),
    ]
}

fn activity(card: &ShareCard) -> [Row; 4] {
    let parallel = if card.avg_concurrency > 0.0 || card.parallel.is_some() {
        format!("avg {:.1}", card.avg_concurrency)
    } else {
        DASH.to_owned()
    };
    let parallel_detail = card
        .parallel
        .map(|(four_plus, peak)| {
            format!(
                "{four_plus}% at 4+{DETAIL_SEPARATOR}peak {}",
                format_count(peak)
            )
        })
        .unwrap_or_default();
    let (turns, turns_detail) = match &card.completion {
        Some((_, _, count, p50, p90, _)) => (
            format_count(*count),
            format!("p50 {p50}{DETAIL_SEPARATOR}p90 {p90}"),
        ),
        None => (DASH.to_owned(), String::new()),
    };
    [
        Row::new(
            "Sessions",
            format_count(card.sessions),
            format!("{}/{} active", card.active_days, card.period_days),
        ),
        Row::new("Parallel", parallel, parallel_detail),
        Row::new("Turns", turns, turns_detail),
        Row::new(
            "Context",
            card.tokens_per_min
                .as_deref()
                .map_or_else(|| DASH.to_owned(), |rate| format!("{rate}/min")),
            card.cached
                .as_deref()
                .map(|cached| format!("{cached} cached"))
                .unwrap_or_default(),
        ),
    ]
}

fn push_row(lines: &mut Vec<Line<'static>>, row: Row, layout: Layout) {
    let label = Span::styled(
        format!("{:LABEL_WIDTH$}", row.label),
        Style::default().fg(MUTED),
    );
    let value_style = Style::default().fg(TEXT).add_modifier(Modifier::BOLD);
    let detail_style = Style::default().fg(MUTED);

    match layout {
        Layout::Inline => lines.push(Line::from(vec![
            label,
            Span::styled(format!("{:VALUE_WIDTH$} ", row.value), value_style),
            Span::styled(row.detail, detail_style),
        ])),
        Layout::Stacked { indent } => {
            lines.push(Line::from(vec![
                label,
                Span::styled(row.value, value_style),
            ]));
            // At the narrowest even the indented detail can overflow, and a
            // terminal wraps that to column 0, so each part gets its own line.
            let parts: Vec<&str> = if indent == NARROW_INDENT {
                row.detail.split(DETAIL_SEPARATOR).collect()
            } else {
                vec![row.detail.as_str()]
            };
            for part in parts.into_iter().filter(|part| !part.is_empty()) {
                lines.push(Line::from(Span::styled(
                    format!("{:indent$}{part}", ""),
                    detail_style,
                )));
            }
        }
    }
}
