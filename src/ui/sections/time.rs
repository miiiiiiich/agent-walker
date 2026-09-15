use crate::format::{format_count, format_duration_ms, format_tokens};
use crate::model::Summary;
use crate::ui::{theme, utils};
use ratatui::prelude::*;

/// WORKING TIME: how long the agent was actually working over the analysis
/// window, and how much context it re-read per minute of that. The turn
/// length minus the human's answer time is the working time — tool runs and
/// polling loops stay in (the agent was on the job), `AskUserQuestion`
/// round-trips come out. Context per minute is the density that tracks
/// cost: a long context dragged through many calls reads high here. Your
/// pace is the other side: how long you take between the agent stopping
/// and your next prompt. "made of" splits the working time into the
/// model's own share and tools running, so the headline hours read
/// honestly — a long batch wait is time the agent spent asleep.
/// Hours and minutes, never days: 30 days of working time reads as "87h
/// 12m", which compares across tabs and months the way "3d 15h" doesn't.
fn format_hours(duration_ms: u64) -> String {
    let minutes = duration_ms / 60_000;
    format!("{}h {:02}m", minutes / 60, minutes % 60)
}

pub(in crate::ui) fn time_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let Some(time) = &summary.active_time else {
        return Vec::new();
    };
    let label_width = utils::kv_label_width(width);
    // Width-fitted like the TURN LENGTH title: the cutoff note in its long
    // form when the rail has room, shorter otherwise, never clipped
    // mid-word. The prefix budget covers "▍ WORKING TIME  ".
    let budget = usize::from(width).saturating_sub("▍ WORKING TIME  ".chars().count());
    let window = utils::window_label(summary);
    let annotation = [
        format!("{window} · gaps over 30m excluded"),
        format!("{window} · >30m gaps out"),
        window.clone(),
    ]
    .into_iter()
    .find(|text| text.chars().count() <= budget)
    .unwrap_or(window);
    let mut lines = vec![utils::section_title("WORKING TIME", &annotation)];
    lines.push(Line::from(vec![
        Span::styled(
            format!("{:<label_width$}", "total"),
            Style::default().fg(theme::MUTED),
        ),
        Span::styled(
            format_hours(time.active_ms),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}/day", format_hours(time.active_per_day_ms())),
            Style::default().fg(theme::MUTED),
        ),
    ]));
    if time.human_wait_ms > 0 {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<label_width$}", "on you"),
                Style::default().fg(theme::MUTED),
            ),
            Span::styled(
                format_hours(time.human_wait_ms),
                Style::default().fg(theme::TEXT),
            ),
            Span::styled("  answering questions", Style::default().fg(theme::MUTED)),
        ]));
    }
    if let Some(per_minute) = time.context_per_minute() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<label_width$}", "context"),
                Style::default().fg(theme::MUTED),
            ),
            Span::styled(
                format!("{}/min", format_tokens(per_minute)),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  read per working minute",
                Style::default().fg(theme::MUTED),
            ),
        ]));
    }
    lines.extend(made_of_line(time, label_width, width));
    if let Some((date, active_ms)) = time.peak_day() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<label_width$}", "peak day"),
                Style::default().fg(theme::MUTED),
            ),
            Span::styled(
                format_hours(active_ms),
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {} {}", utils::month_abbrev(date.month()), date.day()),
                Style::default().fg(theme::MUTED),
            ),
        ]));
    }
    lines.extend(pace_line(time, label_width));
    lines.push(utils::kv("turns", &format_count(time.turns), label_width));
    lines
}

