use std::cell::Cell;
use std::collections::BTreeMap;
use std::io;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph};
use time::{Date, Duration, Weekday};

use crate::app::{Config, load_report};
use crate::cost::usage_cost_usd;
use crate::format::{
    format_count, format_date, format_duration_ms, format_duration_secs, format_percent,
    format_tokens, format_usd, short_model_name,
};
use crate::model::{AppSummary, Provider, Summary};

const ACCENT: Color = Color::Rgb(226, 178, 92);
const HOT: Color = Color::Rgb(219, 105, 84);
const GOLD: Color = Color::Rgb(239, 199, 104);
const GREEN: Color = Color::Rgb(104, 211, 145);
const BLUE: Color = Color::Rgb(132, 167, 255);
const PURPLE: Color = Color::Rgb(186, 148, 255);
const TEAL: Color = Color::Rgb(99, 214, 210);
const MUTED: Color = Color::Rgb(140, 145, 150);
const DIM: Color = Color::Rgb(70, 75, 80);
const FAINT: Color = Color::Rgb(45, 49, 52);
const TEXT: Color = Color::Rgb(238, 237, 230);
const BLACK: Color = Color::Rgb(12, 12, 12);

// GitHub dark-theme contribution-graph greens, plus the empty-cell shade.
const HEAT_RAMP: [Color; 4] = [
    Color::Rgb(14, 68, 41),
    Color::Rgb(0, 109, 50),
    Color::Rgb(38, 166, 65),
    Color::Rgb(57, 211, 83),
];
const HEAT_ZERO: Color = Color::Rgb(33, 38, 45);

/// Two-column layout needs at least this much width; below it sections stack.
/// Kept low — two columns halve the page height, which matters more than
/// generous column widths on small terminals.
const TWO_COLUMN_MIN_WIDTH: u16 = 80;

/// Render one provider tab to plain text at the given size. Used by the
/// hidden `--render` flag and snapshot tests; keeps the TUI inspectable
/// without a terminal session.
pub fn render_text(config: &Config, width: u16, height: u16, tab_index: usize) -> Result<String> {
    let report = load_report(config)?;
    let state = UiState {
        config: config.clone(),
        tab_index: tab_index.min(report.providers.len()),
        report,
        status: String::new(),
        scroll: 0,
        max_scroll: Cell::new(0),
    };
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).context("create test terminal backend")?;
    terminal
        .draw(|frame| draw(frame, &state))
        .context("draw Agent Walker UI to test backend")?;

    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        let mut row = String::new();
        for x in 0..buffer.area.width {
            row.push_str(buffer[(x, y)].symbol());
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }
    Ok(out)
}

pub fn run(config: Config) -> Result<()> {
    let report = load_report(&config)?;
    // First-run guidance: an empty dashboard should explain itself.
    let status = if report.combined.scan_stats.files_seen == 0 {
        "no agent logs found · point me at them with --claude-dir / --codex-dir / --agy-dir"
            .to_owned()
    } else {
        String::new()
    };
    let mut state = UiState {
        config,
        report,
        tab_index: 0,
        status,
        scroll: 0,
        max_scroll: Cell::new(0),
    };

    let mut terminal = setup_terminal()?;
    let run_result = run_loop(&mut terminal, &mut state);
    let restore_result = restore_terminal(&mut terminal);
    run_result.and(restore_result)
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode().context("enable terminal raw mode")?;
    let mut stdout = io::stdout();
    let setup_result = execute!(stdout, EnterAlternateScreen, Hide)
        .context("enter alternate terminal screen")
        .and_then(|()| {
            let backend = CrosstermBackend::new(stdout);
            Terminal::new(backend).context("create terminal backend")
        });
    if setup_result.is_err() {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
    setup_result
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let raw_result = disable_raw_mode().context("disable terminal raw mode");
    let screen_result = execute!(terminal.backend_mut(), LeaveAlternateScreen, Show)
        .context("leave alternate terminal screen");
    let cursor_result = terminal.show_cursor().context("restore terminal cursor");

    raw_result.and(screen_result).and(cursor_result)
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut UiState,
) -> Result<()> {
    loop {
        terminal
            .draw(|frame| draw(frame, state))
            .context("draw Agent Walker UI")?;
        if !event::poll(StdDuration::from_millis(250)).context("poll terminal events")? {
            continue;
        }

        let Event::Key(key) = event::read().context("read terminal event")? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(());
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => state.next_tab(),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => state.previous_tab(),
            KeyCode::Down | KeyCode::Char('j') => {
                state.scroll = state.scroll.saturating_add(1).min(state.max_scroll.get());
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.scroll = state.scroll.saturating_sub(1);
            }
            KeyCode::PageDown => {
                state.scroll = state.scroll.saturating_add(8).min(state.max_scroll.get());
            }
            KeyCode::PageUp => {
                state.scroll = state.scroll.saturating_sub(8);
            }
            KeyCode::Char(digit @ '1'..='9') => {
                let index = digit as usize - '1' as usize;
                if index < state.tab_count() {
                    state.tab_index = index;
                    state.scroll = 0;
                }
            }
            KeyCode::Char('r') => match load_report(&state.config) {
                Ok(report) => {
                    state.report = report;
                    state.tab_index = state.tab_index.min(state.tab_count() - 1);
                    state.status = String::new();
                }
                Err(error) => {
                    state.status = format!("reload failed: {error:#}");
                }
            },
            _ => {}
        }
    }
}

