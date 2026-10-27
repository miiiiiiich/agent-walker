use serde_json::{Value, json};
use time::macros::{date, datetime, offset};

use super::*;
use crate::model::CostTally;
use crate::model::{
    CreditSample, DurationEvent, EffortEvent, InterruptEvent, LimitDay, LimitsHistory, ModeEvent,
    PaceEvent, PermissionEvent, RateLimitSample, SessionTouch, SourceKind, ToolEvent, UsageEvent,
};

fn report() -> AppSummary {
    let mut summary = crate::share::fixtures::sample_summary();
    summary.provider = Provider::Claude;
    summary.period_days = 1;
    summary.period_start = date!(2026 - 09 - 15);
    summary.period_end = date!(2026 - 09 - 15);
    AppSummary {
        generated_at: datetime!(2026-09-15 16:09:30 +09:00),
        period_days: 1,
        load_duration_ms: 7,
        combined: summary.clone(),
        providers: vec![summary],
    }
}

fn collection() -> Collection {
    Collection::new(Provider::Claude, std::path::PathBuf::new())
}

#[test]
fn document_shape_dates_nulls_and_newline() {
    let mut report = report();
    report.providers[0].context = None;
    let mut bytes = Vec::new();
    write_json(&mut bytes, &report, &[collection()], offset!(+09:00)).unwrap();
    assert_eq!(bytes.last(), Some(&b'\n'));
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.starts_with("{\n"));
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    let mut keys = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "generated_at",
            "providers",
            "scan",
            "schema_version",
            "time_basis",
            "total",
            "window",
        ]
    );
    assert_eq!(value["schema_version"], 3);
    assert_eq!(value["generated_at"], "2026-09-15T16:09:30+09:00");
    assert_eq!(
        value["window"],
        json!({"days": 1, "start": "2026-09-15", "end": "2026-09-15"})
    );
    assert_eq!(
        value["time_basis"],
        json!({"kind": "fixed_offset", "offset_seconds": 32_400})
    );
    assert_eq!(value["scan"]["load_ms"], 7);
    assert_eq!(value["providers"].as_array().unwrap().len(), 1);
    let provider = &value["providers"][0];
    assert_eq!(provider["provider"], "claude");
    for field in ["context", "modes", "limits"] {
        assert_eq!(provider["summary"].get(field), Some(&Value::Null));
    }
    for field in [
        "usage_events",
        "turns",
        "sessions",
        "tools",
        "rate_limits",
        "credits",
        "efforts",
        "modes",
        "permissions",
        "interrupts",
        "pace",
    ] {
        assert_eq!(provider.get(field), Some(&json!([])));
    }
    assert!(value["total"].get("usage_events").is_none());
    assert_eq!(
        value["total"]["hourly_profile"].as_array().unwrap().len(),
        24
    );
}

