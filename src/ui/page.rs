use ratatui::prelude::*;

use crate::model::{Provider, Summary};

use super::activity;
use super::badge;
use super::charts;
use super::hero;
use super::sections;
use super::utils;

/// Two-column layout needs at least this much width; below it sections stack.
/// Kept low — two columns halve the page height, which matters more than
/// generous column widths on small terminals.
const TWO_COLUMN_MIN_WIDTH: u16 = 80;

/// The whole dashboard body as one flowing list of lines. Charts and the
/// two-column section area are rendered into lines (not widgets) so the
/// entire page scrolls as a unit.
#[allow(
    clippy::too_many_lines,
    reason = "Flat assembly of the page in decided section order; splitting adds indirection without logic."
)]
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
    let badge = badge::codename_badge_lines(&crate::codename::for_summary(summary));
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
            // Every left-rail chart takes the full left column; the shared
            // one-char-per-column standard inside charts.rs keeps TOKENS PER
            // DAY, LIMITS, and CREDITS at identical widths.
            let left = charts::model_chart_lines(summary, left_u16, CHART_BODY);
            let right = charts::hourly_chart_lines(summary, right_u16, CHART_BODY);
            lines.extend(join_columns(&left, &right, left_width + 2));
        }
        lines.push(Line::default());
    }

    // LIMITS history lives in the chart band on the Codex tab only — the
    // rate-limit data is Codex-specific and deliberately historical (no
    // "current" meter; this dashboard looks back, it doesn't monitor).
    if summary.provider == Provider::Codex {
        let chart_width = if two_column { left_u16 } else { width };
        let limits = charts::limits_chart_lines(summary, chart_width, CHART_BODY);
        if !limits.is_empty() {
            lines.extend(limits);
            lines.push(Line::default());
        }
    }

    // CREDITS history is Copilot-only for the same reason LIMITS is
    // Codex-only: the AI-credit ledger is that provider's own accounting,
    // and it is deliberately historical.
    if summary.provider == Provider::Copilot {
        let chart_width = if two_column { left_u16 } else { width };
        let credits = charts::credits_chart_lines(summary, chart_width, CHART_BODY);
        if !credits.is_empty() {
            lines.extend(credits);
            lines.push(Line::default());
        }
    }

    // Per-tab v0.9 sections: SKILLS is Claude-only (attribution is a Claude
    // log feature), MODES renders each provider's own dial. The Total tab
    // shows neither — they are not cross-provider metrics.
    let skills = if summary.provider == Provider::Claude {
        sections::skill_lines(summary, if two_column { left_u16 } else { width }, 6)
    } else {
        Vec::new()
    };
    // CONTEXT (cache reuse) renders on every tab, Total included — the
    // summary adds up across providers.
    let context = sections::context_lines(summary, if two_column { right_u16 } else { width });
    let modes = if matches!(summary.provider, Provider::Claude | Provider::Codex) {
        sections::modes_lines(summary, if two_column { right_u16 } else { width })
    } else {
        Vec::new()
    };

    // Decided chart priority: activity → token/day → by-hour → completion →
    // PARALLEL AGENTS → models → (cost/signal/projects/tools/agents).
    if width < TWO_COLUMN_MIN_WIDTH {
        if summary.completion_duration.is_some() || summary.interrupted > 0 {
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
        if !skills.is_empty() {
            lines.extend(skills);
            lines.push(Line::default());
        }
        // Same rule as the two-column layout: present drive panels first,
        // then the bookkeeping pair in fixed order (below the usage sections).
        for block in [context, modes].into_iter().filter(|b| !b.is_empty()) {
            lines.extend(block);
            lines.push(Line::default());
        }
        lines.extend(sections::project_lines(summary, width));
        lines.push(Line::default());
        lines.extend(sections::tool_lines(summary, width, 6));
        if !summary.agents.is_empty() {
            lines.push(Line::default());
            lines.extend(sections::agent_lines(summary, width, 4));
        }
        lines.push(Line::default());
        lines.extend(sections::cost_lines(summary, width));
        lines.push(Line::default());
        lines.extend(sections::signal_lines(summary, width));
        return lines;
    }

    // completion | PARALLEL AGENTS, side by side ("PARALLEL next to completion"),
    // before the model/cost sections.
    let has_parallel = summary
        .orchestration
        .time_by_level
        .iter()
        .any(|secs| *secs > 0);
    if summary.completion_duration.is_some() || summary.interrupted > 0 || has_parallel {
        let completion = sections::duration_lines(summary, left_u16);
        let parallel = sections::parallel_lines(summary, right_u16);
        lines.extend(join_columns(&completion, &parallel, left_width + 2));
        lines.push(Line::default());
    }

    // Pair sections column-wise so each row of sections starts on the same
    // line in both columns: MODELS|COST, (SKILLS), PROJECTS|SIGNAL,
    // line in both columns. Left: MODELS, (SKILLS), PROJECTS, TOOLS. Right:
    // CONTEXT, MODES, (SUBAGENTS), COST, SIGNAL. The "how you drive it" panels
    // (CONTEXT, MODES) lead the right column and the bookkeeping panels
    // (COST, SIGNAL) close it; a tab without context or mode data keeps
    // COST / SIGNAL in the top slots. SKILLS slots directly under MODELS on
    // the Claude tab.
    let mut left_blocks = vec![sections::model_lines(summary, left_u16)];
    if !skills.is_empty() {
        left_blocks.push(skills);
    }
    left_blocks.push(sections::project_lines(summary, left_u16));
    left_blocks.push(sections::tool_lines(summary, left_u16, 10));
    // Whichever "drive" panels exist lead in order, then SUBAGENTS, then the
    // bookkeeping pair in its own fixed order — so a tab without MODES still
    // reads CONTEXT, COST, SIGNAL, and one without either keeps COST first.
    let mut right_blocks: Vec<Vec<Line<'static>>> = [context, modes]
        .into_iter()
        .filter(|block| !block.is_empty())
        .collect();
    if !summary.agents.is_empty() {
        right_blocks.push(sections::agent_lines(summary, right_u16, 5));
    }
    right_blocks.push(sections::cost_lines(summary, right_u16));
    right_blocks.push(sections::signal_lines(summary, right_u16));

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

#[cfg(test)]
mod tests {
    use time::macros::date;

    use super::*;
    use crate::model::{LimitDay, LimitsHistory, ModesSummary, SkillStat, TokenUsage};
    use crate::share::fixtures::sample_summary;

    fn rendered(summary: &Summary, width: u16) -> String {
        page_lines(summary, width)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn v09_summary(provider: Provider) -> Summary {
        let mut summary = sample_summary();
        summary.provider = provider;
        summary.skills = vec![SkillStat {
            name: "sk:review".to_owned(),
            usage: TokenUsage {
                input_tokens: 1_000_000,
                ..TokenUsage::default()
            },
        }];
        summary.limits = Some(LimitsHistory {
            days: vec![
                (date!(2026 - 06 - 10), LimitDay::NoUse),
                (date!(2026 - 06 - 11), LimitDay::Measured(100.0)),
                (date!(2026 - 06 - 12), LimitDay::NoSample),
            ],
            peak: Some((date!(2026 - 06 - 11), 100.0)),
        });
        summary.modes = ModesSummary {
            assistant_turns: 100,
            thinking_turns: 49,
            fast_turns: 0,
            efforts: vec![("xhigh".to_owned(), 93), ("low".to_owned(), 6)],
            permissions: Vec::new(),
        };
        summary.credits = Some(crate::model::CreditsHistory {
            days: (0..30)
                .map(|offset| {
                    let date = time::macros::date!(2026 - 06 - 28) + time::Duration::days(offset);
                    (date, if offset == 20 { 4.2 } else { 0.4 })
                })
                .collect(),
            total: 15.8,
            peak: Some((time::macros::date!(2026 - 07 - 18), 4.2)),
        });
        summary
    }

    /// SKILLS renders on the Claude tab only; LIMITS on the Codex tab only;
    /// MODES on each provider tab; the Total tab shows none of them even when
    /// the combined summary carries the data.
    /// Wrapper-level frame pins: LIMITS and CREDITS render in the shared
    /// column frame (7-char gutter + one char per window day) with their own
    /// y-axis labels, and the canonical stat row keeps its exact shape.
    #[test]
    fn column_chart_wrappers_share_the_frame() {
        let text = |line: &ratatui::text::Line<'_>| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };

        let codex = v09_summary(Provider::Codex);
        let limits = charts::limits_chart_lines(&codex, 110, 6);
        let days = codex
            .limits
            .as_ref()
            .expect("fixture has limits")
            .days
            .len();
        assert_eq!(limits[1].width(), charts::Y_AXIS_WIDTH + days);
        assert!(text(&limits[1]).starts_with("  100%│"));

        let copilot = v09_summary(Provider::Copilot);
        let credits = charts::credits_chart_lines(&copilot, 110, 6);
        let days = copilot
            .credits
            .as_ref()
            .expect("fixture has credits")
            .days
            .len();
        assert_eq!(credits[1].width(), charts::Y_AXIS_WIDTH + days);
        assert!(text(&credits[1]).starts_with("  4.20│"));

        let row = utils::stat_bar_line("grok", crate::ui::theme::GREEN, 5, 10, "1.2B", "48%");
        assert_eq!(
            text(&row),
            format!(
                "{:<14}{}{} {:>8}{:>7}",
                "grok",
                "▄".repeat(5),
                "▄".repeat(5),
                "1.2B",
                "48%"
            )
        );
    }

    #[test]
    fn v09_sections_render_on_their_own_tabs_only() {
        let claude = rendered(&v09_summary(Provider::Claude), 110);
        assert!(claude.contains("SKILLS"));
        assert!(claude.contains("attributed"));
        assert!(claude.contains("MODES"));
        assert!(claude.contains("thinking"));
        assert!(!claude.contains("LIMITS"));

        let codex = rendered(&v09_summary(Provider::Codex), 110);
        assert!(codex.contains("LIMITS"));
        assert!(codex.contains("peak 100%"));
        assert!(codex.contains("MODES"));
        assert!(codex.contains("xhigh"));
        assert!(!codex.contains("SKILLS"));

        let copilot = rendered(&v09_summary(Provider::Copilot), 110);
        assert!(copilot.contains("CREDITS"));
        assert!(copilot.contains("30d total"));
        assert!(!copilot.contains("SKILLS"));
        assert!(!copilot.contains("LIMITS"));

        let total = rendered(&v09_summary(Provider::Combined), 110);
        assert!(!total.contains("SKILLS"));
        assert!(!total.contains("LIMITS"));
        assert!(!total.contains("MODES"));
        assert!(!total.contains("CREDITS"));
    }

    /// Narrow terminals stack the sections; the per-tab rules still hold.
    #[test]
    fn v09_sections_render_in_narrow_layout() {
        let claude = rendered(&v09_summary(Provider::Claude), 60);
        assert!(claude.contains("SKILLS"));
        assert!(claude.contains("MODES"));

        let codex = rendered(&v09_summary(Provider::Codex), 60);
        assert!(codex.contains("LIMITS"));
        assert!(!codex.contains("SKILLS"));
    }

    /// The demo (`AGENT_WALKER_DEMO=1`) must actually exercise the new
    /// sections end-to-end: synthetic collections through the real analyzer
    /// through the real page renderer. Guards against demo fixtures that
    /// compile but never fire (e.g. a skill pick rate that rounds to zero).
    /// The demo persona runs Claude and Codex only, so CREDITS (Copilot) is
    /// covered by the fixture tests instead.
    #[test]
    fn demo_report_renders_v09_sections() {
        let config = crate::app::Config {
            demo: true,
            claude_dir: std::path::PathBuf::new(),
            codex_dir: std::path::PathBuf::new(),
            agy_dir: None,
            copilot_dir: None,
            grok_dir: None,
            opencode_dir: None,
            cursor: None,
            days: 30,
            use_cache: false,
            local_offset: time::UtcOffset::UTC,
        };
        let report = crate::demo::demo_report(&config);

        let claude = report
            .providers
            .iter()
            .find(|summary| summary.provider == Provider::Claude)
            .expect("demo should have a Claude provider");
        let claude_page = rendered(claude, 110);
        assert!(claude_page.contains("SKILLS"));
        assert!(claude_page.contains("attributed"));
        assert!(claude_page.contains("thinking"));

        let codex = report
            .providers
            .iter()
            .find(|summary| summary.provider == Provider::Codex)
            .expect("demo should have a Codex provider");
        let codex_page = rendered(codex, 110);
        assert!(codex_page.contains("LIMITS"));
        assert!(codex_page.contains("peak 100%"));
        assert!(codex_page.contains("xhigh"));

        let total_page = rendered(&report.combined, 110);
        assert!(!total_page.contains("SKILLS"));
        assert!(!total_page.contains("LIMITS"));
        assert!(!total_page.contains("MODES"));
        assert!(!total_page.contains("CREDITS"));
    }

    /// Right-column order: the "how you drive it" panels lead (CONTEXT next
    /// to MODELS, then MODES) and the bookkeeping panels close (COST, then
    /// SIGNAL).
    #[test]
    fn right_column_leads_with_context_and_modes() {
        let text = rendered(&v09_summary(Provider::Claude), 120);
        let models_row = text
            .lines()
            .find(|line| line.contains("▍ MODELS"))
            .expect("models row");
        assert!(models_row.contains("▍ CONTEXT"), "{models_row}");
        let at = |title: &str| text.find(title).expect(title);
        assert!(at("▍ CONTEXT") < at("▍ MODES"));
        assert!(at("▍ MODES") < at("▍ COST"));
        assert!(at("▍ COST") < at("▍ SIGNAL"));

        // Without MODES (Total / other providers) COST still precedes SIGNAL.
        let mut total = v09_summary(Provider::Combined);
        total.modes = ModesSummary::default();
        let text = rendered(&total, 120);
        let at = |title: &str| text.find(title).expect(title);
        assert!(!text.contains("▍ MODES"));
        assert!(at("▍ CONTEXT") < at("▍ COST"));
        assert!(at("▍ COST") < at("▍ SIGNAL"));
    }
}
