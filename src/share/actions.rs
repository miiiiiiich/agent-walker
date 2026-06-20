use std::borrow::Cow;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::card::ShareCard;
use super::raster::{render_pixmap, render_png};

/// Default path for a saved card: `~/Downloads/agent-walker.png`, falling back
/// to the home directory if there's no Downloads folder.
pub fn default_save_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let downloads = home.join("Downloads");
    let dir = if downloads.is_dir() { downloads } else { home };
    dir.join("agent-walker.png")
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
