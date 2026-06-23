use super::fixtures::sample_summary;
use super::svg::svg;
use super::{REPO_URL, ShareCard, badge_art, render_png};
use crate::model::ToolStat;

/// The 24 codename animals (matches `codename::GRID`); Chick carries no badge.
const ANIMALS: [&str; 24] = [
    "Hound",
    "Fox",
    "Doberman",
    "Lion",
    "Octopus",
    "Wolf",
    "Orca",
    "Hawk",
    "Raven",
    "Puma",
    "Whale",
    "Swallow",
    "Scorpion",
    "Piranha",
    "Bear",
    "Gull",
    "Cat",
    "Kangaroo",
    "Eel",
    "Deer",
    "Ant",
    "Firefly",
    "Butterfly",
    "Bee",
];

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
    for animal in ANIMALS {
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
    assert!(badge_art::badge_inner("Chick").is_none());
    assert!(badge_art::badge_inner("Nope").is_none());
}

/// Every bundled badge must actually rasterize — resvg accepts the geometry and
/// paints visible pixels — not merely pass the path-only check. A malformed path
/// would render blank as the share-card watermark.
#[test]
fn every_badge_rasterizes() {
    use resvg::{tiny_skia, usvg};
    for animal in ANIMALS {
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
fn card_codename_uses_style_source() {
    // The shared card's codename must match the tab badge: a provider tab card
    // takes its orchestration column from the Total summary, not the provider
    // alone. The combined view is tooling-heavy + parallel (→ Apex); the tab on
    // its own has no tooling (→ Parallel), so the two must differ.
    let window = crate::codename::CODENAME_WINDOW_DAYS as u64;
    let mut combined = sample_summary();
    combined.tools = vec![
        ToolStat {
            name: "Bash".to_owned(),
            calls: 900,
        },
        ToolStat {
            name: "Agent".to_owned(),
            calls: 60,
        },
        ToolStat {
            name: "Skill".to_owned(),
            calls: 40,
        },
    ];
    combined.recent_window_volume = 800_000_000 * window; // R1
    combined.recent_window_active_days = 29;

    let mut tab = combined.clone();
    tab.tools = vec![ToolStat {
        name: "Bash".to_owned(),
        calls: 500,
    }]; // no tooling on its own → Parallel column
    tab.recent_window_volume = 250_000_000 * window; // R3

    let styled = ShareCard::from_summary_styled(&tab, &combined);
    let alone = ShareCard::from_summary(&tab);
    assert_ne!(styled.codename, alone.codename);
    // R3 × Apex (inherited from the Total summary) = Doberman.
    assert!(
        styled.codename.contains("Doberman"),
        "expected R3 Apex Doberman, got {}",
        styled.codename
    );
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
