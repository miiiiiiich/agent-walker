use ratatui::prelude::Color;

use crate::model::Provider;

pub(super) const ACCENT: Color = Color::Rgb(226, 178, 92);
pub(super) const HOT: Color = Color::Rgb(219, 105, 84);
pub(super) const GOLD: Color = Color::Rgb(239, 199, 104);
pub(super) const GREEN: Color = Color::Rgb(104, 211, 145);
pub(super) const BLUE: Color = Color::Rgb(132, 167, 255);
pub(super) const PURPLE: Color = Color::Rgb(186, 148, 255);
pub(super) const TEAL: Color = Color::Rgb(99, 214, 210);
pub(super) const MUTED: Color = Color::Rgb(140, 145, 150);
pub(super) const DIM: Color = Color::Rgb(70, 75, 80);
pub(super) const FAINT: Color = Color::Rgb(45, 49, 52);
pub(super) const TEXT: Color = Color::Rgb(238, 237, 230);
pub(super) const BLACK: Color = Color::Rgb(12, 12, 12);

// GitHub dark-theme contribution-graph greens, plus the empty-cell shade.
pub(super) const HEAT_RAMP: [Color; 4] = [
    Color::Rgb(14, 68, 41),
    Color::Rgb(0, 109, 50),
    Color::Rgb(38, 166, 65),
    Color::Rgb(57, 211, 83),
];
pub(super) const HEAT_ZERO: Color = Color::Rgb(33, 38, 45);

pub(super) fn provider_color(provider: Provider) -> Color {
    match provider {
        Provider::Claude => HOT,
        Provider::Codex => GREEN,
        Provider::Agy => BLUE,
        Provider::Combined => GOLD,
    }
}

pub(super) fn model_color(index: usize) -> Color {
    match index % 6 {
        0 => BLUE,
        1 => GREEN,
        2 => GOLD,
        3 => HOT,
        4 => PURPLE,
        _ => TEAL,
    }
}
