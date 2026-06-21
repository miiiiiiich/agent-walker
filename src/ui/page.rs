use ratatui::prelude::*;

use crate::model::Summary;

use super::activity;
use super::badge;
use super::charts;
use super::hero;
use super::sections;
use super::theme;
use super::utils;

/// Two-column layout needs at least this much width; below it sections stack.
/// Kept low — two columns halve the page height, which matters more than
/// generous column widths on small terminals.
const TWO_COLUMN_MIN_WIDTH: u16 = 80;

/// The whole dashboard body as one flowing list of lines. Charts and the
/// two-column section area are rendered into lines (not widgets) so the
/// entire page scrolls as a unit.
pub(super) fn page_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    const CHART_BODY: usize = 6;

    // Two-column split, shared by the codename badge, the charts, and the
    // section grid so every right column (badge, BY HOUR, COST, SIGNAL,
    // SUBAGENTS) lines up on the same boundary.
    let two_column = width >= TWO_COLUMN_MIN_WIDTH;
    let left_width = usize::from(width) * 56 / 100;
    let left_u16 = u16::try_from(left_width).unwrap_or(width);
    let right_u16 = width.saturating_sub(left_u16 + 2);

    let mut lines = vec![
        Line::default(),
        hero::hero_line(summary, width),
        Line::default(),
    ];

    // Codename animal as a braille badge. Side-by-side in the right column next
    // to the ACTIVITY grass (aligned with BY HOUR / COST on the shared boundary)
    // only when the grass actually fits in the left column — otherwise the
    // gutter would collapse and the badge would collide with the grass. Skip the
    // ACTIVITY title/legend row (index 0) since the badge's first row is blank.
    let activity = activity::activity_lines(summary);
    let badge = codename_badge_lines(&crate::codename::for_summary(summary));
    let grass_width = activity.iter().skip(1).map(Line::width).max().unwrap_or(0);
    if badge.is_empty() {
        lines.extend(activity);
    } else if two_column && grass_width <= left_width {
        // Beside the grass: centre the badge within the right column so it sits
        // in the same band as BY HOUR / COST but isn't pinned to the boundary.
        let centred = centre_lines(badge, usize::from(right_u16));
        lines.extend(join_columns(&activity, &centred, left_width + 2));
    } else {
        // Stacked under the grass: centre across the full width so it isn't
        // jammed against the left edge.
        lines.extend(activity);
        lines.push(Line::default());
        lines.extend(centre_lines(badge, usize::from(width)));
    }
    lines.push(Line::default());

    if utils::token_usage_available(summary) && !summary.model_daily.is_empty() {
        if width < TWO_COLUMN_MIN_WIDTH {
            lines.extend(charts::model_chart_lines(summary, width, CHART_BODY));
            lines.push(Line::default());
            lines.extend(charts::hourly_chart_lines(summary, width, CHART_BODY));
        } else {
            // Cap TOKENS PER DAY at the left column so it never overruns MODELS;
            // BY HOUR fills the right column and joins at the shared boundary.
            let model_w = left_u16.min(u16::try_from(7 + summary.daily.len()).unwrap_or(left_u16));
            let left = charts::model_chart_lines(summary, model_w, CHART_BODY);
            let right = charts::hourly_chart_lines(summary, right_u16, CHART_BODY);
            lines.extend(join_columns(&left, &right, left_width + 2));
        }
        lines.push(Line::default());
    }

    // Decided chart priority: activity → token/day → by-hour → completion →
    // PARALLEL AGENTS → models → (cost/signal/projects/tools/agents).
    if width < TWO_COLUMN_MIN_WIDTH {
        if summary.completion_duration.is_some() {
            lines.extend(sections::duration_lines(summary, width));
            lines.push(Line::default());
        }
        let parallel = sections::parallel_lines(summary, width);
        if !parallel.is_empty() {
            lines.extend(parallel);
            lines.push(Line::default());
        }
        lines.extend(sections::model_lines(summary, width));
        lines.push(Line::default());
        lines.extend(sections::cost_lines(summary, width));
        lines.push(Line::default());
        lines.extend(sections::signal_lines(summary, width));
        lines.push(Line::default());
        lines.extend(sections::project_lines(summary, width));
        lines.push(Line::default());
        lines.extend(sections::tool_lines(summary, width, 6));
        if !summary.agents.is_empty() {
            lines.push(Line::default());
            lines.extend(sections::agent_lines(summary, width, 4));
        }
        return lines;
    }

    // completion | PARALLEL AGENTS, side by side ("PARALLEL next to completion"),
    // before the model/cost sections.
    let has_parallel = summary
        .orchestration
        .time_by_level
        .iter()
        .any(|secs| *secs > 0);
    if summary.completion_duration.is_some() || has_parallel {
        let completion = sections::duration_lines(summary, left_u16);
        let parallel = sections::parallel_lines(summary, right_u16);
        lines.extend(join_columns(&completion, &parallel, left_width + 2));
        lines.push(Line::default());
    }

    // Pair sections column-wise so each row of sections starts on the same
    // line in both columns: MODELS|COST, PROJECTS|SIGNAL, TOOLS|SUBAGENTS.
    let left_blocks = [
        sections::model_lines(summary, left_u16),
        sections::project_lines(summary, left_u16),
        sections::tool_lines(summary, left_u16, 10),
    ];
    let mut right_blocks = vec![
        sections::cost_lines(summary, right_u16),
        sections::signal_lines(summary, right_u16),
    ];
    if !summary.agents.is_empty() {
        right_blocks.push(sections::agent_lines(summary, right_u16, 5));
    }

    lines.extend(join_section_columns(
        &left_blocks,
        &right_blocks,
        left_width + 2,
    ));
    lines
}

