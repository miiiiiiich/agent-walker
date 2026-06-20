//! Shareable stats card: render a 16:9 terminal-style summary as an SVG,
//! rasterize it to PNG locally (no network), and produce a text caption.
//! Used by the in-app share modal and the `--share` flag.

mod actions;
mod badge_art;
mod card;
mod raster;
mod svg;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod tests;

pub use actions::{copy_image, default_save_path, save};
pub use card::ShareCard;
pub use raster::render_png;
pub use svg::svg;

pub(crate) const REPO_URL: &str = "github.com/miiiiiiich/agent-walker";
