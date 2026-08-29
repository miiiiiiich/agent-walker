use std::fmt::Write as _;

use super::REPO_URL;
use super::badge_art;
use super::card::ShareCard;

// Card palette (matches the TUI brand).
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

// Layout — the card reads "how you work", not "how many tokens": three charts
// (activity / by-hour / models) up top, parallel + task-time as plain numbers
// below. Token totals ride along quietly in the header.
const W: u32 = 1200;
const H: u32 = 675;
const LX: u32 = 58; // content left edge
const RX: u32 = 1142; // content right edge

// Three-column chart band: activity is compact (a 30-day grid is small), the
// hourly and model charts take the wide remainder.
const ACT_X: u32 = 58;
const HRL_X: u32 = 242;
const HRL_W: u32 = 470;
const HRL_R: u32 = HRL_X + HRL_W;
const MOD_X: u32 = 746;
const MOD_W: u32 = 396;

const SEC_Y: u32 = 178; // section-label baseline
const BODY_TOP: u32 = 196;
const BODY_BOT: u32 = 486;
const BODY_H: u32 = BODY_BOT - BODY_TOP;

/// Header stat line budget in characters: past this the right-anchored
/// line would reach the codename on the left.
const STAT_LINE_BUDGET: usize = 60;

/// Build the share card SVG at 1200x675.
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

    // Card background + framed panel.
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
        r#"<text x="{LX}" y="{}" fill="{C_DIM}" font-size="17"><tspan fill="{C_MUTED}" font-weight="700">agent-walker</tspan>  ·  {REPO_URL}</text>"#,
        H - 38
    );

    s.push_str("</svg>");
    s
}

/// Time-of-day word → tint. Mirrors the TUI badge mapping (theme colours), so
/// the card and dashboard agree; "Eclipse"/mixed falls through to purple.
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

    let _ = write!(
        s,
        r#"<text x="{RX}" y="74" fill="{C_MUTED}" font-size="18" text-anchor="end"><tspan fill="{C_TEXT}" font-weight="700">{}/{}</tspan> days active   ·   <tspan fill="{C_TEXT}" font-weight="700">{}</tspan> sessions</text>"#,
        card.active_days, card.period_days, card.sessions
    );
    // Cursor's reported cost is an actual charge, not an API-equivalent estimate.
    // An unknown cost renders as "—" — never "$0", which would misread as free.
    let cost_label = if card.has_reported_cost {
        "cost"
    } else {
        "api-equiv"
    };
    let cost = card.cost.as_deref().unwrap_or("—");
    // The stat line grows leftward from RX; with saturated (poisoned) token
    // or cost values it could reach the codename, so the cache share — the
    // optional part — yields first.
    let base = format!("{} tokens   ·   {cost} {cost_label}", card.tokens);
    let cached = card
        .cached
        .as_deref()
        .map(|cached| format!("   ·   {cached}"))
        .filter(|extra| base.chars().count() + extra.chars().count() <= STAT_LINE_BUDGET)
        .unwrap_or_default();
    let _ = write!(
        s,
        r#"<text x="{RX}" y="100" fill="{C_MUTED}" font-size="18" text-anchor="end">{base}{cached}</text>"#
    );

    let _ = write!(
        s,
        r#"<line x1="{LX}" y1="140" x2="{RX}" y2="140" stroke="{C_HAIRLINE}" stroke-width="1"/>"#
    );
}

