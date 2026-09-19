use std::fmt::Write as _;

use crate::format::format_count;

use super::badge_art;
use super::card::ShareCard;

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

#[allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Flat SVG assembly; geometry is display-only."
)]
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

fn draw_watermark(s: &mut String, card: &ShareCard) {
    let Some(art) = badge_art::badge_inner(&card.animal) else {
        return;
    };
    // Resolve the silhouette's currentColor at embed time — robust regardless of
    // the renderer's currentColor support.
    let tinted = art.replace("currentColor", ops_color(&card.ops));
    let _ = write!(
        s,
        r#"<g transform="translate(34,30) scale(0.30)" fill-opacity="0.16">{tinted}</g>"#
    );
}

fn draw_header(s: &mut String, card: &ShareCard) {
    draw_rank_badge(s, card);
    let color = ops_color(&card.ops);
    let _ = write!(
        s,
        r#"<text x="{LX}" y="118" font-size="34" font-weight="800" letter-spacing="0.5"><tspan fill="{color}">{}</tspan><tspan fill="{C_TEXT}"> {}</tspan></text>"#,
        xml_escape(&card.ops),
        xml_escape(&card.animal)
    );

    // An unknown cost renders as "—" — never "$0", which would misread as free.
    let per_day = |value: &str| format!("{value}/day");
    let (working, working_per_day) = match &card.working {
        Some((total, rate)) => (total.clone(), Some(per_day(rate))),
        None => (dash(), None),
    };
    let hero = [
        (
            "Tokens",
            card.tokens.clone(),
            Some(per_day(&card.tokens_per_day)),
        ),
        ("Working time", working, working_per_day),
        (
            "Cost",
            card.cost.clone().unwrap_or_else(dash),
            card.cost_per_day.as_deref().map(per_day),
        ),
    ];
    for (index, (label, number, sub)) in hero.iter().enumerate() {
        let x = RX - (2 - u32::try_from(index).unwrap_or(0)) * HERO_PITCH;
        // Saturated (poisoned) token or cost values shrink to their column
        // instead of running into the neighbour or the codename.
        let size = fit_font(number, f64::from(HERO_PITCH - HERO_GUTTER), 32.0);
        let _ = write!(
            s,
            r#"<text x="{x}" y="60" text-anchor="end" font-size="15" font-weight="700" fill="{C_MUTED}">{label}</text>"#
        );
        let _ = write!(
            s,
            r#"<text x="{x}" y="96" text-anchor="end" font-size="{size:.1}" font-weight="800" fill="{C_TEXT}">{}</text>"#,
            xml_escape(number)
        );
        if let Some(sub) = sub {
            let size = fit_font(sub, f64::from(HERO_PITCH - HERO_GUTTER), 14.0);
            let _ = write!(
                s,
                r#"<text x="{x}" y="120" text-anchor="end" font-size="{size:.1}" fill="{C_MUTED}">{}</text>"#,
                xml_escape(sub)
            );
        }
    }

    let _ = write!(
        s,
        r#"<line x1="{LX}" y1="140" x2="{RX}" y2="140" stroke="{C_HAIRLINE}" stroke-width="1"/>"#
    );
}

