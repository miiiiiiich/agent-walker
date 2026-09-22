use super::fixtures::sample_summary;
use super::svg::{
    CELL_GUTTER, CONTEXT_W, HERO_GUTTER, HERO_PITCH, SESSIONS_W, TURNS_W, number_markup, svg,
};
use super::{REPO_URL, ShareCard, badge_art, render_png};

fn animals() -> Vec<&'static str> {
    crate::codename::all_animals().collect()
}

/// The watermark embeds bundled badge SVGs as raw XML, so they must stay
/// path-only — no script/handler/external-ref vectors can sneak in via a
/// regenerated asset.
#[test]
fn bundled_badges_are_path_only() {
    const FORBIDDEN: [&str; 13] = [
        "<script",
        "<foreignobject",
        "<image",
        "<use",
        "<style",
        "<a",
        "href",
        "xlink",
        "javascript:",
        "onload",
        "onclick",
        "onmouse",
        "onerror",
    ];
    for animal in animals() {
        let art =
            badge_art::badge_inner(animal).unwrap_or_else(|| panic!("missing badge: {animal}"));
        let lower = art.to_ascii_lowercase();
        assert!(lower.contains("<path"), "{animal}: no <path>");
        for token in FORBIDDEN {
            assert!(
                !lower.contains(token),
                "{animal}: forbidden token {token:?}"
            );
        }
    }
    assert!(badge_art::badge_inner("Nope").is_none());
}

/// Every bundled badge must actually rasterize — resvg accepts the geometry and
/// paints visible pixels — not merely pass the path-only check. A malformed path
/// would render blank as the share-card watermark.
#[test]
fn every_badge_rasterizes() {
    use resvg::{tiny_skia, usvg};
    for animal in animals() {
        let art =
            badge_art::badge_inner(animal).unwrap_or_else(|| panic!("missing badge: {animal}"));
        let doc = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">{}</svg>"#,
            art.replace("currentColor", "#000000")
        );
        let tree = usvg::Tree::from_str(&doc, &usvg::Options::default())
            .unwrap_or_else(|error| panic!("{animal} badge does not parse: {error}"));
        let scale = 256.0 / 1024.0;
        let mut pixmap = tiny_skia::Pixmap::new(256, 256).expect("allocate pixmap");
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        let painted = pixmap
            .pixels()
            .iter()
            .filter(|pixel| pixel.alpha() > 0)
            .count();
        assert!(painted > 0, "{animal} badge rendered blank");
    }
}

#[test]
fn unpriced_cost_renders_as_dash_not_zero() {
    let mut summary = sample_summary();
    let usage = crate::model::TokenUsage {
        input_tokens: 1_000_000,
        ..crate::model::TokenUsage::default()
    };
    summary.model_daily.push(crate::model::ModelDailyStat {
        date: summary.period_end,
        model: "model-nobody-priced".to_owned(),
        usage: usage.clone(),
        unreported_usage: usage,
        reported_cost_usd: None,
    });
    let card = ShareCard::from_summary(&summary);
    assert_eq!(card.cost, None);
    let rendered = svg(&card);
    assert!(rendered.contains("—"), "{rendered}");
    assert!(!rendered.contains("$0"), "{rendered}");
    let caption = card.caption();
    assert!(!caption.contains("API-equivalent"), "{caption}");
    assert!(!caption.contains("$0"), "{caption}");
}

#[test]
fn cached_share_rides_context_group_and_caption() {
    let card = ShareCard::from_summary(&sample_summary());
    assert_eq!(card.cached.as_deref(), Some("95%"));
    assert!(svg(&card).contains(">95%</text>"));
    assert!(card.caption().contains("· 95% cached"));

    let mut summary = sample_summary();
    summary.context = None;
    let card = ShareCard::from_summary(&summary);
    assert_eq!(card.cached, None);
    assert!(!svg(&card).contains("95%"));
    assert!(!card.caption().contains("cached"));

    let mut summary = sample_summary();
    summary.period_days = 7;
    assert_eq!(
        ShareCard::from_summary(&summary).cached.as_deref(),
        Some("95%")
    );
}

