use std::fmt::Write as _;

use super::REPO_URL;
use super::card::{ShareCard, Variant};

// Card palette (matches the TUI brand).
const C_TEXT: &str = "#eeede6";
const C_GOLD: &str = "#efc768";
const C_ACCENT: &str = "#e2b25c";
const C_MUTED: &str = "#8c9196";
const C_DIM: &str = "#4d525a";
const C_FAINT: &str = "#33373d";
const C_BORDER: &str = "#26282e";
const C_HAIRLINE: &str = "#1e2026";
const C_CARD_BG: &str = "#0a0a0c";
const C_PANEL_TOP: &str = "#17181d";
const C_PANEL_BOTTOM: &str = "#0f1014";
const C_BLUE: &str = "#84a7ff";
const C_CODEC: &str = "#54d98c"; // dog-tag codec green
const C_CODEC_BG: &str = "#0d1a12"; // dog-tag pill fill
const C_HEAT_ZERO: &str = "#21262d";
const C_HEAT: [&str; 4] = ["#0e4429", "#006d32", "#26a641", "#39d353"];
const C_MODEL: [&str; 6] = [
    "#84a7ff", "#68d391", "#efc768", "#db6954", "#ba94ff", "#63d6d2",
];
/// Turn-duration distribution: cool shades under 20m, green ramp for the
/// 20m+ autonomy range — the more green, the more unattended work.
const C_TURNS: [&str; 7] = [
    "#2c3140", "#3b4663", "#516399", "#1d6b45", "#27935b", "#39d353", "#7ce8a4",
];
/// PARALLEL AGENTS cool→hot ramp: solo (cool blue) → heavy parallel (hot).
const C_PARALLEL: [&str; 6] = ["#84a7ff", "#63d6d2", "#68d391", "#efc768", "#e0863c", "#db6954"];

const FONT: &str = "'SF Mono','Menlo','DejaVu Sans Mono','Consolas',monospace";

/// Build the card SVG at 1200x675 (16:9): hero band, then a dense
/// two-column grid (activity / hourly / completion · models / breakdown).
#[allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Flat SVG assembly; geometry is display-only."
)]
pub fn svg(card: &ShareCard) -> String {
    const W: u32 = 1200;
    const H: u32 = 675;
    const LX: u32 = 60; // left column origin
    const RX: u32 = 624; // right column origin
    const LW: u32 = 500; // left column width
    let right_edge = W - 60;

    let mut s = String::new();
    let _ = write!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" font-family="{FONT}">"#
    );
    let _ = write!(
        s,
        r##"<defs>
<linearGradient id="panel" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{C_PANEL_TOP}"/><stop offset="1" stop-color="{C_PANEL_BOTTOM}"/></linearGradient>
<linearGradient id="highlight-bg" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#141519"/><stop offset="1" stop-color="#0c0d10"/></linearGradient>
<linearGradient id="gold-grad" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="{C_GOLD}"/><stop offset="1" stop-color="{C_GOLD}cc"/></linearGradient>
<linearGradient id="blue-grad" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="{C_BLUE}"/><stop offset="1" stop-color="#4d7eff"/></linearGradient>
<linearGradient id="accent-grad" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="{C_ACCENT}"/><stop offset="1" stop-color="#ba94ff"/></linearGradient>
"##
    );
    for (i, color) in C_MODEL.iter().enumerate() {
        let _ = write!(
            s,
            r#"<linearGradient id="model-grad-{i}" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="{color}"/><stop offset="1" stop-color="{color}88"/></linearGradient>"#
        );
    }
    s.push_str("</defs>");

    let _ = write!(s, r#"<rect width="{W}" height="{H}" fill="{C_CARD_BG}"/>"#);
    let _ = write!(
        s,
        r#"<rect x="24" y="24" width="{}" height="{}" rx="20" fill="url(#panel)" stroke="{C_BORDER}" stroke-width="1.5"/>"#,
        W - 48,
        H - 48
    );
    let _ = write!(
        s,
        r#"<rect x="25" y="25" width="{}" height="{}" rx="19" fill="none" stroke="{C_HAIRLINE}" stroke-width="1" stroke-opacity="0.5"/>"#,
        W - 50,
        H - 50
    );

    let _ = write!(
        s,
        r#"<rect x="{LX}" y="58" width="8" height="28" rx="3" fill="{C_GOLD}"/>"#
    );
    let _ = write!(
        s,
        r#"<text x="{}" y="81" fill="{C_TEXT}" font-size="27" font-weight="700" letter-spacing="3">AGENT WALKER</text>"#,
        LX + 24
    );
    let _ = write!(
        s,
        r#"<text x="{right_edge}" y="81" fill="{C_MUTED}" font-size="18" text-anchor="end">{} of last {} days active</text>"#,
        card.active_days, card.period_days
    );
    // Dog-tag: the earned codename, centered as a codec-green pill.
    let _ = write!(
        s,
        r#"<rect x="455" y="55" width="290" height="34" rx="17" fill="{C_CODEC_BG}" stroke="{C_CODEC}" stroke-width="1.5"/>"#
    );
    let _ = write!(
        s,
        r#"<text x="600" y="78" fill="{C_CODEC}" font-size="20" font-weight="700" letter-spacing="1.5" text-anchor="middle">{}</text>"#,
        xml_escape(&card.codename.to_uppercase())
    );
    let _ = write!(
        s,
        r#"<line x1="{LX}" y1="104" x2="{right_edge}" y2="104" stroke="{C_HAIRLINE}" stroke-width="1"/>"#
    );

    draw_hero(&mut s, card, LX, right_edge);
    draw_left_column(&mut s, card, LX, LW);
    draw_right_column(&mut s, card, RX, right_edge);

    let _ = write!(
        s,
        r#"<text x="{LX}" y="{}" fill="{C_DIM}" font-size="18">{REPO_URL}</text>"#,
        H - 36
    );
    let _ = write!(
        s,
        r#"<text x="{right_edge}" y="{}" fill="{C_DIM}" font-size="18" text-anchor="end">no telemetry · 100% local</text>"#,
        H - 36
    );

    s.push_str("</svg>");
    s
}

