use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::card::ShareCard;
use super::raster::{render_pixmap, render_png};

/// Default path for a saved card: `~/agent-walker.png`, else the working dir.
pub fn default_save_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("agent-walker.png")
}

/// Write the card PNG to `path`.
pub fn save(card: &ShareCard, path: &Path) -> Result<()> {
    std::fs::write(path, render_png(card)?)
        .with_context(|| format!("write card to {}", path.display()))
}

/// Copy the text caption to the system clipboard.
pub fn copy_caption(card: &ShareCard) -> Result<()> {
    arboard::Clipboard::new()
        .context("open clipboard")?
        .set_text(card.caption())
        .context("set clipboard text")
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

/// Share to X: copy the card image to the clipboard, then open a compose
/// window pre-filled with the caption. X intents are text-only, so the user
/// pastes the image (already on the clipboard) into the composer.
pub fn share_to_x(card: &ShareCard) -> Result<()> {
    copy_image(card)?;
    let url = format!(
        "https://x.com/intent/post?text={}",
        percent_encode(&card.caption())
    );
    open::that(url).context("open X compose window")
}

/// Minimal RFC-3986 percent-encoding for a query value.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}
