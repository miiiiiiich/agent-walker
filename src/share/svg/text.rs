use std::fmt::Write as _;

use super::{ADVANCE, C_DIM, C_MUTED, SEC_Y};

pub(in crate::share::svg) fn dash() -> String {
    "—".to_owned()
}

/// Durations lose their space and carry their units at caption size on the
/// same baseline ("1h15m"), so the longest cell stops dwarfing its neighbours.
/// Anything that is not plain digits-and-units is escaped and left alone.
pub(in crate::share) fn number_markup(text: &str, size: f64) -> String {
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
pub(in crate::share::svg) fn fit_font(text: &str, width: f64, base: f64) -> f64 {
    let chars = text.chars().count().max(1) as f64;
    // Floored to the precision it is printed at: rounding up would draw the
    // text a hair wider than the column it was sized for.
    (base.min(width / (chars * ADVANCE)) * 10.0).floor() / 10.0
}

pub(in crate::share::svg) fn section(
    s: &mut String,
    x: u32,
    right: u32,
    label: &str,
    annotation: &str,
) {
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
pub(in crate::share::svg) fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub(in crate::share::svg) fn truncate_tail(text: &str, max: usize) -> String {
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