fn draw(frame: &mut Frame<'_>, state: &UiState) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().fg(TEXT).bg(BLACK)),
        area,
    );
    let padded = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    let summary = state.current_summary();
    let width = padded.width;
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .split(padded);

    // Only the tab bar and footer stay fixed; everything from the hero line
    // down lives in one scrollable page.
    frame.render_widget(Paragraph::new(header_line(state, width)), rows[0]);
    let lines = page_lines(summary, width);
    let scroll = clamp_scroll(state, lines.len(), rows[1].height);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), rows[1]);
    draw_footer(frame, rows[2], state, summary);
}

/// The whole dashboard body as one flowing list of lines. Charts and the
/// two-column section area are rendered into lines (not widgets) so the
/// entire page scrolls as a unit.
fn page_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    const CHART_BODY: usize = 6;
    let mut lines = vec![Line::default(), hero_line(summary, width), Line::default()];
    lines.extend(activity_lines(summary));
    lines.push(Line::default());

    if token_usage_available(summary) && !summary.model_daily.is_empty() {
        if width < TWO_COLUMN_MIN_WIDTH {
            lines.extend(model_chart_lines(summary, width, CHART_BODY));
            lines.push(Line::default());
            lines.extend(hourly_chart_lines(summary, width, CHART_BODY));
        } else {
            let desired = u16::try_from(7 + summary.daily.len()).unwrap_or(u16::MAX);
            let model_width = desired.min(width.saturating_sub(36)).max(40);
            let left = model_chart_lines(summary, model_width, CHART_BODY);
            let right =
                hourly_chart_lines(summary, width.saturating_sub(model_width + 2), CHART_BODY);
            lines.extend(join_columns(&left, &right, usize::from(model_width) + 2));
        }
        lines.push(Line::default());
    }

    if width < TWO_COLUMN_MIN_WIDTH {
        lines.extend(model_lines(summary, width));
        lines.push(Line::default());
        lines.extend(cost_lines(summary, width));
        lines.push(Line::default());
        lines.extend(signal_lines(summary, width));
        lines.push(Line::default());
        lines.extend(project_lines(summary, width));
        lines.push(Line::default());
        lines.extend(tool_lines(summary, width, 6));
        if !summary.agents.is_empty() {
            lines.push(Line::default());
            lines.extend(agent_lines(summary, width, 4));
        }
        if summary.completion_duration.is_some() {
            lines.push(Line::default());
            lines.extend(duration_lines(summary, width));
        }
        return lines;
    }

    let left_width = usize::from(width) * 56 / 100;
    let left_u16 = u16::try_from(left_width).unwrap_or(width);
    let right_u16 = width.saturating_sub(left_u16 + 2);

    let mut left = Vec::new();
    left.extend(model_lines(summary, left_u16));
    left.push(Line::default());
    left.extend(project_lines(summary, left_u16));
    left.push(Line::default());
    left.extend(tool_lines(summary, left_u16, 10));

    let mut right = Vec::new();
    right.extend(cost_lines(summary, right_u16));
    right.push(Line::default());
    right.extend(signal_lines(summary, right_u16));
    if !summary.agents.is_empty() {
        right.push(Line::default());
        right.extend(agent_lines(summary, right_u16, 5));
    }
    if summary.completion_duration.is_some() {
        right.push(Line::default());
        right.extend(duration_lines(summary, right_u16));
    }

    lines.extend(join_columns(&left, &right, left_width + 2));
    lines
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
        Style::default().fg(MUTED),
    ))
}

/// Tokens by hour of day as hand-rendered bars with a labelled y-axis; the
/// peak hour glows gold.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Chart geometry is display-only."
)]
fn hourly_chart_lines(summary: &Summary, width: u16, body_height: usize) -> Vec<Line<'static>> {
    let max = summary.hourly_usage.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return Vec::new();
    }
    let peak_hour = summary.busiest_hour.map(|(hour, _)| usize::from(hour));
    let graph_available = usize::from(width).saturating_sub(7);
    let chars_per_bar = if graph_available >= 48 { 2 } else { 1 };
    let height = body_height.max(1);
    let half_cells = height * 2;

    let annotation = summary
        .busiest_hour
        .map_or_else(String::new, |(hour, usage)| {
            if width < 40 {
                format!("peak {hour:02}:00")
            } else {
                format!("peak {hour:02}:00 · {}", format_tokens(usage))
            }
        });
    let mut out = vec![section_title("BY HOUR", &annotation)];

    let levels: Vec<usize> = summary
        .hourly_usage
        .iter()
        .map(|value| {
            if *value == 0 {
                0
            } else {
                ((*value as f64 / max as f64) * half_cells as f64)
                    .round()
                    .max(1.0) as usize
            }
        })
        .collect();

    for row in 0..height {
        let label = if row == 0 {
            format!("{:>6}", format_tokens(max))
        } else if row == height / 2 {
            format!(
                "{:>6}",
                format_tokens((max as f64 * (height - height / 2) as f64 / height as f64) as u64)
            )
        } else if row == height - 1 {
            format!("{:>6}", 0)
        } else {
            " ".repeat(6)
        };
        let mut spans = vec![
            Span::styled(label, Style::default().fg(MUTED)),
            Span::styled("│", Style::default().fg(DIM)),
        ];
        let half_bottom = 2 * (height - 1 - row);
        let half_top = half_bottom + 1;
        for (hour, level) in levels.iter().enumerate() {
            let glyph = if *level > half_top {
                "█"
            } else if *level > half_bottom {
                "▄"
            } else {
                " "
            };
            let color = if peak_hour == Some(hour) { GOLD } else { BLUE };
            spans.push(Span::styled(
                glyph.repeat(chars_per_bar),
                Style::default().fg(color),
            ));
        }
        out.push(Line::from(spans));
    }

    let points: Vec<(usize, String)> = (0..=6)
        .map(|step| {
            let hour = step * 4;
            let column = (hour * chars_per_bar).min(24 * chars_per_bar - 1);
            (7 + column, format!("{hour:02}"))
        })
        .collect();
    out.push(axis_label_row(width, &points));
    out
}