#[test]
fn x_weight_counts_whitespace_and_flat_urls() {
    assert_eq!(super::card::x_weight("a b"), 3);
    assert_eq!(super::card::x_weight("a\n\nb"), 4);
    assert_eq!(
        super::card::x_weight(
            "see https://github.com/miiiiiiich/agent-walker/releases/tag/v0.14.0"
        ),
        4 + 23
    );
    assert_eq!(super::card::x_weight("日本"), 4);
    assert_eq!(super::card::x_weight("a — b"), 5);
}

#[test]
fn caption_and_header_fit_their_budgets() {
    let mut summary = sample_summary();
    summary.total_usage.input_tokens = u64::MAX;
    summary.model_daily.push(crate::model::ModelDailyStat {
        date: summary.period_end,
        model: "claude-opus-4-8".to_owned(),
        usage: crate::model::TokenUsage::default(),
        unreported_usage: crate::model::TokenUsage::default(),
        reported_cost_usd: Some(1.0e18),
    });
    let card = ShareCard::from_summary(&summary);
    let caption = card.caption();
    assert!(super::card::x_weight(&caption) <= 280, "{caption}");
    // Saturated values shrink to their header column instead of running
    // into the neighbouring stat or the codename.
    let heroes = hero_numbers(&svg(&card));
    assert_eq!(heroes.len(), 3);
    for (text, size) in &heroes {
        assert!(advance(text, *size) <= hero_width(), "{text} at {size}px");
    }
    assert!(heroes[0].1 < 32.0 && heroes[2].1 < 32.0, "{heroes:?}");
    render_png(&card).expect("poisoned card must rasterize");
    assert!(
        hero_numbers(&svg(&ShareCard::from_summary(&sample_summary())))
            .iter()
            .all(|(_, size)| (*size - 32.0).abs() < f64::EPSILON)
    );

    let card = ShareCard::from_summary(&sample_summary());
    assert!(card.caption().contains("95% cached"));
    assert!(super::card::x_weight(&card.caption()) <= 280);
}

#[test]
fn caption_includes_headline_and_repo() {
    let card = ShareCard::from_summary(&sample_summary());
    let caption = card.caption();
    assert!(caption.contains("30 days"));
    assert!(caption.contains("tokens"));
    assert!(caption.contains("15 turns ran 20m+"));
    assert!(caption.contains(REPO_URL));
}

#[test]
fn card_rank_badge_reflects_own_volume() {
    let mut summary = sample_summary();
    summary.recent_window_volume = 250_000_000 * u64::from(summary.period_days);
    summary.recent_window_active_days = 29;

    let card = ShareCard::from_summary(&summary);
    assert!(
        card.codename.contains("Octopus"),
        "expected A-band Octopus, got {}",
        card.codename
    );
    assert_eq!(card.rank, crate::codename::Rank::A);
    assert!(card.caption().contains("Rank A"));
    let svg_text = svg(&card);
    assert!(svg_text.contains("rank-badge"), "rank badge missing");
    assert!(svg_text.contains(">RANK A</text>"), "badge label missing");
    assert!(svg_text.contains("#6b9bd8"), "A-rank 冠位 blue missing");
    assert!(
        !svg_text.contains("CODENAME"),
        "the CODENAME label is retired — the badge owns that slot"
    );
    render_png(&card).expect("ranked card must rasterize");

    let unranked = ShareCard::from_summary(&sample_summary());
    assert_eq!(unranked.rank, crate::codename::Rank::Unranked);
    assert!(!unranked.caption().contains("Rank"));
    assert!(!svg(&unranked).contains("rank-badge"));
}

