use super::fixtures::sample_summary;
use super::{REPO_URL, ShareCard, Variant, render_png, svg};

#[test]
fn caption_includes_headline_and_repo() {
    let card = ShareCard::from_summary(&sample_summary(), Variant::Summary);
    let caption = card.caption();
    assert!(caption.contains("90 days"));
    assert!(caption.contains("tokens"));
    assert!(caption.contains("15 turns ran 20m+ unattended"));
    assert!(caption.contains(REPO_URL));
}

#[test]
fn both_variants_render_with_hourly_and_completion() {
    let summary = sample_summary();
    let full = ShareCard::from_summary(&summary, Variant::Full);
    let summary_card = ShareCard::from_summary(&summary, Variant::Summary);
    let svg_text = svg(&summary_card);
    assert!(svg_text.contains("BY HOUR"));
    assert!(svg_text.contains("COMPLETION"));
    assert!(svg_text.contains("Project A"));
    assert!(render_png(&full).is_ok());
    assert!(render_png(&summary_card).is_ok());
}