/// Daily volume as stacked per-model bars, rendered by hand: one column per
/// day (or per day-bucket on narrow terminals), each half-cell colored by
/// the segment that owns it. The Chart widget painter-stacking left rounding
/// artifacts (floating caps, bleeding columns); exact half-cell assignment
/// cannot.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Chart geometry is display-only."
)]
#[allow(
    clippy::too_many_lines,
    reason = "Flat renderer: bucketing, scaling, and cell painting in one pass."
)]
fn model_chart_lines(summary: &Summary, width: u16, body_height: usize) -> Vec<Line<'static>> {
    const Y_WIDTH: usize = 7; // 6-char label column + axis bar
    let day_count = summary.daily.len();
    let graph_width = usize::from(width).saturating_sub(Y_WIDTH).max(1);
    let height = body_height.max(1);
    if day_count == 0 {
        return Vec::new();
    }
    let chunk = day_count.div_ceil(graph_width);
    let columns = day_count.div_ceil(chunk);

    let mut out = vec![section_title(
        "TOKENS PER DAY",
        if chunk > 1 {
            "stacked by model · bucket avg"
        } else {
            "stacked by model"
        },
    )];

    // Per-segment per-column mean volume: top models, then the remainder.
    let top_models: Vec<_> = summary
        .models
        .iter()
        .filter(|model| model.usage.token_volume() > 0)
        .take(6)
        .collect();
    let bucket_mean = |values: &[u64]| -> Vec<f64> {
        (0..columns)
            .map(|column| {
                let slice = &values[column * chunk..((column + 1) * chunk).min(values.len())];
                if slice.is_empty() {
                    0.0
                } else {
                    slice.iter().sum::<u64>() as f64 / slice.len() as f64
                }
            })
            .collect()
    };
    let daily_totals: Vec<u64> = summary
        .daily
        .iter()
        .map(|stat| stat.usage.token_volume())
        .collect();
    let totals = bucket_mean(&daily_totals);
    let mut segments: Vec<(Color, Vec<f64>)> = top_models
        .iter()
        .enumerate()
        .map(|(index, model)| {
            (
                model_color(index),
                bucket_mean(&model_daily_values(summary, &model.name)),
            )
        })
        .collect();
    let known: Vec<f64> = (0..columns)
        .map(|column| segments.iter().map(|(_, values)| values[column]).sum())
        .collect();
    if totals
        .iter()
        .zip(&known)
        .any(|(total, accounted)| *total > *accounted + 0.5)
    {
        segments.push((
            DIM,
            totals
                .iter()
                .zip(&known)
                .map(|(total, accounted)| (total - accounted).max(0.0))
                .collect(),
        ));
    }

    let max_total = totals.iter().copied().fold(0.0_f64, f64::max).max(1.0);
    let half_cells = height * 2;
    let to_level = |value: f64| -> usize {
        let level = (value / max_total * half_cells as f64).round() as usize;
        if value > 0.0 {
            level.max(1).min(half_cells)
        } else {
            0
        }
    };

    // Cumulative segment boundaries per column, in half-cell units.
    let boundaries: Vec<Vec<usize>> = (0..columns)
        .map(|column| {
            let mut running = 0.0;
            segments
                .iter()
                .map(|(_, values)| {
                    running += values[column];
                    to_level(running)
                })
                .collect()
        })
        .collect();
    let color_at = |column: usize, half_index: usize| -> Option<Color> {
        boundaries[column]
            .iter()
            .position(|boundary| half_index < *boundary)
            .map(|segment| segments[segment].0)
    };

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(height);
    for row in 0..height {
        let label = if row == 0 {
            format!("{:>6}", format_tokens(max_total as u64))
        } else if row == height / 2 {
            format!(
                "{:>6}",
                format_tokens((max_total * (height - height / 2) as f64 / height as f64) as u64)
            )
        } else if row == height - 1 {
            format!("{:>6}", 0)
        } else {
            " ".repeat(6)
        };
        let mut spans = vec![
            Span::styled(label, Style::default().fg(MUTED)),
            Span::styled("│", Style::default().fg(DIM)),
        ];
        let half_top = half_cells - 1 - 2 * row;
        let half_bottom = half_cells - 2 - 2 * row;
        for column in 0..columns {
            let top = color_at(column, half_top);
            let bottom = color_at(column, half_bottom);
            spans.push(match (top, bottom) {
                (None, None) => Span::raw(" "),
                (Some(color_top), Some(color_bottom)) if color_top == color_bottom => {
                    Span::styled("█", Style::default().fg(color_top))
                }
                (Some(color_top), Some(color_bottom)) => {
                    Span::styled("▀", Style::default().fg(color_top).bg(color_bottom))
                }
                (None, Some(color_bottom)) => Span::styled("▄", Style::default().fg(color_bottom)),
                (Some(color_top), None) => Span::styled("▀", Style::default().fg(color_top)),
            });
        }
        lines.push(Line::from(spans));
    }
    out.extend(lines);

    let points: Vec<(usize, String)> = [0.0, 0.25, 0.5, 0.75, 1.0]
        .iter()
        .filter_map(|fraction| {
            let index = ((day_count.saturating_sub(1)) as f64 * fraction).round() as usize;
            let day = summary.daily.get(index)?;
            // Center the label on the exact column that draws this day.
            Some((
                Y_WIDTH + index / chunk,
                format!("{} {}", month_abbrev(day.date.month()), day.date.day()),
            ))
        })
        .collect();
    out.push(axis_label_row(width, &points));
    out
}