fn draw_rank_badge(s: &mut String, card: &ShareCard) {
    let (Some(letters), Some((r, g, b))) = (card.rank.letters(), card.rank.display_rgb()) else {
        return;
    };
    let color = format!("#{r:02x}{g:02x}{b:02x}");
    let label = format!("RANK {letters}");
    let width = 44 + 10 * u32::try_from(label.len()).unwrap_or(7);
    let _ = write!(
        s,
        r#"<rect class="rank-badge" x="{LX}" y="48" width="{width}" height="28" rx="14" fill="{color}" fill-opacity="0.12" stroke="{color}" stroke-opacity="0.65" stroke-width="1.3"/>"#
    );
    let _ = write!(
        s,
        r#"<text x="{}" y="67" text-anchor="middle" font-size="14" font-weight="800" letter-spacing="2" fill="{color}">{label}</text>"#,
        LX + width / 2
    );
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "Grid geometry is display-only."
)]
fn draw_activity(s: &mut String, card: &ShareCard) {
    section(s, ACT_X, 0, "Activity", "");
    let size = 22_u32;
    let pitch = 27_u32;
    let grid_h = 7 * pitch - (pitch - size);
    let y0 = CHART_BASE - grid_h;
    let _ = write!(
        s,
        r#"<text x="{ACT_X}" y="{AXIS_Y}" font-size="15" fill="{C_MUTED}"><tspan fill="{C_TEXT}" font-weight="700">{}/{}</tspan> active</text>"#,
        card.active_days, card.period_days
    );
    for (col, week) in card.grass.cells.iter().enumerate() {
        for (row, level) in week.iter().enumerate() {
            let fill = match level {
                None => continue,
                Some(0) => C_HEAT_ZERO,
                Some(n) => C_HEAT[(n - 1).min(3)],
            };
            let x = ACT_X + u32::try_from(col).unwrap_or(0) * pitch;
            let y = y0 + u32::try_from(row).unwrap_or(0) * pitch;
            let _ = write!(
                s,
                r#"<rect x="{x}" y="{y}" width="{size}" height="{size}" rx="4" fill="{fill}"/>"#
            );
        }
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Chart geometry is display-only."
)]
fn draw_hourly(s: &mut String, card: &ShareCard) {
    let Some((heights, peak, _label)) = &card.hourly else {
        return;
    };
    section(s, HRL_X, HRL_R, "By hour", &format!("peak {peak:02}:00"));
    let max_h = 232_f64;
    let baseline = f64::from(CHART_BASE);
    let slot = f64::from(HRL_W) / 24.0;
    for (hour, height) in heights.iter().enumerate() {
        if *height <= 0.0 {
            continue;
        }
        let bar_h = (max_h * height).max(3.0);
        let x = f64::from(HRL_X) + slot * hour as f64;
        let fill = if hour == *peak { C_GOLD } else { C_BLUE };
        let _ = write!(
            s,
            r#"<rect x="{x:.1}" y="{:.1}" width="{:.1}" height="{bar_h:.1}" rx="3" fill="{fill}"/>"#,
            baseline - bar_h,
            slot - 5.0
        );
    }
    let _ = write!(
        s,
        r#"<line x1="{HRL_X}" y1="{:.0}" x2="{HRL_R}" y2="{:.0}" stroke="{C_HAIRLINE}" stroke-width="1"/>"#,
        baseline + 4.0,
        baseline + 4.0
    );
    for (hour, anchor) in [
        (0_u32, "start"),
        (6, "middle"),
        (12, "middle"),
        (18, "middle"),
        (24, "end"),
    ] {
        let x = f64::from(HRL_X) + slot * f64::from(hour);
        let _ = write!(
            s,
            r#"<text x="{x:.0}" y="{AXIS_Y}" fill="{C_DIM}" font-size="13" text-anchor="{anchor}">{hour:02}</text>"#
        );
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Chart geometry is display-only."
)]
fn draw_models(s: &mut String, card: &ShareCard) {
    section(s, MOD_X, 0, "Models", "");
    if card.models.is_empty() {
        return;
    }
    let noun = if card.model_count == 1 {
        "model"
    } else {
        "models"
    };
    let _ = write!(
        s,
        r#"<text x="{RX}" y="{AXIS_Y}" font-size="15" fill="{C_MUTED}" text-anchor="end"><tspan fill="{C_TEXT}" font-weight="700">{}</tspan> {noun}</text>"#,
        format_count(card.model_count)
    );
    let rows = card.models.len().min(4);
    let pitch = 56_u32;
    // A row is its label line (26px) over a 13px track; four of them end on
    // CHART_BASE, fewer are centred in that same span.
    let row_h = 39_u32;
    let full_h = 3 * pitch + row_h;
    let block_h = (u32::try_from(rows).unwrap_or(1) - 1) * pitch + row_h;
    let mut ry = CHART_BASE - full_h + (full_h - block_h) / 2;
    let track_x = MOD_X;
    let track_w = MOD_W;
    for (index, (name, share, ratio, _formatted)) in card.models.iter().take(4).enumerate() {
        let grad = C_MODEL[index % C_MODEL.len()];
        let _ = write!(
            s,
            r#"<text x="{MOD_X}" y="{}" fill="{C_TEXT}" font-size="19">{}</text>"#,
            ry + 17,
            // Rows are merged by label, so the label prints whole: a shorter
            // cut let two different labels collapse into one displayed name.
            xml_escape(&truncate_tail(name, MODEL_LABEL_MAX))
        );
        let _ = write!(
            s,
            r#"<text x="{RX}" y="{}" fill="{C_MUTED}" font-size="17" text-anchor="end">{}</text>"#,
            ry + 17,
            xml_escape(share)
        );
        let _ = write!(
            s,
            r#"<rect x="{track_x}" y="{}" width="{track_w}" height="13" rx="6.5" fill="{C_TRACK}"/>"#,
            ry + 26
        );
        // Clamp untrusted ratio: a malformed >1 / non-finite value must not draw
        // a bar past the track.
        let safe_ratio = if ratio.is_finite() {
            ratio.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let fill_w = ((f64::from(track_w) * safe_ratio).round().max(6.0) as u32).min(track_w);
        let _ = write!(
            s,
            r#"<rect x="{track_x}" y="{}" width="{fill_w}" height="13" rx="6.5" fill="{grad}"/>"#,
            ry + 26
        );
        ry += pitch;
    }
}

/// Bottom strip — three groups told apart by space alone. Columns are sized to
/// what they hold rather than split evenly: even slots either starve the wide
/// duration cells or force the captions below legible size on a shrunk card.
fn draw_bottom(s: &mut String, card: &ShareCard) {
    let par_avg = if card.avg_concurrency > 0.0 || card.parallel.is_some() {
        format!("{:.1}", card.avg_concurrency)
    } else {
        dash()
    };
    let (par_four, par_peak) = match &card.parallel {
        Some((four_plus, peak)) => (format!("{four_plus}%"), format_count(*peak)),
        None => (dash(), dash()),
    };
    let (turns, p50, p90, max, unattended) = match &card.completion {
        Some((_, un, count, p50, p90, max)) => (
            format_count(*count),
            p50.clone(),
            p90.clone(),
            max.clone(),
            format_count(*un),
        ),
        None => (dash(), dash(), dash(), dash(), dash()),
    };
    let accent = ops_color(&card.ops);

    let _ = write!(
        s,
        r#"<line x1="{LX}" y1="520" x2="{RX}" y2="520" stroke="{C_HAIRLINE}" stroke-width="1"/>"#
    );
    let mut x = LX;
    x = group(
        s,
        x,
        "Sessions",
        &SESSIONS_W,
        &[
            (format_count(card.sessions), "sessions", C_TEXT),
            (par_avg, "avg parallel", C_TEXT),
            (par_four, "4+", C_TEXT),
            (par_peak, "peak", C_TEXT),
        ],
    ) + GROUP_GAP;
    x = group(
        s,
        x,
        "Turns",
        &TURNS_W,
        &[
            (turns, "turns", C_TEXT),
            (p50, "p50", C_TEXT),
            (p90, "p90", C_TEXT),
            (max, "max", C_TEXT),
            (unattended, "20m+ runs", accent),
        ],
    ) + GROUP_GAP;
    group(
        s,
        x,
        "Context",
        &CONTEXT_W,
        &[
            (
                card.tokens_per_min.clone().unwrap_or_else(dash),
                "tokens/min",
                C_TEXT,
            ),
            (card.cached.clone().unwrap_or_else(dash), "cached", C_TEXT),
        ],
    );
}

fn dash() -> String {
    "—".to_owned()
}

fn group(
    s: &mut String,
    x0: u32,
    label: &str,
    widths: &[u32],
    cells: &[(String, &str, &str)],
) -> u32 {
    let _ = write!(
        s,
        r#"<text x="{x0}" y="548" fill="{C_MUTED}" font-size="15" font-weight="500">{label}</text>"#
    );
    let mut x = x0;
    for ((number, caption, color), width) in cells.iter().zip(widths) {
        let compact: String = number.chars().filter(|c| *c != ' ').collect();
        let size = fit_font(&compact, f64::from(width - CELL_GUTTER), 22.0);
        let _ = write!(
            s,
            r#"<text x="{x}" y="582" fill="{color}" font-size="{size:.1}" font-weight="700">{}</text>"#,
            number_markup(number, size)
        );
        let _ = write!(
            s,
            r#"<text x="{x}" y="602" fill="{C_MUTED}" font-size="14">{caption}</text>"#
        );
        x += width;
    }
    x
}

/// Durations lose their space and carry their units at caption size on the
/// same baseline ("1h15m"), so the longest cell stops dwarfing its neighbours.
/// Anything that is not plain digits-and-units is escaped and left alone.
pub(super) fn number_markup(text: &str, size: f64) -> String {
    let is_duration = text.chars().any(|c| c.is_ascii_digit())
        && text.chars().any(|c| c.is_ascii_lowercase())
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase() || c == ' ');
    if !is_duration {
        return xml_escape(text);
    }
    // Units never outgrow the digits: a number shrunk to fit its column was
    // sized as if every character shrank with it.
    let open = format!(
        r#"<tspan font-size="{:.1}" font-weight="600">"#,
        size.min(14.0)
    );
    let mut out = String::new();
    let mut in_unit = false;
    for c in text.chars().filter(|c| *c != ' ') {
        if c.is_ascii_lowercase() != in_unit {
            in_unit = !in_unit;
            out.push_str(if in_unit { &open } else { "</tspan>" });
        }
        out.push(c);
    }
    if in_unit {
        out.push_str("</tspan>");
    }
    out
}

#[allow(
    clippy::cast_precision_loss,
    reason = "Layout geometry is display-only."
)]
fn fit_font(text: &str, width: f64, base: f64) -> f64 {
    let chars = text.chars().count().max(1) as f64;
    // Floored to the precision it is printed at: rounding up would draw the
    // text a hair wider than the column it was sized for.
    (base.min(width / (chars * ADVANCE)) * 10.0).floor() / 10.0
}

fn section(s: &mut String, x: u32, right: u32, label: &str, annotation: &str) {
    let _ = write!(
        s,
        r#"<text x="{x}" y="{SEC_Y}" fill="{C_MUTED}" font-size="17" font-weight="700">{label}</text>"#
    );
    if !annotation.is_empty() && right > 0 {
        let _ = write!(
            s,
            r#"<text x="{right}" y="{SEC_Y}" fill="{C_DIM}" font-size="14" text-anchor="end">{}</text>"#,
            xml_escape(annotation)
        );
    }
}

/// Model names come from logs (untrusted), so escape quotes too — otherwise a
/// `"` in a value would break out of an attribute and fail SVG parsing.
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn truncate_tail(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let tail: String = text.chars().skip(count - (max - 1)).collect();
    format!("…{tail}")
}
