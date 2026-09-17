//! Chart adapters live in one file per chart (`charts/*.rs`) so a PR's
//! changed-file list names exactly which panel it touches.

use ratatui::prelude::*;

use super::theme;
use super::utils;

mod credits;
mod hourly;
mod limits;
mod model_daily;

pub(super) use credits::credits_chart_lines;
pub(super) use hourly::hourly_chart_lines;
pub(super) use limits::limits_chart_lines;
pub(super) use model_daily::model_chart_lines;

/// Hand-positioned x-axis label row. Each label is centered on an absolute
/// character column, computed by the caller from the same mapping that
/// placed the data — so labels cannot drift from the bars they annotate.
fn axis_label_row(width: u16, points: &[(usize, String)]) -> Line<'static> {
    let total = usize::from(width);
    let mut buffer = vec![' '; total];
    for (center, text) in points {
        let length = text.chars().count();
        if length > total {
            continue;
        }
        let start = center.saturating_sub(length / 2).min(total - length);
        for (index, character) in text.chars().enumerate() {
            buffer[start + index] = character;
        }
    }
    Line::from(Span::styled(
        buffer.into_iter().collect::<String>(),
        Style::default().fg(theme::MUTED),
    ))
}

pub(super) const Y_AXIS_WIDTH: usize = 7;

pub(super) struct ChartColumn {
    pub level: usize,
    pub color: Color,
    pub baseline: &'static str,
}

pub(super) fn column_chart_lines(
    title: &'static str,
    annotation: &str,
    y_labels: &[String; 3],
    columns: &[ChartColumn],
    x_points: &[(usize, String)],
    width: u16,
    body_height: usize,
) -> Vec<Line<'static>> {
    let height = body_height.max(1);
    let mut out = vec![utils::section_title(title, annotation)];
    for row in 0..height {
        let label = if row == 0 {
            format!("{:>6}", y_labels[0])
        } else if row == height / 2 {
            format!("{:>6}", y_labels[1])
        } else if row == height - 1 {
            format!("{:>6}", y_labels[2])
        } else {
            " ".repeat(6)
        };
        let mut spans = vec![
            Span::styled(label, Style::default().fg(theme::MUTED)),
            Span::styled("│", Style::default().fg(theme::DIM)),
        ];
        let half_bottom = 2 * (height - 1 - row);
        let half_top = half_bottom + 1;
        let baseline = row == height - 1;
        for column in columns {
            let glyph = if column.level > half_top {
                "█"
            } else if column.level > half_bottom {
                "▄"
            } else if baseline && column.level == 0 {
                column.baseline
            } else {
                " "
            };
            spans.push(Span::styled(
                glyph.to_owned(),
                Style::default().fg(column.color),
            ));
        }
        out.push(Line::from(spans));
    }
    let points: Vec<(usize, String)> = x_points
        .iter()
        .map(|(index, label)| (Y_AXIS_WIDTH + index, label.clone()))
        .collect();
    out.push(axis_label_row(width, &points));
    out
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Chart geometry is display-only."
)]
pub(super) fn level_for(value: f64, max: f64, body_height: usize) -> usize {
    if value <= 0.0 || max <= 0.0 {
        return 0;
    }
    let half_cells = body_height.max(1) * 2;
    ((value / max) * half_cells as f64).round().max(1.0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_chart_body_width_is_axis_plus_one_char_per_column() {
        let columns: Vec<ChartColumn> = (0..30)
            .map(|index| ChartColumn {
                level: index % 13,
                color: theme::GREEN,
                baseline: "·",
            })
            .collect();
        let lines = column_chart_lines(
            "LIMITS",
            "",
            &["100%".to_owned(), "50%".to_owned(), "0".to_owned()],
            &columns,
            &[],
            120,
            6,
        );
        assert_eq!(lines.len(), 8);
        for body in &lines[1..7] {
            assert_eq!(body.width(), Y_AXIS_WIDTH + columns.len());
        }
    }

    #[test]
    fn baseline_glyphs_render_only_on_the_bottom_row() {
        let columns = vec![
            ChartColumn {
                level: 0,
                color: theme::GREEN,
                baseline: "▁",
            },
            ChartColumn {
                level: 0,
                color: theme::DIM,
                baseline: "·",
            },
            ChartColumn {
                level: 12,
                color: theme::GREEN,
                baseline: "▁",
            },
        ];
        let lines = column_chart_lines(
            "CREDITS",
            "",
            &["1".to_owned(), "0.5".to_owned(), "0".to_owned()],
            &columns,
            &[],
            80,
            6,
        );
        let row = |index: usize| -> String {
            lines[index]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        let bottom = row(6);
        assert!(bottom.ends_with("▁·█"));
        assert!(!row(1).contains('▁') && !row(1).contains('·'));
    }
}