fn model_daily_values(summary: &Summary, model_name: &str) -> Vec<u64> {
    let usage_by_date = summary
        .model_daily
        .iter()
        .filter(|day| day.model == model_name)
        .map(|day| (day.date, day.usage.token_volume()))
        .collect::<BTreeMap<_, _>>();
    summary
        .daily
        .iter()
        .map(|day| usage_by_date.get(&day.date).copied().unwrap_or(0))
        .collect()
}

fn header_line(state: &UiState, width: u16) -> Line<'static> {
    let mut spans = vec![
        Span::styled("▌ ", Style::default().fg(GOLD)),
        Span::styled(
            "Agent Walker",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
    ];
    // Tabs always fit before decoration: title 14 + tabs ≈ 4 labels × ~12.
    if width >= 84 {
        spans.push(Span::styled(
            format!("  last {} days", state.report.period_days),
            Style::default().fg(MUTED),
        ));
        spans.push(Span::raw("  "));
    }

    // Provider tabs: provider-colored bar + underlined name marks the selection.
    for (index, (label, color)) in state.tabs().into_iter().enumerate() {
        spans.push(Span::raw("   "));
        if index == state.tab_index {
            spans.push(Span::styled("▍", Style::default().fg(color)));
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(TEXT)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
        } else {
            spans.push(Span::styled("▍", Style::default().fg(FAINT)));
            spans.push(Span::styled(label, Style::default().fg(MUTED)));
        }
    }
    Line::from(spans)
}

/// Progressively shed decoration, then secondary metrics, until the line fits.
fn hero_line(summary: &Summary, width: u16) -> Line<'static> {
    let width = usize::from(width);
    let full = build_hero(summary, "   ·   ", false, false);
    if full.width() <= width {
        return full;
    }
    let compact = build_hero(summary, " · ", true, false);
    if compact.width() <= width {
        return compact;
    }
    build_hero(summary, " · ", true, true)
}

fn build_hero(
    summary: &Summary,
    separator: &'static str,
    compact: bool,
    essentials_only: bool,
) -> Line<'static> {
    let cache_pressure = if summary.total_usage.prompt_tokens() == 0 {
        None
    } else {
        Some(format_percent(
            summary.total_usage.cache_read_input_tokens,
            summary.total_usage.prompt_tokens(),
        ))
    };

    let mut spans = Vec::new();
    if summary.total_usage.token_volume() > 0 {
        push_hero(
            &mut spans,
            format_tokens(summary.total_usage.token_volume()),
            if compact { "tok" } else { "tokens" },
            GOLD,
            separator,
        );
        if !compact
            && let Some(span) = delta_span(
                summary.total_usage.token_volume(),
                summary.previous_total_volume,
            )
        {
            spans.push(span);
        }
        if let Some(cache) = cache_pressure
            && !essentials_only
        {
            push_hero(&mut spans, cache, "cache", TEXT, separator);
        }
    }
    push_hero(
        &mut spans,
        format_count(summary.sessions),
        if compact { "sess" } else { "sessions" },
        TEXT,
        separator,
    );
    push_hero(
        &mut spans,
        format!("{}/{}", summary.active_days, summary.period_days),
        if compact { "days" } else { "days active" },
        TEXT,
        separator,
    );
    if summary.current_streak_days > 0 && !essentials_only {
        push_hero(
            &mut spans,
            format!("{}d", summary.current_streak_days),
            "streak",
            TEXT,
            separator,
        );
    }
    Line::from(spans)
}

/// Period-over-period delta badge ("↑12%"). None when there is no previous
/// data to compare against.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "Display-only percentage."
)]
fn delta_span(current: u64, previous: u64) -> Option<Span<'static>> {
    if previous == 0 {
        return None;
    }
    let percent = ((current as f64 - previous as f64) / previous as f64 * 100.0).round() as i64;
    if percent == 0 {
        return None;
    }
    let (arrow, color) = if percent > 0 {
        ("↑", GREEN)
    } else {
        ("↓", HOT)
    };
    Some(Span::styled(
        format!(" {arrow}{}%", percent.abs()),
        Style::default().fg(color),
    ))
}

fn push_hero(
    spans: &mut Vec<Span<'static>>,
    value: String,
    label: &'static str,
    color: Color,
    separator: &'static str,
) {
    if !spans.is_empty() {
        spans.push(Span::styled(separator, Style::default().fg(FAINT)));
    }
    spans.push(Span::styled(
        value,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!(" {label}"),
        Style::default().fg(MUTED),
    ));
}

