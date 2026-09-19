use std::fmt::Write as _;

use crate::format::format_count;
use crate::share::card::ShareCard;

use super::text::{dash, fit_font, number_markup};
use super::{
    C_HAIRLINE, C_MUTED, C_TEXT, CELL_GUTTER, CONTEXT_W, GROUP_GAP, LX, RX, SESSIONS_W, TURNS_W,
    ops_color,
};

/// Bottom strip — three groups told apart by space alone. Columns are sized to
/// what they hold rather than split evenly: even slots either starve the wide
/// duration cells or force the captions below legible size on a shrunk card.
pub(in crate::share::svg) fn draw_bottom(s: &mut String, card: &ShareCard) {
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
