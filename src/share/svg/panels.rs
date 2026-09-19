use std::fmt::Write as _;

use crate::format::format_count;
use crate::share::card::ShareCard;

use super::text::{section, truncate_tail, xml_escape};
use super::{
    ACT_X, AXIS_Y, C_BLUE, C_DIM, C_GOLD, C_HAIRLINE, C_HEAT, C_HEAT_ZERO, C_MODEL, C_MUTED,
    C_TEXT, C_TRACK, CHART_BASE, HRL_R, HRL_W, HRL_X, MOD_W, MOD_X, MODEL_LABEL_MAX, RX,
};

pub(in crate::share::svg) fn draw_activity(s: &mut String, card: &ShareCard) {
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
    reason = "Chart geometry is display-only."
)]
pub(in crate::share::svg) fn draw_hourly(s: &mut String, card: &ShareCard) {
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
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Chart geometry is display-only."
)]
pub(in crate::share::svg) fn draw_models(s: &mut String, card: &ShareCard) {
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