#[test]
fn rank_badge_variants_cover_width_and_ink_lift() {
    let card_at = |tokens_per_day: u64| {
        let mut summary = sample_summary();
        summary.recent_window_volume = tokens_per_day * u64::from(summary.period_days);
        summary.recent_window_active_days = 29;
        ShareCard::from_summary(&summary)
    };

    let ss = svg(&card_at(800_000_000));
    assert!(ss.contains(">RANK SS</text>"));
    assert!(ss.contains("width=\"114\""), "SS pill width");
    assert!(ss.contains("#a678f0"), "SS 濃紫 missing");

    let e = svg(&card_at(5_000_000));
    assert!(e.contains(">RANK E</text>"));
    assert!(e.contains("#7a8088"), "E ink lift missing");
    let (r, g, b) = crate::codename::Rank::E
        .color_rgb()
        .expect("E has a canonical colour");
    let raw_ink = format!("#{r:02x}{g:02x}{b:02x}");
    assert!(!e.contains(&raw_ink), "raw ink must not reach the card");
}

#[test]
fn card_renders_with_charts_and_numbers() {
    let summary = sample_summary();
    let card = ShareCard::from_summary(&summary);
    let svg_text = svg(&card);
    assert!(svg_text.contains(">By hour<"));
    assert!(svg_text.contains(">Models<"));
    assert!(svg_text.contains(">Turns<"));
    assert!(!svg_text.contains("TASK TIME"));
    assert!(!svg_text.contains("orchestra"));
    assert!(render_png(&card).is_ok());
}

/// Width the layout assumes for `text` at `size` — the same 0.6em monospace
/// advance the card budgets with.
#[allow(clippy::cast_precision_loss, reason = "Test geometry.")]
fn advance(text: &str, size: f64) -> f64 {
    text.chars().count() as f64 * 0.6 * size
}

/// `(text, font-size)` of every `<text>` on the given baseline, tags stripped.
fn texts_at(svg_text: &str, y: u32) -> Vec<(String, f64)> {
    let marker = format!(r#" y="{y}" "#);
    svg_text
        .split("<text")
        .skip(1)
        .filter_map(|rest| {
            let (attrs, tail) = rest.split_once('>')?;
            if !format!("{attrs} ").contains(&marker) {
                return None;
            }
            let (_, size) = attrs.split_once(r#"font-size=""#)?;
            let (size, _) = size.split_once('"')?;
            let (body, _) = tail.split_once("</text>")?;
            let mut text = String::new();
            let mut in_tag = false;
            for c in body.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    _ if !in_tag => text.push(c),
                    _ => {}
                }
            }
            Some((text, size.parse().ok()?))
        })
        .collect()
}

fn hero_width() -> f64 {
    f64::from(HERO_PITCH - HERO_GUTTER)
}

fn hero_numbers(svg_text: &str) -> Vec<(String, f64)> {
    texts_at(svg_text, 96)
}

/// Every strip column is budgeted at full digit size; a saturated value must
/// shrink — units included — rather than run into the next cell.
#[test]
fn saturated_strip_values_stay_in_their_columns() {
    let widths: Vec<f64> = SESSIONS_W
        .iter()
        .chain(&TURNS_W)
        .chain(&CONTEXT_W)
        .map(|width| f64::from(width - CELL_GUTTER))
        .collect();
    let mut summary = sample_summary();
    summary.sessions = usize::MAX;
    summary.orchestration.peak_concurrency = usize::MAX;
    summary.orchestration.avg_concurrency = 1e300;
    if let Some(duration) = summary.completion_duration.as_mut() {
        duration.count = usize::MAX;
        duration.p50_ms = u64::MAX;
        duration.p90_ms = u64::MAX;
        duration.max_ms = u64::MAX;
    }
    if let Some(time) = summary.active_time.as_mut() {
        time.active_ms = u64::MAX;
        time.context_tokens = u64::MAX;
    }
    let card = ShareCard::from_summary(&summary);
    let rendered = svg(&card);
    let cells = texts_at(&rendered, 582);
    assert_eq!(cells.len(), widths.len(), "{cells:?}");
    for ((text, size), width) in cells.iter().zip(widths) {
        assert!(
            advance(text, *size) <= width,
            "{text} at {size}px > {width}"
        );
    }
    for unit in rendered.split(r#"<tspan font-size=""#).skip(1) {
        let (size, _) = unit.split_once('"').expect("closing quote");
        assert!(size.parse::<f64>().expect("unit size") <= 14.0);
    }
    for (text, size) in hero_numbers(&rendered) {
        assert!(advance(&text, size) <= hero_width(), "{text} at {size}px");
    }
    render_png(&card).expect("saturated card must rasterize");

    summary.orchestration.avg_concurrency = f64::NAN;
    render_png(&ShareCard::from_summary(&summary)).expect("NaN concurrency must rasterize");
}

