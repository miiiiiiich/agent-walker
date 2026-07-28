use crate::format::{format_percent, format_tokens};
use crate::model::Summary;
use crate::ui::{theme, utils};
use ratatui::prelude::*;

/// SKILLS: token volume by Claude `attributionSkill` over the fixed 30-day
/// window (the display `--days` does not apply — attribution fields exist
/// only in recent logs). Claude-tab only, TUI-only: skill names are
/// personal-environment labels that must never reach the share card, so this
/// section reads `summary.skills`, never the `models` path the card renders.
/// Share is of ATTRIBUTED volume; the subtitle carries the honest denominator.
pub(in crate::ui) fn skill_lines(
    summary: &Summary,
    width: u16,
    limit: usize,
) -> Vec<Line<'static>> {
    if summary.skills.is_empty() {
        return Vec::new();
    }
    let attributed = summary.skills.iter().fold(0_u64, |sum, skill| {
        sum.saturating_add(skill.usage.token_volume())
    });
    let subtitle = if summary.recent_window_volume > 0 {
        format!(
            "30d · attributed {} of volume",
            format_percent(attributed, summary.recent_window_volume)
        )
    } else {
        "30d".to_owned()
    };
    let max_volume = summary
        .skills
        .first()
        .map_or(0, |skill| skill.usage.token_volume());
    let bar_width = usize::from(width).saturating_sub(31).clamp(8, 24);

    let mut lines = vec![utils::section_title("SKILLS", &subtitle)];
    for skill in summary.skills.iter().take(limit) {
        let volume = skill.usage.token_volume();
        let filled = utils::bar_fill(volume, max_volume, bar_width);
        lines.push(utils::stat_bar_line(
            &skill.name,
            theme::ACCENT,
            filled,
            bar_width,
            &format_tokens(volume),
            &format_percent(volume, attributed),
        ));
    }
    lines
}