fn activity_lines(summary: &Summary) -> Vec<Line<'static>> {
    let mut title = section_title(
        "ACTIVITY",
        &format!(
            "{} – {}",
            format_date(summary.period_start),
            format_date(summary.period_end)
        ),
    );
    title
        .spans
        .push(Span::styled("   less ", Style::default().fg(DIM)));
    title
        .spans
        .push(Span::styled("▄", Style::default().fg(HEAT_ZERO)));
    for color in HEAT_RAMP {
        title.spans.push(Span::raw(" "));
        title
            .spans
            .push(Span::styled("▄", Style::default().fg(color)));
    }
    title
        .spans
        .push(Span::styled(" more", Style::default().fg(DIM)));
    let mut lines = vec![title];

    if !token_usage_available(summary) && summary.scan_stats.lines_seen > 0 {
        lines.push(Line::from(Span::styled(
            "No token-volume heatmap for this provider — activity below uses session touches only.",
            Style::default().fg(MUTED),
        )));
        lines.extend(session_heatmap(summary));
        return lines;
    }

    lines.extend(usage_heatmap(summary));
    lines
}

/// GitHub-style weekly grid driven by token volume.
fn usage_heatmap(summary: &Summary) -> Vec<Line<'static>> {
    let usage_by_date = summary
        .daily
        .iter()
        .map(|day| (day.date, day.usage.token_volume()))
        .collect::<BTreeMap<_, _>>();
    heatmap_grid(summary, &usage_by_date)
}

/// Fallback heatmap from per-day session counts (providers without usage numbers).
fn session_heatmap(summary: &Summary) -> Vec<Line<'static>> {
    let sessions_by_date = summary
        .daily_sessions
        .iter()
        .map(|day| (day.date, u64::try_from(day.sessions).unwrap_or(u64::MAX)))
        .collect::<BTreeMap<_, _>>();
    heatmap_grid(summary, &sessions_by_date)
}

/// Grass grid with guaranteed square cells: "▄" is 1 char wide x 1/2 line
/// tall = 1:1 on a ~1:2 terminal cell. The one-char horizontal gutter and
/// the empty upper half-line are the smallest gaps the character lattice
/// allows without giving up squareness.
fn heatmap_grid(summary: &Summary, value_by_date: &BTreeMap<Date, u64>) -> Vec<Line<'static>> {
    const CELL_PITCH: usize = 2; // 1-char cell + 1-char gap
    let thresholds = heat_thresholds(value_by_date);
    let start =
        summary.period_start - Duration::days(i64::from(weekday_index(summary.period_start)));
    let weeks = ((summary.period_end - start).whole_days() / 7 + 1).max(1);

    let mut lines = Vec::new();

    // Month markers aligned to week columns.
    let mut months = " ".repeat(5);
    let mut last_month = None;
    for week in 0..weeks {
        let month = (start + Duration::days(week * 7)).month();
        if last_month.is_none_or(|last| last != month) {
            last_month = Some(month);
            let position = 5 + usize::try_from(week).unwrap_or(0) * CELL_PITCH;
            if position >= months.chars().count() {
                while months.chars().count() < position {
                    months.push(' ');
                }
                months.push_str(month_abbrev(month));
                months.push(' ');
            }
        }
    }
    lines.push(Line::from(Span::styled(months, Style::default().fg(MUTED))));

    for weekday in 0..7 {
        let label = match weekday {
            0 => "Mon",
            2 => "Wed",
            4 => "Fri",
            6 => "Sun",
            _ => "",
        };
        let mut spans = vec![Span::styled(
            format!("{label:<5}"),
            Style::default().fg(DIM),
        )];
        for week in 0..weeks {
            let date = start + Duration::days(week * 7 + weekday);
            if date < summary.period_start || date > summary.period_end {
                spans.push(Span::raw("  "));
                continue;
            }
            let value = value_by_date.get(&date).copied().unwrap_or(0);
            spans.push(Span::styled(
                "▄",
                Style::default().fg(heat_color(value, &thresholds)),
            ));
            spans.push(Span::raw(" "));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Quartile thresholds over the non-zero days. Quantile bucketing keeps the
/// four greens evenly used even when one outlier day dwarfs the rest —
/// linear max-scaling collapsed everything else into the darkest shade.
fn heat_thresholds(value_by_date: &BTreeMap<Date, u64>) -> Vec<u64> {
    let mut values = value_by_date
        .values()
        .copied()
        .filter(|value| *value > 0)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return Vec::new();
    }
    values.sort_unstable();
    [25, 50, 75]
        .iter()
        .map(|quantile| values[(values.len() - 1) * quantile / 100])
        .collect()
}

fn heat_color(value: u64, thresholds: &[u64]) -> Color {
    if value == 0 || thresholds.is_empty() {
        return HEAT_ZERO;
    }
    let bucket = thresholds
        .iter()
        .filter(|threshold| value > **threshold)
        .count();
    HEAT_RAMP[bucket.min(HEAT_RAMP.len() - 1)]
}

/// Record how far the sections can scroll and clamp the current offset.
fn clamp_scroll(state: &UiState, content_lines: usize, viewport_height: u16) -> u16 {
    let max_scroll = u16::try_from(content_lines)
        .unwrap_or(u16::MAX)
        .saturating_sub(viewport_height);
    state.max_scroll.set(max_scroll);
    state.scroll.min(max_scroll)
}

/// API-equivalent spend, cache-aware, with the JPY conversion that answers
/// "is the subscription paying for itself". Shows trailing windows (today /
/// 7d / 30d) cut from the per-day, per-model aggregates.
fn cost_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let label_width = kv_label_width(width);
    let annotation = if width < 44 {
        "api-equivalent".to_owned()
    } else {
        crate::cost::pricing_as_of().map_or_else(
            || "api-equivalent · cache-aware".to_owned(),
            |date| format!("api-equivalent · rates {date}"),
        )
    };
    let mut total = 0.0_f64;
    let mut per_model: Vec<(String, f64)> = Vec::new();
    for model in &summary.models {
        if let Some(cost) = usage_cost_usd(&model.name, &model.usage) {
            total += cost;
            per_model.push((short_model_name(&model.name), cost));
        }
    }
    if total < 0.01 {
        return Vec::new();
    }
    per_model.sort_by(|left, right| right.1.total_cmp(&left.1));

    let mut lines = vec![section_title("COST", &annotation)];
    for (label, window_days) in [("Today", 1_u16), ("7 days", 7), ("30 days", 30)] {
        if window_days >= summary.period_days {
            break;
        }
        lines.push(cost_row(
            label,
            window_cost_usd(summary, window_days),
            false,
            label_width,
        ));
    }
    lines.push(cost_row(
        &format!("{} days", summary.period_days),
        total,
        true,
        label_width,
    ));
    for (name, cost) in per_model.iter().take(3) {
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{:<label_width$}",
                    compact_label(name, label_width.saturating_sub(1))
                ),
                Style::default().fg(MUTED),
            ),
            Span::styled(format_usd(*cost), Style::default().fg(TEXT)),
        ]));
    }
    lines
}