/// Two labels that differ only near the front used to share one displayed
/// name once the row cut them to their tails.
#[test]
fn long_model_labels_stay_distinct_on_the_card() {
    let mut summary = sample_summary();
    let base = summary.models[0].clone();
    summary.models.clear();
    for name in [
        "gemini-2.5-pro-preview-05-06",
        "gemini-1.5-pro-preview-0514",
    ] {
        let mut model = base.clone();
        name.clone_into(&mut model.name);
        summary.models.push(model);
    }
    let card = ShareCard::from_summary(&summary);
    assert_eq!(card.model_count, 2);
    let svg_text = svg(&card);
    for (name, ..) in &card.models {
        assert_eq!(svg_text.matches(&format!(">{name}<")).count(), 1, "{name}");
    }
    assert_ne!(card.models[0].0, card.models[1].0);
}

#[test]
fn model_rows_end_on_the_shared_floor() {
    let track_bottoms = |svg_text: &str| -> Vec<u32> {
        let mut pieces: Vec<&str> = svg_text
            .split(r##"height="13" rx="6.5" fill="#262b33""##)
            .collect();
        pieces.pop(); // what follows the last track is not a track
        pieces
            .into_iter()
            .filter_map(|before| {
                let (_, y) = before.rsplit_once(r#" y=""#)?;
                let (y, _) = y.split_once('"')?;
                y.parse::<u32>().ok().map(|y| y + 13)
            })
            .collect()
    };
    let mut summary = sample_summary();
    let one = track_bottoms(&svg(&ShareCard::from_summary(&summary)));
    assert_eq!(one.len(), 1);
    assert!(one[0] <= 447);

    let base = summary.models[0].clone();
    for name in ["gpt-5.5", "gemini-3-pro", "grok-4", "claude-haiku-4-5"] {
        let mut model = base.clone();
        name.clone_into(&mut model.name);
        summary.models.push(model);
    }
    let four = track_bottoms(&svg(&ShareCard::from_summary(&summary)));
    assert_eq!(four.len(), 4, "the card draws at most four rows");
    assert_eq!(four[3], 447);
}

#[test]
fn header_carries_per_day_rates() {
    let card = ShareCard::from_summary(&sample_summary());
    assert_eq!(
        card.working,
        Some(("87h 00m".to_owned(), "2h 54m".to_owned()))
    );
    let svg_text = svg(&card);
    assert!(svg_text.contains(">Working time<"));
    assert!(svg_text.contains(">87h 00m<"));
    assert!(svg_text.contains(">2h 54m/day<"));
    assert!(svg_text.contains(&format!(">{}/day<", card.tokens_per_day)));

    let mut summary = sample_summary();
    summary.active_time = None;
    let card = ShareCard::from_summary(&summary);
    assert_eq!(card.working, None);
    assert_eq!(card.tokens_per_min, None);
    assert!(render_png(&card).is_ok());
}

#[test]
fn strip_groups_sessions_turns_and_context() {
    let card = ShareCard::from_summary(&sample_summary());
    assert_eq!(card.tokens_per_min.as_deref(), Some("606.0K"));
    let svg_text = svg(&card);
    for label in [">Sessions<", ">Turns<", ">Context<"] {
        assert!(svg_text.contains(label), "{label}");
    }
    for caption in [
        ">sessions<",
        ">avg parallel<",
        ">turns<",
        ">tokens/min<",
        ">cached<",
    ] {
        assert!(svg_text.contains(caption), "{caption}");
    }
    assert!(svg_text.contains(">42</text>"), "session count");
    assert!(svg_text.contains(">100</text>"), "turn count");
    assert!(svg_text.contains(">606.0K</text>"));
}

#[test]
fn durations_drop_the_space_and_shrink_units() {
    let unit = |text: &str| format!(r#"<tspan font-size="14.0" font-weight="600">{text}</tspan>"#);
    assert_eq!(
        number_markup("1h 15m", 22.0),
        format!("1{}15{}", unit("h"), unit("m"))
    );
    assert_eq!(number_markup("45s", 22.0), format!("45{}", unit("s")));
    assert_eq!(
        number_markup("1d 2h 3m", 22.0),
        format!("1{}2{}3{}", unit("d"), unit("h"), unit("m"))
    );
    // A number shrunk below unit size takes its units down with it.
    assert_eq!(
        number_markup("5m", 7.0),
        r#"5<tspan font-size="7.0" font-weight="600">m</tspan>"#
    );
    // Counts, rates, the unknown marker and unit-less words are not durations.
    for plain in ["1,204", "5.4M", "—", "inf", "abc"] {
        assert_eq!(number_markup(plain, 22.0), plain);
    }
    assert_eq!(number_markup("<b>", 22.0), "&lt;b&gt;");
}

#[test]
fn panels_caption_active_days_model_count_and_period() {
    let card = ShareCard::from_summary(&sample_summary());
    assert_eq!(card.model_count, 1);
    assert_eq!(
        card.period,
        ("2026-05-14".to_owned(), "2026-06-12".to_owned())
    );
    let svg_text = svg(&card);
    assert!(svg_text.contains(">30/30</tspan> active<"));
    assert!(svg_text.contains(">1</tspan> model<"));
    assert!(svg_text.contains("2026-05-14 – 2026-06-12"));
    assert!(svg_text.contains(">bunx <"));
    assert!(!svg_text.contains("github.com"), "the card carries no URL");

    // A dated and an undated id of one model print as one row and count once.
    let mut summary = sample_summary();
    let mut twin = summary.models[0].clone();
    twin.name = format!("{}-20260101", twin.name);
    summary.models.push(twin);
    let card = ShareCard::from_summary(&summary);
    assert_eq!(card.model_count, 1);
    assert_eq!(card.models.len(), 1);

    // The halves add up: two 60s outrank a single 100 once merged.
    let mut summary = sample_summary();
    let volume = |tokens: u64| crate::model::TokenUsage {
        input_tokens: tokens,
        ..crate::model::TokenUsage::default()
    };
    let base = summary.models[0].clone();
    summary.models.clear();
    for (name, tokens) in [
        ("gpt-5.5", 100),
        ("claude-opus-4-8", 60),
        ("claude-opus-4-8-20260101", 60),
    ] {
        let mut model = base.clone();
        name.clone_into(&mut model.name);
        model.usage = volume(tokens);
        summary.models.push(model);
    }
    summary.total_usage = volume(220);
    let card = ShareCard::from_summary(&summary);
    assert_eq!(card.model_count, 2);
    let rows: Vec<(&str, &str, String)> = card
        .models
        .iter()
        .map(|(name, share, ratio, _)| (name.as_str(), share.as_str(), format!("{ratio:.2}")))
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].1, rows[0].2.as_str()), ("54.5%", "1.00"));
    assert_eq!((rows[1].1, rows[1].2.as_str()), ("45.5%", "0.83"));
    assert_ne!(rows[0].0, rows[1].0);
}
