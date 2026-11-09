use std::path::PathBuf;

use time::macros::{datetime, offset};
use time::{OffsetDateTime, UtcOffset};

use super::lines;
use crate::analyzer::{SessionModel, SessionReport};
use crate::model::{AgentStat, SkillStat, TokenUsage, ToolStat};
use crate::share::fixtures::sample_summary;
use crate::ui::{BLUE, GOLD};

const OFFSET: UtcOffset = offset!(+09:00);
const AT: OffsetDateTime = datetime!(2026-11-03 08:43 UTC);

fn report() -> SessionReport {
    let mut summary = sample_summary();
    summary.root = PathBuf::new();
    summary.tools = ["Bash", "WebFetch", "Read"]
        .into_iter()
        .map(|name| ToolStat {
            name: name.to_owned(),
            calls: 12,
        })
        .collect();
    summary.agents = vec![AgentStat {
        name: "code-evaluator".to_owned(),
        usage: TokenUsage::default(),
        calls: 2,
    }];
    summary.skills = vec![SkillStat {
        name: "ref:ntn".to_owned(),
        usage: TokenUsage::default(),
    }];
    if let Some(time) = summary.active_time.as_mut() {
        time.human_wait_ms = 120_000;
    }
    SessionReport {
        id: "232f4aab-9dfd-4a3c-bbfb-3900229fcc57".to_owned(),
        first_at: AT,
        last_at: AT + time::Duration::days(2),
        summary,
        turn_lengths_ms: vec![60_000, 3_960_000, 120_000, 1_500_000],
        models: vec![SessionModel {
            label: "Opus 5.5".to_owned(),
            tokens: 100,
        }],
    }
}

