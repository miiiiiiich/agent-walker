use std::borrow::Cow;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::card::ShareCard;
use super::raster::{render_pixmap, render_png};

/// Default path for a saved card: `<home>/Downloads/agent-walker.png`, falling
/// back to the home directory if there's no Downloads folder. Resolves through
/// `paths::downloads_dir` so Windows and Unix share the same code path.
pub fn default_save_path() -> PathBuf {
    crate::paths::downloads_dir()
        .unwrap_or_default()
        .join("agent-walker.png")
}

/// Write the card PNG to `path`.
pub fn save(card: &ShareCard, path: &Path) -> Result<()> {
    std::fs::write(path, render_png(card)?)
        .with_context(|| format!("write card to {}", path.display()))
}

/// Copy the rendered card image to the system clipboard. The card is opaque,
/// so premultiplied RGBA equals straight RGBA.
#[allow(
    clippy::cast_possible_truncation,
    reason = "Pixmap dimensions are small."
)]
pub fn copy_image(card: &ShareCard) -> Result<()> {
    let pixmap = render_pixmap(card)?;
    let image = arboard::ImageData {
        width: pixmap.width() as usize,
        height: pixmap.height() as usize,
        bytes: Cow::Borrowed(pixmap.data()),
    };
    arboard::Clipboard::new()
        .context("open clipboard")?
        .set_image(image)
        .context("set clipboard image")
}