/// Key column width for kv-style rows, shrinking on narrow columns.
fn kv_label_width(width: u16) -> usize {
    if width < 40 { 11 } else { 17 }
}

fn cost_row(label: &str, cost: f64, emphasize: bool, label_width: usize) -> Line<'static> {
    let value_style = if emphasize {
        Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(TEXT)
    };
    Line::from(vec![
        Span::styled(format!("{label:<label_width$}"), Style::default().fg(MUTED)),
        Span::styled(format_usd(cost), value_style),
    ])
}

/// Cost over the trailing `days` ending at the period end, summed from the
/// per-day per-model usage (cache-aware per entry).
fn window_cost_usd(summary: &Summary, days: u16) -> f64 {
    let start = summary.period_end - Duration::days(i64::from(days) - 1);
    summary
        .model_daily
        .iter()
        .filter(|entry| entry.date >= start)
        .filter_map(|entry| usage_cost_usd(&entry.model, &entry.usage))
        .sum()
}

/// Top repositories by token volume — where the AI time actually goes.
fn project_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    if summary.projects.is_empty() {
        return Vec::new();
    }
    let max = summary
        .projects
        .iter()
        .map(|project| project.usage.token_volume())
        .max()
        .unwrap_or(0);
    if max == 0 {
        return Vec::new();
    }

    // Repository names need more label room than tool names; trade bar length.
    let label_width = 20_usize;
    let bar_width = usize::from(width)
        .saturating_sub(label_width + 9)
        .clamp(8, 24);
    let mut lines = vec![section_title("PROJECTS", "by token volume")];
    for project in summary.projects.iter().take(6) {
        let label = compact_label_tail(&project.name, label_width - 1);
        let value = project.usage.token_volume();
        let filled = bar_fill(value, max, bar_width);
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<label_width$}"), Style::default().fg(TEXT)),
            Span::styled("▄".repeat(filled), Style::default().fg(ACCENT)),
            Span::styled("▄".repeat(bar_width - filled), Style::default().fg(FAINT)),
            Span::styled(
                format!(" {:>7}", format_tokens(value)),
                Style::default().fg(MUTED),
            ),
        ]));
    }
    let hidden = summary.projects.len().saturating_sub(6);
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("+{hidden} more"),
            Style::default().fg(DIM),
        )));
    }
    lines
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Bar width is a bounded terminal rendering concern."
)]
fn bar_fill(value: u64, max: u64, width: usize) -> usize {
    if max == 0 {
        0
    } else {
        ((value as f64 / max as f64) * width as f64).round() as usize
    }
    .min(width)
}

fn section_title(title: &'static str, annotation: &str) -> Line<'static> {
    let mut spans = vec![
        Span::styled("▍ ", Style::default().fg(GOLD)),
        Span::styled(
            title,
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
    ];
    if !annotation.is_empty() {
        spans.push(Span::styled(
            format!("  {annotation}"),
            Style::default().fg(DIM),
        ));
    }
    Line::from(spans)
}

fn model_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let top_models = summary.models.iter().take(6).collect::<Vec<_>>();
    if top_models.is_empty() {
        return vec![
            section_title("MODELS", ""),
            Line::from(Span::styled(
                "No model usage found",
                Style::default().fg(MUTED),
            )),
        ];
    }

    if !token_usage_available(summary) {
        let mut lines = vec![section_title(
            "MODELS",
            "observed in logs — no token volume",
        )];
        for model in top_models {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<22}", compact_label(&short_model_name(&model.name), 21)),
                    Style::default().fg(TEXT),
                ),
                Span::styled(
                    format!("{:>6} events", model.events),
                    Style::default().fg(MUTED),
                ),
            ]));
        }
        return lines;
    }

    // Claude-usage-style horizontal bars, one color per model.
    let total_volume = summary.total_usage.token_volume();
    let max_volume = top_models
        .first()
        .map_or(0, |model| model.usage.token_volume());
    let bar_width = usize::from(width).saturating_sub(31).clamp(8, 24);
    let mut lines = vec![section_title("MODELS", "share of period")];
    for (index, model) in top_models.into_iter().enumerate() {
        let volume = model.usage.token_volume();
        let share = if total_volume == 0 {
            String::new()
        } else {
            format_percent(volume, total_volume)
        };
        let filled = bar_fill(volume, max_volume, bar_width);
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<14}", compact_label(&short_model_name(&model.name), 13)),
                Style::default().fg(TEXT),
            ),
            Span::styled("▄".repeat(filled), Style::default().fg(model_color(index))),
            Span::styled("▄".repeat(bar_width - filled), Style::default().fg(FAINT)),
            Span::styled(
                format!(" {:>8}", format_tokens(volume)),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{share:>7}"), Style::default().fg(MUTED)),
        ]));
    }
    lines
}