#[allow(
    clippy::too_many_lines,
    reason = "Hero SVG is intentionally flat to keep geometry readable."
)]
fn draw_hero(s: &mut String, card: &ShareCard, lx: u32, right_edge: u32) {
    let _ = write!(
        s,
        r#"<text x="{}" y="188" fill="{C_CARD_BG}" font-size="78" font-weight="700">{}</text>"#,
        lx + 2,
        card.tokens
    );
    let _ = write!(
        s,
        r#"<text x="{lx}" y="186" fill="{C_TEXT}" font-size="78" font-weight="700">{}</text>"#,
        card.tokens
    );
    let _ = write!(
        s,
        r#"<text x="{lx}" y="216" fill="{C_MUTED}" font-size="20">tokens</text>"#
    );

    let box_x = 420_u32;
    let box_y = 125_u32;
    let box_w = 360_u32;
    let box_h = 96_u32;
    let _ = write!(
        s,
        r#"<rect x="{box_x}" y="{box_y}" width="{box_w}" height="{box_h}" rx="12" fill="url(#highlight-bg)" stroke="{C_BORDER}" stroke-width="1.5" stroke-opacity="0.8"/>"#
    );
    let _ = write!(
        s,
        r#"<rect x="{}" y="{}" width="3" height="12" rx="1.5" fill="{C_GOLD}"/>"#,
        box_x + 16,
        box_y + 16
    );
    let _ = write!(
        s,
        r#"<text x="{}" y="{}" fill="{C_MUTED}" font-size="12" font-weight="700" letter-spacing="2">HIGHLIGHTS</text>"#,
        box_x + 26,
        box_y + 26
    );

    let top_day_val = card.top_day.as_deref().unwrap_or("n/a");
    let _ = write!(
        s,
        r#"<text x="{}" y="{}" fill="{C_MUTED}" font-size="14">top day</text>"#,
        box_x + 16,
        box_y + 51
    );
    let _ = write!(
        s,
        r#"<text x="{}" y="{}" fill="{C_TEXT}" font-size="14" font-weight="700" text-anchor="end">{}</text>"#,
        box_x + box_w - 16,
        box_y + 51,
        xml_escape(top_day_val)
    );

    let longest_val = card.longest_session.as_deref().unwrap_or("n/a");
    let _ = write!(
        s,
        r#"<text x="{}" y="{}" fill="{C_MUTED}" font-size="14">longest session</text>"#,
        box_x + 16,
        box_y + 75
    );
    let _ = write!(
        s,
        r#"<text x="{}" y="{}" fill="{C_TEXT}" font-size="14" font-weight="700" text-anchor="end">{}</text>"#,
        box_x + box_w - 16,
        box_y + 75,
        xml_escape(longest_val)
    );

    let _ = write!(
        s,
        r#"<text x="{}" y="188" fill="{C_CARD_BG}" font-size="78" font-weight="700" text-anchor="end">{}</text>"#,
        right_edge + 2,
        card.cost
    );
    let _ = write!(
        s,
        r#"<text x="{right_edge}" y="186" fill="{C_GOLD}" font-size="78" font-weight="700" text-anchor="end">{}</text>"#,
        card.cost
    );
    let _ = write!(
        s,
        r#"<text x="{right_edge}" y="216" fill="{C_MUTED}" font-size="20" text-anchor="end">API-equivalent</text>"#
    );

    let mut subs: Vec<String> = vec![format!("{} sessions", card.sessions)];
    if let Some((up, pct)) = &card.delta {
        subs.push(format!("{}{pct} vs previous", if *up { "↑" } else { "↓" }));
    }
    let _ = write!(
        s,
        r#"<text x="{lx}" y="254" fill="{C_MUTED}" font-size="21">{}</text>"#,
        subs.join("   ·   ")
    );
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    reason = "SVG chart geometry is display-only."
)]
fn draw_left_column(s: &mut String, card: &ShareCard, lx: u32, lw: u32) {
    section(s, lx, 304, "ACTIVITY", "");
    let cell = 12_u32;
    let pitch = 16_u32;
    let grass_y = 318_u32;
    for (col, week) in card.grass.cells.iter().enumerate() {
        for (row, level) in week.iter().enumerate() {
            let fill = match level {
                None => continue,
                Some(0) => C_HEAT_ZERO,
                Some(n) => C_HEAT[(n - 1).min(3)],
            };
            let x = lx + u32::try_from(col).unwrap_or(0) * pitch;
            let y = grass_y + u32::try_from(row).unwrap_or(0) * pitch;
            let _ = write!(
                s,
                r#"<rect x="{x}" y="{y}" width="{cell}" height="{cell}" rx="3" fill="{fill}"/>"#
            );
        }
    }

    if let Some((heights, peak, peak_label)) = &card.hourly {
        section(s, lx, 450, "BY HOUR", &format!("peak {peak_label}"));
        let base = 516_f64;
        let max_h = 58_f64;
        let slot = f64::from(lw) / 24.0;
        for (hour, height) in heights.iter().enumerate() {
            if *height <= 0.0 {
                continue;
            }
            let bar_h = (max_h * height).max(3.0);
            let x = f64::from(lx) + slot * hour as f64;
            let fill = if hour == *peak {
                "url(#gold-grad)"
            } else {
                "url(#blue-grad)"
            };
            let _ = write!(
                s,
                r#"<rect x="{x:.1}" y="{:.1}" width="{:.1}" height="{bar_h:.1}" rx="3" fill="{fill}"/>"#,
                base - bar_h,
                slot - 6.0
            );
        }
        let _ = write!(
            s,
            r#"<line x1="{lx}" y1="{}" x2="{}" y2="{}" stroke="{C_HAIRLINE}" stroke-width="1"/>"#,
            520,
            lx + lw,
            520
        );
        for (hour, anchor) in [
            (0_u32, "start"),
            (6, "middle"),
            (12, "middle"),
            (18, "middle"),
            (24, "end"),
        ] {
            let x = f64::from(lx) + f64::from(lw) / 24.0 * f64::from(hour);
            let _ = write!(
                s,
                r#"<text x="{x:.0}" y="536" fill="{C_DIM}" font-size="13" text-anchor="{anchor}">{hour:02}</text>"#
            );
        }
    }

    if let Some((levels, four_plus_pct, peak)) = &card.parallel {
        section(
            s,
            lx,
            606,
            "PARALLEL AGENTS",
            &format!("{four_plus_pct}% at 4+ agents · peak {peak}"),
        );
        let total: u64 = levels.iter().sum::<u64>().max(1);
        let bar_y = 614_u32;
        let mut x = f64::from(lx);
        for (index, secs) in levels.iter().enumerate() {
            if *secs == 0 {
                continue;
            }
            let width = (f64::from(lw) * (*secs as f64) / total as f64).max(5.0);
            let _ = write!(
                s,
                r#"<rect x="{x:.1}" y="{bar_y}" width="{:.1}" height="13" rx="3" fill="{}"/>"#,
                width - 2.0,
                C_PARALLEL[index.min(C_PARALLEL.len() - 1)]
            );
            x += width;
        }
    }

    if let Some((counts, unattended, total, _p50, _p90, _max)) = &card.completion {
        section(
            s,
            lx,
            566,
            "COMPLETION",
            &format!("{unattended} of {total} turns ran 20m+ unattended"),
        );
        let total_count: usize = counts.iter().sum::<usize>().max(1);
        let bar_y = 574_u32;
        let mut x = f64::from(lx);
        for (index, count) in counts.iter().enumerate() {
            if *count == 0 {
                continue;
            }
            let width = (f64::from(lw) * (*count as f64) / total_count as f64).max(5.0);
            let _ = write!(
                s,
                r#"<rect x="{x:.1}" y="{bar_y}" width="{:.1}" height="13" rx="3" fill="{}"/>"#,
                width - 2.0,
                C_TURNS[index.min(C_TURNS.len() - 1)]
            );
            x += width;
        }
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "SVG chart geometry is display-only."
)]
fn draw_right_column(s: &mut String, card: &ShareCard, rx: u32, right_edge: u32) {
    section(s, rx, 304, "MODELS", "share of tokens");
    let mut ry = 322_u32;
    let pct_col = 145_u32;
    let bar_x = rx + 125;
    let bar_max = right_edge - bar_x - pct_col;
    for (index, (name, share, ratio, formatted_tokens)) in card.models.iter().enumerate() {
        let grad_id = index % C_MODEL.len();
        let _ = write!(
            s,
            r#"<text x="{rx}" y="{}" fill="{C_TEXT}" font-size="20">{}</text>"#,
            ry + 15,
            xml_escape(&truncate_tail(name, 12))
        );
        let _ = write!(
            s,
            r#"<rect x="{bar_x}" y="{}" width="{bar_max}" height="12" rx="6" fill="{C_FAINT}"/>"#,
            ry + 3
        );
        let fill_w = (f64::from(bar_max) * ratio).round() as u32;
        let _ = write!(
            s,
            r#"<rect x="{bar_x}" y="{}" width="{fill_w}" height="12" rx="6" fill="url(#model-grad-{grad_id})"/>"#,
            ry + 3
        );
        let label = format!("{formatted_tokens} · {share}");
        let _ = write!(
            s,
            r#"<text x="{right_edge}" y="{}" fill="{C_MUTED}" font-size="17" text-anchor="end">{}</text>"#,
            ry + 15,
            xml_escape(&label)
        );
        ry += 33;
    }

    if card.projects.is_empty() {
        section(s, rx, 496, "PROJECTS", "no tracked projects");
    } else {
        section(s, rx, 496, "PROJECTS", "by token volume");
        let mut py = 514_u32;
        let pbar_x = rx + 210;
        let pbar_max = right_edge - pbar_x;
        for (index, (name, ratio)) in card.projects.iter().enumerate() {
            let display_name = if card.variant == Variant::Summary {
                format!("Project {}", (b'A' + (index as u8) % 26) as char)
            } else {
                name.clone()
            };
            let _ = write!(
                s,
                r#"<text x="{rx}" y="{}" fill="{C_MUTED}" font-size="18">{}</text>"#,
                py + 13,
                xml_escape(&truncate_tail(&display_name, 17))
            );
            let _ = write!(
                s,
                r#"<rect x="{pbar_x}" y="{}" width="{pbar_max}" height="10" rx="5" fill="{C_FAINT}"/>"#,
                py + 3
            );
            let fill_w = (f64::from(pbar_max) * ratio).round() as u32;
            let _ = write!(
                s,
                r#"<rect x="{pbar_x}" y="{}" width="{fill_w}" height="10" rx="5" fill="url(#accent-grad)"/>"#,
                py + 3
            );
            py += 30;
        }
    }
}

/// Section header: gold tick + letter-spaced label + muted annotation.
fn section(s: &mut String, x: u32, y: u32, label: &str, annotation: &str) {
    let _ = write!(
        s,
        r#"<rect x="{x}" y="{}" width="4" height="14" rx="2" fill="{C_GOLD}"/>"#,
        y - 12
    );
    let _ = write!(
        s,
        r#"<text x="{}" y="{y}" fill="{C_MUTED}" font-size="15" letter-spacing="2.5" font-weight="700">{label}</text>"#,
        x + 12
    );
    if !annotation.is_empty() {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "Section labels are short ASCII."
        )]
        let after = x + 12 + u32::try_from(label.chars().count()).unwrap_or(0) * 12 + 18;
        let _ = write!(
            s,
            r#"<text x="{after}" y="{y}" fill="{C_DIM}" font-size="14">{}</text>"#,
            xml_escape(annotation)
        );
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn truncate_tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let tail: String = text.chars().skip(count - (max - 1)).collect();
    format!("…{tail}")
}