fn plain(report: &SessionReport, width: usize) -> Vec<String> {
    lines::lines(report, width, OFFSET)
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn a_wide_terminal_puts_the_graph_beside_the_numbers() {
    let wide = plain(&report(), 100);
    assert!(
        wide[0].contains("SESSION  232f4aab  Nov 3 17:43 – Nov 5 17:43"),
        "{wide:#?}"
    );
    assert!(wide[0].contains("TURN LENGTH"), "{wide:#?}");
    assert!(wide[1].starts_with("Cost"), "{wide:#?}");

    let narrow = plain(&report(), 60);
    let title = narrow
        .iter()
        .position(|line| line.contains("TURN LENGTH"))
        .unwrap();
    let cost = narrow
        .iter()
        .position(|line| line.starts_with("Cost"))
        .unwrap();
    assert!(cost < title, "{narrow:#?}");
}

#[test]
fn nothing_passes_the_width() {
    let mut reports = vec![report()];
    let mut wide_names = report();
    wide_names.summary.tools[0].name = "日本語のとても長いツールの名前です".repeat(2);
    reports.push(wide_names);
    let mut joined_emoji = report();
    joined_emoji.summary.tools[0].name =
        "mcp__family__👨\u{200d}👩\u{200d}👧\u{200d}👦__retrieve_customer_account_status"
            .to_owned();
    reports.push(joined_emoji);
    let mut long_turn = report();
    long_turn.turn_lengths_ms = vec![36_600_000, 60_000];
    reports.push(long_turn);
    let mut all_cached = report();
    if let Some(context) = all_cached.summary.context.as_mut() {
        context.cached_tokens = context.context_tokens;
    }
    reports.push(all_cached);
    for report in &reports {
        for width in [24, 30, 45, 46, 48, 60, 80, 91, 92, 93, 100, 140, 200] {
            for line in lines::lines(report, width, OFFSET) {
                assert!(line.width() <= width, "{width}: {:?}", line.to_string());
            }
        }
    }
}

#[test]
fn the_rows_are_the_tuis_and_none_is_about_tokens() {
    let report = report();
    let labels: Vec<&str> = lines::rows(&report).iter().map(|row| row.label).collect();
    assert_eq!(
        labels,
        [
            "Cost",
            "Working time",
            "On you",
            "Turns",
            "Your pace",
            "Tools",
            "Subagents",
            "Skills",
            "Models"
        ]
    );
    let text = plain(&report, 100).join("\n");
    assert!(text.contains("ran ≥20m"), "{text}");
    assert!(!text.contains("Tokens"), "{text}");
}

#[test]
fn a_row_with_nothing_to_say_is_left_out() {
    let mut report = report();
    report.summary.agents.clear();
    report.summary.skills.clear();
    if let Some(time) = report.summary.active_time.as_mut() {
        time.human_wait_ms = 0;
        time.pace_gaps_ms.clear();
    }
    let labels: Vec<&str> = lines::rows(&report).iter().map(|row| row.label).collect();
    assert_eq!(labels, ["Cost", "Working time", "Turns", "Tools", "Models"]);
}

#[test]
fn a_subagent_with_no_calls_to_count_is_named_without_a_zero() {
    let mut report = report();
    report.summary.agents[0].calls = 0;
    let line = plain(&report, 80)
        .into_iter()
        .find(|line| line.starts_with("Subagents"))
        .unwrap();
    assert!(line.contains("code-evaluator"), "{line}");
    assert!(!line.contains("calls") && !line.contains(" 0"), "{line}");
}

#[test]
fn shares_of_a_partly_measured_session_say_so() {
    let mut report = report();
    let time = report.summary.active_time.as_mut().unwrap();
    time.active_ms = 36_000_000;
    time.measured_ms = 3_600_000;
    time.model_ms = 3_600_000;
    let line = plain(&report, 80)
        .into_iter()
        .find(|line| line.starts_with("Working time"))
        .unwrap();
    assert!(line.contains("model 100% of 1h 00m"), "{line}");
    assert!(!line.contains("tools"), "{line}");
}

#[test]
fn an_unpriced_model_shows_beside_the_cost() {
    let mut report = report();
    report.summary.models[0].estimated.unpriced_volume = 5_000;
    let line = plain(&report, 80)
        .into_iter()
        .find(|line| line.starts_with("Cost"))
        .unwrap();
    assert!(line.contains("5.0K tokens unpriced"), "{line}");
}

#[test]
fn time_on_you_shows_only_when_there_is_some() {
    let mut report = report();
    if let Some(time) = report.summary.active_time.as_mut() {
        time.human_wait_ms = 120_000;
    }
    assert!(
        plain(&report, 80)
            .iter()
            .any(|line| line.starts_with("On you"))
    );
}

#[test]
fn unknown_values_show_a_dash_and_no_graph() {
    let mut report = report();
    report.summary.active_time = None;
    report.summary.completion_duration = None;
    report.turn_lengths_ms.clear();
    let text = plain(&report, 80);
    for label in ["Working time", "Turns"] {
        let line = text.iter().find(|line| line.starts_with(label)).unwrap();
        assert!(line.trim_end().ends_with('—'), "{line}");
    }
    assert!(
        !text.iter().any(|line| line.contains("TURN LENGTH")),
        "{text:#?}"
    );
}

#[test]
fn a_turn_is_gold_from_twenty_minutes_up() {
    let mut report = report();
    report.turn_lengths_ms = vec![1_199_999, 1_200_000];
    let lines = lines::lines(&report, 80, OFFSET);
    let bars: Vec<_> = lines
        .last()
        .unwrap()
        .spans
        .iter()
        .skip(2)
        .map(|span| span.style.fg)
        .collect();
    assert_eq!(bars, [Some(BLUE), Some(GOLD)]);
}

#[test]
fn a_long_turn_survives_being_squeezed() {
    let squeezed = lines::fit_columns(&[1, 2, 3_960_000, 4, 5, 6], 3);
    assert_eq!(squeezed, [2, 3_960_000, 6]);
    assert_eq!(lines::fit_columns(&[], 3), Vec::<u64>::new());
}

#[test]
fn the_cache_sits_under_the_graph_when_wide_and_last_when_stacked() {
    let report = report();
    assert!(report.summary.context.is_some());
    let wide = plain(&report, 100);
    let cache = wide
        .iter()
        .position(|line| line.contains("% cached"))
        .unwrap();
    let graph_end = wide.iter().rposition(|line| line.contains("┤")).unwrap();
    assert!(cache > graph_end, "{wide:#?}");
    assert!(
        cache < 10,
        "the context row must not lengthen the pane: {wide:#?}"
    );

    let narrow = plain(&report, 60);
    let cache = narrow
        .iter()
        .position(|line| line.starts_with("Context"))
        .unwrap();
    let models = narrow
        .iter()
        .position(|line| line.starts_with("Models"))
        .unwrap();
    assert_eq!(cache, models + 1, "{narrow:#?}");
}