/// Like `join_columns`, but aligns *section boundaries*: each (left, right)
/// block pair is padded to the taller of the two before the next pair begins,
/// so the second section in each column (PROJECTS / SIGNAL) starts on the same
/// row even when the first sections differ in height.
fn join_section_columns(
    left_blocks: &[Vec<Line<'static>>],
    right_blocks: &[Vec<Line<'static>>],
    right_start: usize,
) -> Vec<Line<'static>> {
    let empty: Vec<Line<'static>> = Vec::new();
    let pairs = left_blocks.len().max(right_blocks.len());
    let mut out = Vec::new();
    for index in 0..pairs {
        if index > 0 {
            out.push(Line::default());
        }
        let left = left_blocks.get(index).unwrap_or(&empty);
        let right = right_blocks.get(index).unwrap_or(&empty);
        out.extend(join_columns(left, right, right_start));
    }
    out
}

/// Zip two column line-lists into full-width lines: the left column is
/// padded to `right_start`, then the right column's spans are appended.
fn join_columns(
    left: &[Line<'static>],
    right: &[Line<'static>],
    right_start: usize,
) -> Vec<Line<'static>> {
    let rows = left.len().max(right.len());
    (0..rows)
        .map(|index| {
            let mut line = left.get(index).cloned().unwrap_or_default();
            if let Some(right_line) = right.get(index) {
                let pad = right_start.saturating_sub(line.width());
                line.spans.push(Span::raw(" ".repeat(pad)));
                line.spans.extend(right_line.spans.iter().cloned());
            }
            line
        })
        .collect()
}

/// Left-pad each non-blank line so the block is centred within `width`.
fn centre_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| {
            let pad = width.saturating_sub(line.width()) / 2;
            if pad == 0 || line.spans.is_empty() {
                return line;
            }
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(Span::raw(" ".repeat(pad)));
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Codename badge: title (OPS in its colour, animal in white) above the braille
/// animal in the OPS colour. Empty for the no-data "Chick" floor.
fn codename_badge_lines(codename: &crate::codename::Codename) -> Vec<Line<'static>> {
    if codename.animal == "Chick" {
        return Vec::new();
    }
    let color = ops_color(codename.ops);
    let icon: Vec<&str> = badge::braille_for(codename.animal)
        .lines()
        .filter(|l| !l.chars().all(|c| c == '⠀' || c == ' '))
        .collect();

    let mut lines = vec![
        Line::default(),
        Line::from(vec![
            Span::styled(
                codename.ops.to_owned(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}", codename.animal),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ];
    for row in icon {
        lines.push(Line::from(Span::styled(
            row.to_owned(),
            Style::default().fg(color),
        )));
    }
    lines
}

fn ops_color(ops: &str) -> Color {
    match ops {
        "Aurora" => theme::TEAL,
        "Sol" => theme::GOLD,
        "Luna" => theme::BLUE,
        "Eclipse" => theme::PURPLE,
        _ => theme::MUTED,
    }
}
