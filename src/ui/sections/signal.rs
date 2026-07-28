use crate::format::{format_date, format_duration_secs, format_tokens, short_model_name};
use crate::model::Summary;
use crate::ui::utils;
use ratatui::prelude::*;

pub(in crate::ui) fn signal_lines(summary: &Summary, width: u16) -> Vec<Line<'static>> {
    let label_width = utils::kv_label_width(width);
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

    let mut lines = vec![utils::section_title("SIGNAL", "")];
    lines.push(utils::kv(
        "Favorite",
        &summary
            .favorite_model
            .as_deref()
            .map_or_else(|| "—".to_owned(), short_model_name),
        label_width,
    ));
    lines.push(utils::kv("Top day", &most_active, label_width));
    lines.push(utils::kv("Peak hour", &busiest_hour, label_width));
    lines.push(utils::kv("Longest", &longest_session, label_width));
    lines.push(utils::kv("Streak", &streaks, label_width));
    lines
}