fn tool_lines(summary: &Summary, width: u16, limit: usize) -> Vec<Line<'static>> {
    if summary.tools.is_empty() {
        return vec![
            section_title("TOOLS", ""),
            Line::from(Span::styled(
                "No tool calls found",
                Style::default().fg(MUTED),
            )),
        ];
    }
    let total_calls: usize = summary.tools.iter().map(|tool| tool.calls).sum();
    let max = summary
        .tools
        .iter()
        .map(|tool| tool.calls)
        .max()
        .unwrap_or(0);
    let mut lines = vec![section_title(
        "TOOLS",
        &format!("{} calls", format_count(total_calls)),
    )];
    let bar_width = bar_width_for(width);
    lines.extend(
        summary
            .tools
            .iter()
            .take(limit)
            .map(|tool| count_bar_line(&tool.name, tool.calls, max, bar_width, GREEN)),
    );
    let hidden = summary.tools.len().saturating_sub(limit);
    if hidden > 0 {
        let hidden_calls: usize = summary
            .tools
            .iter()
            .skip(limit)
            .map(|tool| tool.calls)
            .sum();
        lines.push(Line::from(Span::styled(
            format!("+{hidden} more · {} calls", format_count(hidden_calls)),
            Style::default().fg(DIM),
        )));
    }
    lines
}

fn signal_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let label_width = kv_label_width(width);
    let most_active = summary.most_active_day.as_ref().map_or_else(
        || "—".to_owned(),
        |day| {
            format!(
                "{} · {}",
                format_date(day.date),
                format_tokens(day.usage.token_volume())
            )
        },
    );
    let busiest_hour = summary.busiest_hour.map_or_else(
        || "—".to_owned(),
        |(hour, usage)| format!("{hour:02}:00 · {}", format_tokens(usage)),
    );
    let longest_session = summary.longest_session.as_ref().map_or_else(
        || "—".to_owned(),
        |session| format_duration_secs(session.duration_secs()),
    );
    let streaks = format!(
        "{}d now · {}d best",
        summary.current_streak_days, summary.longest_streak_days
    );

    let mut lines = vec![section_title("SIGNAL", "")];
    lines.push(kv(
        "Favorite",
        &summary
            .favorite_model
            .as_deref()
            .map_or_else(|| "—".to_owned(), short_model_name),
        label_width,
    ));
    lines.push(kv("Top day", &most_active, label_width));
    lines.push(kv("Peak hour", &busiest_hour, label_width));
    lines.push(kv("Longest", &longest_session, label_width));
    lines.push(kv("Streak", &streaks, label_width));
    lines
}

