use anyhow::{Context, Result};

use super::card::ShareCard;
use super::svg::svg;

/// Rasterize the card SVG at 2x. Pure-Rust, no network; uses locally
/// installed fonts via fontdb.
pub(crate) fn render_pixmap(card: &ShareCard) -> Result<resvg::tiny_skia::Pixmap> {
    use resvg::tiny_skia;
    use resvg::usvg;

    let svg = svg(card);
    let mut options = usvg::Options::default();
    options.fontdb_mut().load_system_fonts();
    let tree = usvg::Tree::from_str(&svg, &options).context("parse generated SVG")?;

    let scale = 2.0_f32;
    let size = tree.size();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "Card dimensions are fixed and small."
    )]
    let (width, height) = (
        (size.width() * scale) as u32,
        (size.height() * scale) as u32,
    );
    let mut pixmap = tiny_skia::Pixmap::new(width, height).context("allocate pixmap for card")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap)
}

/// Card as PNG bytes.
pub fn render_png(card: &ShareCard) -> Result<Vec<u8>> {
    render_pixmap(card)?.encode_png().context("encode card PNG")
}
