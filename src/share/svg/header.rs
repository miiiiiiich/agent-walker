use std::fmt::Write as _;

use crate::share::badge_art;
use crate::share::card::ShareCard;

use super::text::{dash, fit_font, xml_escape};
use super::{C_HAIRLINE, C_MUTED, C_TEXT, HERO_GUTTER, HERO_PITCH, LX, RX, ops_color};

pub(in crate::share::svg) fn draw_watermark(s: &mut String, card: &ShareCard) {
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

pub(in crate::share::svg) fn draw_header(s: &mut String, card: &ShareCard) {
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