fn agent_lines(summary: &Summary, width: u16, limit: usize) -> Vec<Line<'static>> {
    let with_usage = token_usage_available(summary);
    let show_calls = width >= 40;
    let name_width = usize::from(width)
        .saturating_sub(if show_calls { 20 } else { 10 })
        .clamp(10, 18);
    let mut lines = vec![section_title("SUBAGENTS", "by token volume")];
    for agent in summary.agents.iter().take(limit) {
        let mut spans = vec![Span::styled(
            format!(
                "{:<width$}",
                compact_label(&agent.name, name_width.saturating_sub(1)),
                width = name_width
            ),
            Style::default().fg(TEXT),
        )];
        if with_usage {
            spans.push(Span::styled(
                format!("{:>8}", format_tokens(agent.usage.token_volume())),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ));
        }
        if show_calls && agent.calls > 0 {
            spans.push(Span::styled(
                format!("  {} calls", agent.calls),
                Style::default().fg(MUTED),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn duration_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let Some(duration) = &summary.completion_duration else {
        return Vec::new();
    };
    let max = duration
        .buckets
        .iter()
        .map(|bucket| bucket.count)
        .max()
        .unwrap_or(0);
    // Autonomy signal: how often a turn ran 20+ minutes unattended.
    let autonomous: usize = duration
        .buckets
        .iter()
        .skip(3)
        .map(|bucket| bucket.count)
        .sum();
    let mut lines = vec![
        section_title(
            "COMPLETION",
            &format!(
                "{} turns · {} ran ≥20m",
                format_count(duration.count),
                format_count(autonomous)
            ),
        ),
        Line::from(vec![
            Span::styled("p50 ", Style::default().fg(MUTED)),
            Span::styled(
                format_duration_ms(duration.p50_ms),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   p90 ", Style::default().fg(MUTED)),
            Span::styled(
                format_duration_ms(duration.p90_ms),
                Style::default().fg(TEXT),
            ),
            Span::styled("   max ", Style::default().fg(MUTED)),
            Span::styled(
                format_duration_ms(duration.max_ms),
                Style::default().fg(TEXT),
            ),
        ]),
    ];
    let bar_width = bar_width_for(width);
    for bucket in &duration.buckets {
        lines.push(count_bar_line(
            &bucket.label,
            bucket.count,
            max,
            bar_width,
            BLUE,
        ));
    }
    lines
}

/// Bar track length for a column: fixed label (14) + count (8) columns,
/// the bar absorbs the rest.
fn bar_width_for(width: u16) -> usize {
    usize::from(width).saturating_sub(22).clamp(8, 24)
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, state: &UiState, summary: &Summary) {
    let mut key_spans = vec![
        Span::styled("←→", Style::default().fg(MUTED)),
        Span::styled(" provider   ", Style::default().fg(DIM)),
    ];
    if state.max_scroll.get() > 0 {
        key_spans.push(Span::styled("↑↓", Style::default().fg(MUTED)));
        key_spans.push(Span::styled(" scroll   ", Style::default().fg(DIM)));
    }
    key_spans.extend([
        Span::styled("r", Style::default().fg(MUTED)),
        Span::styled(" reload   ", Style::default().fg(DIM)),
        Span::styled("q", Style::default().fg(MUTED)),
        Span::styled(" quit", Style::default().fg(DIM)),
    ]);
    let keys = Line::from(key_spans);

    let scan = if state.status.is_empty() {
        Line::from(Span::styled(
            format!(
                "{} files · {} lines · loaded in {}ms",
                format_count(summary.scan_stats.files_seen),
                format_count(summary.scan_stats.lines_seen),
                state.report.load_duration_ms
            ),
            Style::default().fg(DIM),
        ))
    } else {
        Line::from(Span::styled(state.status.clone(), Style::default().fg(HOT)))
    };

    frame.render_widget(Paragraph::new(keys.clone()), area);
    // Right-side stats only when they fit beside the key hints.
    if keys.width() + scan.width() + 3 <= usize::from(area.width) {
        frame.render_widget(Paragraph::new(scan).alignment(Alignment::Right), area);
    }
}

fn weekday_index(date: Date) -> u8 {
    match date.weekday() {
        Weekday::Monday => 0,
        Weekday::Tuesday => 1,
        Weekday::Wednesday => 2,
        Weekday::Thursday => 3,
        Weekday::Friday => 4,
        Weekday::Saturday => 5,
        Weekday::Sunday => 6,
    }
}

fn month_abbrev(month: time::Month) -> &'static str {
    match month {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    }
}

fn kv(label: &str, value: &str, label_width: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<label_width$}"), Style::default().fg(MUTED)),
        Span::styled(value.to_owned(), Style::default().fg(TEXT)),
    ])
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Bar width is a bounded terminal rendering concern."
)]
fn count_bar_line(
    label: &str,
    value: usize,
    max: usize,
    width: usize,
    color: Color,
) -> Line<'static> {
    let filled = if max == 0 {
        0
    } else {
        ((value as f64 / max as f64) * width as f64).round() as usize
    }
    .min(width);
    let empty = width - filled;
    let label = compact_label(label, 13);
    Line::from(vec![
        Span::styled(format!("{label:<14}"), Style::default().fg(TEXT)),
        Span::styled("▄".repeat(filled), Style::default().fg(color)),
        Span::styled("▄".repeat(empty), Style::default().fg(FAINT)),
        Span::styled(
            format!(" {:>6}", format_count(value)),
            Style::default().fg(MUTED),
        ),
    ])
}

/// Truncate keeping the END of the label — repository names differ at the
/// tail ("…-genkan-app"), not the head.
fn compact_label_tail(label: &str, width: usize) -> String {
    let count = label.chars().count();
    if count <= width {
        return label.to_owned();
    }
    let keep = width.saturating_sub(1);
    let mut value = String::from("…");
    value.extend(label.chars().skip(count - keep));
    value
}

fn compact_label(label: &str, width: usize) -> String {
    if label.chars().count() <= width {
        return label.to_owned();
    }
    let keep = width.saturating_sub(1);
    let mut value = label.chars().take(keep).collect::<String>();
    value.push('…');
    value
}

fn provider_color(provider: Provider) -> Color {
    match provider {
        Provider::Claude => HOT,
        Provider::Codex => GREEN,
        Provider::Agy => BLUE,
        Provider::Combined => GOLD,
    }
}

fn model_color(index: usize) -> Color {
    match index % 6 {
        0 => BLUE,
        1 => GREEN,
        2 => GOLD,
        3 => HOT,
        4 => PURPLE,
        _ => TEAL,
    }
}

fn token_usage_available(summary: &Summary) -> bool {
    summary.total_usage.token_volume() > 0
}

struct UiState {
    config: Config,
    report: AppSummary,
    tab_index: usize,
    status: String,
    /// Scroll offset for the sections area (alt-screen TUIs have no terminal
    /// scrollback, so overflow must scroll in-app).
    scroll: u16,
    /// Largest valid scroll offset, measured during the last draw.
    max_scroll: Cell<u16>,
}

impl UiState {
    fn tab_count(&self) -> usize {
        self.report.providers.len() + 1
    }

    fn tabs(&self) -> Vec<(&'static str, Color)> {
        let mut tabs = self
            .report
            .providers
            .iter()
            .map(|summary| (summary.provider.label(), provider_color(summary.provider)))
            .collect::<Vec<_>>();
        tabs.push((
            Provider::Combined.label(),
            provider_color(Provider::Combined),
        ));
        tabs
    }

    fn current_summary(&self) -> &Summary {
        self.report
            .providers
            .get(self.tab_index)
            .unwrap_or(&self.report.combined)
    }

    fn next_tab(&mut self) {
        self.tab_index = (self.tab_index + 1) % self.tab_count();
        self.scroll = 0;
    }

    fn previous_tab(&mut self) {
        self.tab_index = if self.tab_index == 0 {
            self.tab_count() - 1
        } else {
            self.tab_index - 1
        };
        self.scroll = 0;
    }
}
