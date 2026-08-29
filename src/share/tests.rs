use super::fixtures::sample_summary;
use super::svg::svg;
use super::{REPO_URL, ShareCard, badge_art, render_png};

/// The 24 codename animals, straight from the ladder — badge assets must cover
/// exactly this set (Ant is the unranked floor and has one).
fn animals() -> Vec<&'static str> {
    crate::codename::all_animals().collect()
}

/// The watermark embeds bundled badge SVGs as raw XML, so they must stay
/// path-only — no script/handler/external-ref vectors can sneak in via a
/// regenerated asset. (Safety is enforced here, not just asserted in docs.)
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
        // The silhouette's native space is 1024×1024 (see the badge `<g>` transform).
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

/// A card whose window holds tokens with no known price shows "—" instead of
/// an undercounted "$0" — and its caption drops the cost stat entirely.
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

/// The cache-reuse share rides the header stat line and the caption as one
/// number; a summary without context data leaves both untouched.
#[test]
fn cached_share_rides_header_and_caption() {
    let card = ShareCard::from_summary(&sample_summary());
    assert_eq!(card.cached.as_deref(), Some("95% cached"));
    assert!(svg(&card).contains("api-equiv   ·   95% cached"));
    assert!(card.caption().contains("· 95% cached"));

    let mut summary = sample_summary();
    summary.context = None;
    let card = ShareCard::from_summary(&summary);
    assert_eq!(card.cached, None);
    assert!(!svg(&card).contains("cached"));
    assert!(!card.caption().contains("cached"));
}

/// The caption fits X's 280-weight limit by dropping optional stats in
/// priority order; a saturated stat line on the SVG yields the cache share
/// before it can reach the codename.
#[test]
fn caption_and_header_fit_their_budgets() {
    let mut summary = sample_summary();
    summary.total_usage.input_tokens = u64::MAX;
    // A provider-reported cost this large makes the stat line overrun its
    // budget on its own, so the cache share has to yield.
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
    // The share was dropped from the SVG stat line, the line itself stays.
    let rendered = svg(&card);
    assert!(rendered.contains("tokens   ·"), "{rendered}");
    assert!(
        !rendered.contains("api-equiv   ·   95% cached"),
        "stat line should yield the share"
    );

    // The everyday fixture keeps everything.
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
    // The card ranks on the summary's own 30-day throughput: 250M/day sits at
    // the bottom of the A band → Octopus. The rank pill carries the 冠位
    // colour for A (blue) and the caption carries the letters — never a step
    // counter.
    let window = crate::codename::CODENAME_WINDOW_DAYS as u64;
    let mut summary = sample_summary();
    summary.recent_window_volume = 250_000_000 * window;
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

    // The unranked fixture (≈700K tokens/day) leaves the badge slot empty.
    let unranked = ShareCard::from_summary(&sample_summary());
    assert_eq!(unranked.rank, crate::codename::Rank::Unranked);
    assert!(!unranked.caption().contains("Rank"));
    assert!(!svg(&unranked).contains("rank-badge"));
}

#[test]
fn rank_badge_variants_cover_width_and_ink_lift() {
    let window = crate::codename::CODENAME_WINDOW_DAYS as u64;
    let card_at = |tokens_per_day: u64| {
        let mut summary = sample_summary();
        summary.recent_window_volume = tokens_per_day * window;
        summary.recent_window_active_days = 29;
        ShareCard::from_summary(&summary)
    };

    // SS is one glyph longer → the pill widens.
    let ss = svg(&card_at(800_000_000));
    assert!(ss.contains(">RANK SS</text>"));
    assert!(ss.contains("width=\"114\""), "SS pill width");
    assert!(ss.contains("#a678f0"), "SS 濃紫 missing");

    // E (墨) renders with the lifted display shade, never the raw ink.
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
    assert!(svg_text.contains("BY HOUR"));
    assert!(svg_text.contains("MODELS"));
    assert!(svg_text.contains("TASK TIME"));
    // Privacy-safe: no repo names ever leak onto the card.
    assert!(!svg_text.contains("orchestra"));
    assert!(render_png(&card).is_ok());
}