/// The rank as a pill badge above the title, in the slot the "CODENAME" label
/// used to occupy (the label said nothing the card doesn't already show).
/// Coloured by the 冠位十二階 ladder via `Rank::display_rgb`; unranked leaves
/// the slot empty.
fn draw_rank_badge(s: &mut String, card: &ShareCard) {
    let (Some(letters), Some((r, g, b))) = (card.rank.letters(), card.rank.display_rgb()) else {
        return;
    };
    let color = format!("#{r:02x}{g:02x}{b:02x}");
    let label = format!("RANK {letters}");
    // Monospace label: ~10px per glyph at 14px + tracking, plus pill padding.
    // (6–7 ASCII glyphs, so the conversion never hits the fallback.)
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
    section(s, ACT_X, 0, "ACTIVITY", "");
    let size = 22_u32;
    let pitch = 27_u32;
    let grid_h = 7 * pitch - (pitch - size);
    let y0 = BODY_TOP + (BODY_H - grid_h) / 2;
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
    section(s, HRL_X, HRL_R, "BY HOUR", &format!("peak {peak:02}:00"));
    let max_h = 232_f64;
    let axis_gap = 20_f64;
    let baseline = f64::from(BODY_TOP) + (f64::from(BODY_H) - max_h - axis_gap) / 2.0 + max_h;
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
            r#"<text x="{x:.0}" y="{:.0}" fill="{C_DIM}" font-size="13" text-anchor="{anchor}">{hour:02}</text>"#,
            baseline + axis_gap
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
    section(s, MOD_X, RX, "MODELS", "share");
    if card.models.is_empty() {
        return;
    }
    let rows = card.models.len().min(4);
    let pitch = 56_u32;
    let block_h = u32::try_from(rows).unwrap_or(1) * pitch - 24;
    let mut ry = BODY_TOP + (BODY_H - block_h) / 2;
    let track_x = MOD_X;
    let track_w = MOD_W;
    for (index, (name, share, ratio, _formatted)) in card.models.iter().take(4).enumerate() {
        let grad = C_MODEL[index % C_MODEL.len()];
        let _ = write!(
            s,
            r#"<text x="{MOD_X}" y="{}" fill="{C_TEXT}" font-size="19">{}</text>"#,
            ry + 17,
            xml_escape(&truncate_tail(name, 16))
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

/// Bottom strip — parallel + task time as plain numbers. The two groups split
/// the width proportionally (parallel 3 cells : task 4 cells) so every cell ends
/// up the same width: a clean, even division.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Layout geometry is display-only."
)]
fn draw_bottom(s: &mut String, card: &ShareCard) {
    let accent = ops_color(&card.ops);
    // avg comes from its own field so it shows whenever there's a concurrency
    // signal, independent of the level breakdown that drives 4+/peak.
    let par_avg = if card.avg_concurrency > 0.0 || card.parallel.is_some() {
        format!("{:.1}", card.avg_concurrency)
    } else {
        "—".to_owned()
    };
    let (par_four, par_peak) = match &card.parallel {
        Some((four_plus, peak)) => (format!("{four_plus}%"), peak.to_string()),
        None => ("—".to_owned(), "—".to_owned()),
    };
    let (p50, p90, max, unattended) = match &card.completion {
        Some((_, un, _, p50, p90, max)) => (p50.clone(), p90.clone(), max.clone(), un.to_string()),
        None => (
            "—".to_owned(),
            "—".to_owned(),
            "—".to_owned(),
            "—".to_owned(),
        ),
    };

    let line_y = 520_u32;
    let label_y = 548_u32;
    let num_y = 582_u32;
    let cap_y = 602_u32;
    let _ = write!(
        s,
        r#"<line x1="{LX}" y1="{line_y}" x2="{RX}" y2="{line_y}" stroke="{C_HAIRLINE}" stroke-width="1"/>"#
    );

    // 7 equal cells, with a wider gap between the two groups.
    let slot = 145_u32;
    let par_x = [LX, LX + slot, LX + 2 * slot];
    let task_x0 = LX + 3 * slot + 69;
    let task_x = [
        task_x0,
        task_x0 + slot,
        task_x0 + 2 * slot,
        task_x0 + 3 * slot,
    ];

    section_label(s, par_x[0], label_y, "PARALLEL");
    section_label(s, task_x[0], label_y, "TASK TIME");

    let par = [
        (par_avg.as_str(), "avg", false),
        (par_four.as_str(), "4+", false),
        (par_peak.as_str(), "peak", false),
    ];
    for (i, (num, cap, _)) in par.iter().enumerate() {
        cell(s, par_x[i], num_y, cap_y, num, cap, C_TEXT);
    }
    let task = [
        (p50.as_str(), "p50", C_TEXT),
        (p90.as_str(), "p90", C_TEXT),
        (max.as_str(), "max", C_TEXT),
        (unattended.as_str(), "20m+ runs", accent),
    ];
    for (i, (num, cap, color)) in task.iter().enumerate() {
        cell(s, task_x[i], num_y, cap_y, num, cap, color);
    }
}

/// One bottom-strip stat: big number with a small caption beneath.
fn cell(s: &mut String, x: u32, num_y: u32, cap_y: u32, num: &str, cap: &str, color: &str) {
    let _ = write!(
        s,
        r#"<text x="{x}" y="{num_y}" fill="{color}" font-size="26" font-weight="700">{}</text>"#,
        xml_escape(num)
    );
    let _ = write!(
        s,
        r#"<text x="{x}" y="{cap_y}" fill="{C_MUTED}" font-size="14">{cap}</text>"#
    );
}

/// Section header: muted letter-spaced label, optional right-aligned annotation.
fn section(s: &mut String, x: u32, right: u32, label: &str, annotation: &str) {
    let _ = write!(
        s,
        r#"<text x="{x}" y="{SEC_Y}" fill="{C_MUTED}" font-size="15" letter-spacing="2.5" font-weight="700">{label}</text>"#
    );
    if !annotation.is_empty() && right > 0 {
        let _ = write!(
            s,
            r#"<text x="{right}" y="{SEC_Y}" fill="{C_DIM}" font-size="14" text-anchor="end">{}</text>"#,
            xml_escape(annotation)
        );
    }
}

/// A bare group label for the bottom strip.
fn section_label(s: &mut String, x: u32, y: u32, label: &str) {
    let _ = write!(
        s,
        r#"<text x="{x}" y="{y}" fill="{C_MUTED}" font-size="15" letter-spacing="2" font-weight="700">{label}</text>"#
    );
}

/// Escape a string for safe inclusion in SVG element text or attribute values.
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
