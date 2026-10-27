use std::borrow::Cow;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::card::ShareCard;
use super::raster::{render_pixmap, render_png};
use crate::owned_file;

pub fn default_save_path() -> PathBuf {
    crate::paths::downloads_dir()
        .unwrap_or_default()
        .join("agent-walker.png")
}

/// The TUI saves to a fixed name in Downloads without asking, so whatever sits
/// at that name is replaced rather than written through. `--share <path>`
/// keeps plain overwrite semantics: the user named that path.
pub fn save(card: &ShareCard, path: &Path) -> Result<()> {
    owned_file::write_replacing(path, &render_png(card)?, owned_file::SHARED_MODE)
        .with_context(|| format!("write card to {}", path.display()))
}

/// The card is opaque, so premultiplied RGBA equals straight RGBA.
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

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::share::fixtures::sample_summary;

    #[test]
    fn save_replaces_a_planted_symlink_instead_of_writing_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join(".zshrc");
        fs::write(&victim, b"export PATH=/usr/bin\n").unwrap();
        let path = dir.path().join("agent-walker.png");
        symlink(&victim, &path).unwrap();

        let card = ShareCard::from_summary(&sample_summary());
        save(&card, &path).unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"export PATH=/usr/bin\n");
        assert!(fs::symlink_metadata(&path).unwrap().is_file());
        assert!(fs::read(&path).unwrap().starts_with(b"\x89PNG"));
    }
}