/// What the working time is made of: the model's share vs tools running,
/// qualified with the measured hours when some provider can't tell.
fn made_of_line(
    time: &crate::model::ActiveTimeSummary,
    label_width: usize,
    width: u16,
) -> Option<Line<'static>> {
    let model = time.model_share()?;
    let pct = |share: f64| format!("{:.0}%", share * 100.0);
    let label = Span::styled(
        format!("{:<label_width$}", "made of"),
        Style::default().fg(theme::MUTED),
    );
    let model_span = |text: String| {
        Span::styled(
            text,
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        )
    };
    let muted = |text: String| Span::styled(text, Style::default().fg(theme::MUTED));
    if time.measured_ms >= time.active_ms {
        return Some(Line::from(vec![
            label,
            muted("model ".to_owned()),
            model_span(pct(model)),
            muted(" · tools ".to_owned()),
            Span::styled(pct(1.0 - model), Style::default().fg(theme::TEXT)),
        ]));
    }
    // Providers whose logs can't tell (Copilot, OpenCode) are outside the
    // split, so the row must say how much of the total it covers — in
    // whichever form fits the rail, never clipped off the end: the
    // qualifier is the point of the row, so the tools half goes first.
    let measured = format_hours(time.measured_ms);
    let budget = usize::from(width).saturating_sub(label_width);
    let full = format!(
        "model {} · tools {}  of {measured} measured",
        pct(model),
        pct(1.0 - model)
    );
    let mid = format!(
        "model {} · tools {} ({measured})",
        pct(model),
        pct(1.0 - model)
    );
    let short = format!("model {} of {measured}", pct(model));
    let text = [full, mid, short]
        .into_iter()
        .find(|text| text.chars().count() <= budget)
        .unwrap_or_else(|| format!("model {}", pct(model)));
    let (head, rest) = text.split_at("model ".len());
    let (value, tail) = rest.split_once(' ').unwrap_or((rest, ""));
    Some(Line::from(vec![
        label,
        muted(head.to_owned()),
        model_span(value.to_owned()),
        muted(if tail.is_empty() {
            String::new()
        } else {
            format!(" {tail}")
        }),
    ]))
}

/// Your pace as one row: p50 / p90 / average gap before a prompt.
fn pace_line(time: &crate::model::ActiveTimeSummary, label_width: usize) -> Option<Line<'static>> {
    let (p50, p90) = time.pace_percentiles()?;
    let mut spans = vec![
        Span::styled(
            format!("{:<label_width$}", "your pace"),
            Style::default().fg(theme::MUTED),
        ),
        Span::styled("p50 ", Style::default().fg(theme::MUTED)),
        Span::styled(
            format_duration_ms(p50),
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · p90 ", Style::default().fg(theme::MUTED)),
        Span::styled(format_duration_ms(p90), Style::default().fg(theme::TEXT)),
    ];
    if let Some(mean) = time.pace_mean_ms() {
        spans.push(Span::styled(" · avg ", Style::default().fg(theme::MUTED)));
        spans.push(Span::styled(
            format_duration_ms(mean),
            Style::default().fg(theme::TEXT),
        ));
    }
    spans.push(Span::styled(
        "  between turns",
        Style::default().fg(theme::MUTED),
    ));
    Some(Line::from(spans))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// The fixture's 87h over 30 days renders the working time, the human
    /// wait, and the context rate; a summary without turns renders nothing.
    #[test]
    fn renders_working_time_and_density() {
        let summary = crate::share::fixtures::sample_summary();
        let lines = time_lines(&summary, 60);
        let text: Vec<String> = lines.iter().map(rendered).collect();
        assert!(
            text[0].contains("WORKING TIME") && text[0].contains("30m"),
            "{text:?}"
        );
        assert!(!rendered(&time_lines(&summary, 30)[0]).contains("30m"));
        assert!(
            text[1].contains("87h 00m") && text[1].contains("2h 54m/day"),
            "{text:?}"
        );
        assert!(text[2].contains("6h 40m"), "{text:?}");
        assert!(text[3].contains("/min"), "{text:?}");
        assert!(
            text[4].contains("made of")
                && text[4].contains("model 55%")
                && text[4].contains("of 80h 00m measured"),
            "{text:?}"
        );
        let mut full = crate::share::fixtures::sample_summary();
        if let Some(time) = full.active_time.as_mut() {
            time.measured_ms = time.active_ms;
        }
        assert!(!rendered(&time_lines(&full, 60)[4]).contains("measured"));
        // Narrower rails keep the coverage in a shorter form, never clipped.
        for width in [60_u16, 46, 38, 32] {
            let row = rendered(&time_lines(&summary, width)[4]);
            assert!(row.contains("80h 00m"), "{width}: {row}");
            assert!(row.chars().count() <= usize::from(width), "{width}: {row}");
        }

        let mut silent = summary;
        silent.active_time = None;
        assert!(time_lines(&silent, 60).is_empty());
    }
}