#[test]
fn limit_days_keep_all_three_states() {
    let history = LimitsHistory {
        days: vec![
            (date!(2026 - 09 - 13), LimitDay::Measured(42.0)),
            (date!(2026 - 09 - 14), LimitDay::NoSample),
            (date!(2026 - 09 - 15), LimitDay::NoUse),
        ],
        peak: Some((date!(2026 - 09 - 13), 42.0)),
    };
    let value = serde_json::to_value(histories::LimitsDto::new(&history)).unwrap();
    assert_eq!(
        value,
        json!({
            "peak": {"date": "2026-09-13", "used_percent": 42.0},
            "days": [
                {"date": "2026-09-13", "used_percent": 42.0, "state": "measured"},
                {"date": "2026-09-14", "used_percent": null, "state": "no_sample"},
                {"date": "2026-09-15", "used_percent": null, "state": "no_use"}
            ]
        })
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "Exercise every raw event stream at the same boundaries."
)]
fn every_event_stream_uses_local_dates_and_drops_undated_rows() {
    let report = report();
    let mut collection = collection();
    for timestamp in [
        None,
        Some(datetime!(2026-09-14 14:59:59 UTC)),
        Some(datetime!(2026-09-14 15:00 UTC)),
        Some(datetime!(2026-09-15 14:59:59 UTC)),
        Some(datetime!(2026-09-15 15:00 UTC)),
    ] {
        collection.usage_events.push(UsageEvent {
            timestamp,
            session_id: Some("session".to_owned()),
            model: None,
            source_kind: SourceKind::Subagent,
            attribution_agent: Some("reviewer".to_owned()),
            attribution_skill: Some("review".to_owned()),
            project: Some("work/repo".to_owned()),
            usage: TokenUsage {
                input_tokens: 10,
                ..TokenUsage::default()
            },
            granularity: crate::model::UsageGranularity::Call,
        });
        collection.duration_events.push(DurationEvent {
            timestamp,
            session_id: Some("session".to_owned()),
            duration_ms: 123,
            human_wait_ms: 23,
            model_ms: None,
            status: None,
        });
        collection.tool_events.push(ToolEvent {
            timestamp,
            session_id: Some("session".to_owned()),
            tool_name: "Read".to_owned(),
            subagent_type: None,
            source_kind: SourceKind::Main,
        });
        collection.effort_events.push(EffortEvent {
            timestamp,
            effort: "high".to_owned(),
        });
        collection.mode_events.push(ModeEvent {
            timestamp,
            has_thinking: true,
            fast: false,
        });
        collection.permission_events.push(PermissionEvent {
            timestamp,
            mode: "auto".to_owned(),
        });
        collection
            .interrupt_events
            .push(InterruptEvent { timestamp });
        collection.pace_events.push(PaceEvent {
            timestamp,
            gap_ms: 500,
        });
        if let Some(timestamp) = timestamp {
            collection.rate_limit_samples.push(RateLimitSample {
                timestamp,
                used_percent: 12.0,
            });
            collection.credit_samples.push(CreditSample {
                timestamp,
                nano_aiu: 1_000,
            });
            collection.session_touches.push(SessionTouch {
                timestamp,
                session_id: "session".to_owned(),
            });
        }
    }
    let value = serde_json::to_value(
        ReportDto::new(&report, std::slice::from_ref(&collection), offset!(+09:00)).unwrap(),
    )
    .unwrap();
    let provider = &value["providers"][0];
    for field in [
        "usage_events",
        "turns",
        "tools",
        "rate_limits",
        "credits",
        "efforts",
        "modes",
        "permissions",
        "interrupts",
        "pace",
    ] {
        let rows = provider[field].as_array().unwrap();
        assert_eq!(rows.len(), 2, "{field}");
        assert_eq!(rows[0]["at"], "2026-09-15T00:00:00+09:00", "{field}");
        assert_eq!(rows[1]["at"], "2026-09-15T23:59:59+09:00", "{field}");
    }
    assert_eq!(provider["usage_events"][0]["project"], "work/repo");
    assert_eq!(provider["usage_events"][0]["skill"], "review");
    assert_eq!(provider["usage_events"][0]["source"], "subagent");
    assert_eq!(provider["usage_events"][0]["model"], Value::Null);
    assert_eq!(
        provider["usage_events"][0]["tokens"]
            .as_object()
            .unwrap()
            .len(),
        7
    );
    assert_eq!(provider["turns"][0]["at_marks"], "end");
    assert_eq!(provider["turns"][0]["duration_ms"], 123);
    assert_eq!(provider["turns"][0]["human_wait_ms"], 23);
    assert_eq!(
        provider["sessions"],
        json!([{
            "session_id": "session", "date": "2026-09-15",
            "first_at": "2026-09-15T00:00:00+09:00",
            "last_at": "2026-09-15T23:59:59+09:00"
        }])
    );
    collection.provider = Provider::OpenCode;
    let raw = serde_json::to_value(EventsDto::new(
        &collection,
        EventWindow {
            start: date!(2026 - 09 - 15),
            end: date!(2026 - 09 - 16),
            offset: offset!(+09:00),
        },
    ))
    .unwrap();
    assert_eq!(raw["turns"][0]["at_marks"], "start");
    assert_eq!(raw["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(raw["sessions"][1]["date"], "2026-09-16");
}

#[test]
fn cost_estimate_is_null_while_tokens_are_unpriced() {
    let mut summary = crate::share::fixtures::sample_summary();
    summary.models[0].estimated = CostTally {
        priced_usd: 5.0,
        unpriced_volume: 0,
    };
    let value = serde_json::to_value(SummaryDto::new(&summary, offset!(+09:00))).unwrap();
    assert_eq!(
        value["cost"],
        json!({
            "estimated_usd": 5.0, "unpriced_tokens": 0
        })
    );
    summary.models[0].estimated = CostTally {
        priced_usd: 0.0,
        unpriced_volume: 1_000_000,
    };
    let value = serde_json::to_value(SummaryDto::new(&summary, offset!(+09:00))).unwrap();
    assert_eq!(
        value["cost"],
        json!({
            "estimated_usd": null, "unpriced_tokens": 1_000_000
        })
    );
}

#[test]
fn project_ids_units_and_large_integer_literals_are_preserved() {
    let mut summary = crate::share::fixtures::sample_summary();
    summary.projects[0].path = "work/agent-walker".to_owned();
    summary.total_usage.input_tokens = u64::MAX;
    summary.orchestration.time_by_level[0] = 2;
    let value = serde_json::to_value(SummaryDto::new(&summary, offset!(+09:00))).unwrap();
    assert_eq!(value["projects"][0]["id"], "work/agent-walker");
    assert_eq!(value["projects"][0]["label"], "agent-walker");
    assert_eq!(value["tokens"]["input_tokens"].as_u64(), Some(u64::MAX));
    assert_eq!(
        value["parallel"]["time_by_level"][0],
        json!({"level": "1", "active_ms": 2_000})
    );
    let plain = serde_json::to_value(SummaryDto::new(
        &crate::share::fixtures::sample_summary(),
        offset!(+09:00),
    ))
    .unwrap();
    let share_sum: f64 = plain["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["share"].as_f64().unwrap())
        .sum();
    assert!((share_sum - 1.0).abs() < 1e-9);
}

/// A parse cache written by another build can hold an instant that overflows
/// in the local offset; the export skips it instead of panicking.
#[test]
fn events_that_overflow_locally_are_left_out() {
    let edge = datetime!(9999-12-31 23:00 UTC);
    let mut collection = collection();
    collection.tool_events.push(ToolEvent {
        timestamp: Some(edge),
        session_id: Some("s1".to_owned()),
        tool_name: "Read".to_owned(),
        subagent_type: None,
        source_kind: SourceKind::Main,
    });
    collection.session_touches.push(SessionTouch {
        timestamp: edge,
        session_id: "s1".to_owned(),
    });
    let mut bytes = Vec::new();

    write_json(&mut bytes, &report(), &[collection], offset!(+09:00)).unwrap();

    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["providers"][0]["tools"], json!([]));
    assert_eq!(value["providers"][0]["sessions"], json!([]));
}

fn invisible_code_points() -> Vec<char> {
    [
        (0x7F, 0x9F),
        (0x061C, 0x061C),
        (0x200B, 0x200F),
        (0x2028, 0x202E),
        (0x2060, 0x2064),
        (0x2066, 0x2069),
        (0xFEFF, 0xFEFF),
        (0xE0000, 0xE007F),
    ]
    .into_iter()
    .flat_map(|(first, last)| first..=last)
    .map(|code| char::from_u32(code).unwrap())
    .collect()
}

fn assert_escaped(bytes: &[u8], code_points: &[char]) {
    let text = std::str::from_utf8(bytes).unwrap();
    for &ch in code_points {
        let code = u32::from(ch);
        let escape = ch
            .encode_utf16(&mut [0; 2])
            .iter()
            .map(|unit| format!("\\u{unit:04x}"))
            .collect::<Vec<_>>()
            .concat();
        assert!(!text.contains(ch), "raw U+{code:04X} in output");
        assert!(text.contains(&escape), "U+{code:04X} not escaped");
    }
}

#[test]
fn invisible_code_points_are_escaped_and_round_trip() {
    let hostile = invisible_code_points()
        .into_iter()
        .flat_map(|ch| [ch, 'x'])
        .collect::<String>();
    let value = std::collections::BTreeMap::from([(
        hostile.clone(),
        vec![hostile.clone(), format!("\u{1b}[2J\u{9b}31m{hostile}")],
    )]);
    let mut bytes = Vec::new();
    escape::to_writer_pretty(&mut bytes, &value).unwrap();
    assert_escaped(&bytes, &invisible_code_points());
    assert!(
        std::str::from_utf8(&bytes)
            .unwrap()
            .contains("\\udb40\\udc41")
    );
    assert!(!bytes.contains(&0x1B));
    assert_eq!(
        serde_json::from_slice::<std::collections::BTreeMap<String, Vec<String>>>(&bytes).unwrap(),
        value
    );
}

#[test]
fn ordinary_values_serialize_byte_identically_to_serde_json() {
    let neighbors = "~\u{a0}\u{61b}\u{61d}\u{200a}\u{2010}\u{2027}\u{202f}\u{205f}\u{2065}\u{206a}\u{fefe}\u{fffd}\u{dffff}\u{e0080}";
    let value = json!({
        "ascii": "quote \" slash / backslash \\ tab \t newline \n bell \u{7} esc \u{1b}",
        "text": "日本語 café 🦁",
        "neighbors": neighbors,
        neighbors: [neighbors],
        "numbers": [0, -3, u64::MAX, i64::MIN, 1.5, -0.25],
        "flags": [true, false, null],
        "empty": {"array": [], "object": {}},
        "nested": [[1, {"k": "v"}], []],
    });
    let mut bytes = Vec::new();
    escape::to_writer_pretty(&mut bytes, &value).unwrap();
    assert_eq!(bytes, serde_json::to_vec_pretty(&value).unwrap());
}

#[test]
fn export_bytes_match_plain_serde_json() {
    let report = report();
    let dto = ReportDto::new(&report, &[collection()], offset!(+09:00)).unwrap();
    let mut expected = serde_json::to_vec_pretty(&dto).unwrap();
    expected.push(b'\n');
    let mut bytes = Vec::new();
    write_json(&mut bytes, &report, &[collection()], offset!(+09:00)).unwrap();
    assert_eq!(bytes, expected);
}

#[test]
fn export_escapes_hostile_names_without_changing_values() {
    let mut report = report();
    report.providers[0].projects[0].path = "work/\u{9b}31mrepo\u{202e}gpj.exe".to_owned();
    report.providers[0].favorite_model =
        Some("model\u{200b}\u{2066}\u{7f}\u{feff}\u{2028}\u{2062}\u{e0041}".to_owned());
    let mut bytes = Vec::new();
    write_json(&mut bytes, &report, &[collection()], offset!(+09:00)).unwrap();
    assert_escaped(
        &bytes,
        &[
            '\u{9b}',
            '\u{202e}',
            '\u{200b}',
            '\u{2066}',
            '\u{7f}',
            '\u{feff}',
            '\u{2028}',
            '\u{2062}',
            '\u{e0041}',
        ],
    );
    let dto = ReportDto::new(&report, &[collection()], offset!(+09:00)).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        serde_json::to_value(&dto).unwrap()
    );
}
