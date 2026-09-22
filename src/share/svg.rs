mod header;
mod panels;
mod strip;
mod text;

use std::fmt::Write as _;

use super::card::ShareCard;

use header::{draw_header, draw_watermark};
use panels::{draw_activity, draw_hourly, draw_models};
use strip::draw_bottom;
use text::xml_escape;

#[cfg(test)]
pub(super) use text::number_markup;

const C_TEXT: &str = "#eeede6";
const C_MUTED: &str = "#8c9196";
const C_DIM: &str = "#5c6675";
const C_BORDER: &str = "#26282e";
const C_HAIRLINE: &str = "#26282e";
const C_TRACK: &str = "#262b33";
const C_CARD_BG: &str = "#0a0a0c";
const C_PANEL_TOP: &str = "#17181d";
const C_PANEL_BOTTOM: &str = "#0f1014";
const C_GOLD: &str = "#efc768";
const C_BLUE: &str = "#84a7ff";
const C_HEAT_ZERO: &str = "#21262d";
const C_HEAT: [&str; 4] = ["#0e4429", "#006d32", "#26a641", "#39d353"];
const C_MODEL: [&str; 6] = [
    "#84a7ff", "#68d391", "#efc768", "#db6954", "#ba94ff", "#63d6d2",
];

const FONT: &str = "'SF Mono','Menlo','DejaVu Sans Mono','Consolas',monospace";

const W: u32 = 1200;
const H: u32 = 675;
const LX: u32 = 58; // content left edge
const RX: u32 = 1142; // content right edge

const ACT_X: u32 = 58;
const HRL_X: u32 = 242;
const HRL_W: u32 = 470;
const HRL_R: u32 = HRL_X + HRL_W;
const MOD_X: u32 = 746;
const MOD_W: u32 = 396;

const SEC_Y: u32 = 178; // section-label baseline

/// The three panels share one floor: bars, grass and model tracks all end on
/// `CHART_BASE`, and each panel's one-line caption sits on the hour-axis
/// baseline below it.
const CHART_BASE: u32 = 447;
const AXIS_Y: u32 = 467;

/// Monospace advance as a fraction of the font size — close enough to size a
/// column, and the only metric available without shaping the text.
const ADVANCE: f64 = 0.6;

pub(super) const HERO_PITCH: u32 = 200;
pub(super) const HERO_GUTTER: u32 = 10;

/// `short_model_name` caps labels at this length; at 19px it still clears the
/// share figure on the same line.
const MODEL_LABEL_MAX: usize = 24;

pub(super) const GROUP_GAP: u32 = 36;
pub(super) const SESSIONS_W: [u32; 4] = [80, 112, 72, 60];
pub(super) const TURNS_W: [u32; 5] = [88, 88, 88, 88, 104];
pub(super) const CONTEXT_W: [u32; 2] = [120, 112];
pub(super) const CELL_GUTTER: u32 = 8;

pub fn svg(card: &ShareCard) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" font-family="{FONT}">"#
    );
    let _ = write!(
        s,
        r#"<defs><linearGradient id="panel" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{C_PANEL_TOP}"/><stop offset="1" stop-color="{C_PANEL_BOTTOM}"/></linearGradient></defs>"#
    );

    let _ = write!(s, r#"<rect width="{W}" height="{H}" fill="{C_CARD_BG}"/>"#);
    let _ = write!(
        s,
        r#"<rect x="24" y="24" width="{}" height="{}" rx="20" fill="url(#panel)" stroke="{C_BORDER}" stroke-width="1.5"/>"#,
        W - 48,
        H - 48
    );

    draw_watermark(&mut s, card);

    draw_header(&mut s, card);
    draw_activity(&mut s, card);
    draw_hourly(&mut s, card);
    draw_models(&mut s, card);
    draw_bottom(&mut s, card);

    let _ = write!(
        s,
        r#"<text x="{LX}" y="{}" fill="{C_DIM}" font-size="17">bunx <tspan fill="{C_MUTED}" font-weight="700">agent-walker</tspan></text>"#,
        H - 38
    );
    let _ = write!(
        s,
        r#"<text x="{RX}" y="{}" fill="{C_DIM}" font-size="15" text-anchor="end">{} – {}</text>"#,
        H - 38,
        xml_escape(&card.period.0),
        xml_escape(&card.period.1)
    );

    s.push_str("</svg>");
    s
}

/// Mirror the TUI tint mapping so the card and dashboard agree.
fn ops_color(ops: &str) -> &'static str {
    match ops {
        "Aurora" => "#63d6d2",
        "Sol" => "#efc768",
        "Luna" => "#84a7ff",
        _ => "#ba94ff",
    }
}
