use super::proto::{message_field, string_field};

/// The display name (`Gemini 4 Neon (High)`) in the pricing id form
/// (`gemini-4-neon`). Rows on a `-tiered` selection carry no display name, so
/// labelling the rest by it raw split one model into two labels. The steps are
/// the pricing lookup's (lowercase, cut a `[deploy]` or `(tier)` suffix,
/// hyphenate spaces), so a label is the key it prices under. Only ASCII
/// whitespace becomes `-`: any other stays, and the shared-output sanitizer
/// still collapses the label.
pub(in crate::collector::agy) fn display_name_label(display: &str) -> String {
    let lower = display.to_ascii_lowercase();
    let no_deploy = lower
        .split_once('[')
        .map_or(lower.as_str(), |(head, _)| head);
    let no_tier = no_deploy
        .split_once('(')
        .map_or(no_deploy, |(head, _)| head);
    no_tier
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

/// The label is the selected model minus its last segment (the mode: `-high`,
/// `-tiered`, …), taken only when the served id is that base or the base plus
/// one segment (`gemini-3.8-flash-n` under `gemini-3.8-flash-high` →
/// `gemini-3.8-flash`). Matching the tags themselves (`-n`, `-trial`, …) was
/// rejected: they are relabelled server-side and a list goes stale on the next
/// one. An id that lines up with no selection stays as served rather than being
/// guessed into another model.
pub(in crate::collector::agy) fn served_model_label<'a>(
    served: &'a str,
    selected: &[String],
) -> &'a str {
    let selection_base = |base: &str| selected.iter().any(|id| parent_id(id) == Some(base));
    if selection_base(served) {
        return served;
    }
    match parent_id(served) {
        Some(base) if selection_base(base) => base,
        _ => served,
    }
}

fn parent_id(id: &str) -> Option<&str> {
    id.rsplit_once('-').map(|(head, _)| head)
}

pub(in crate::collector::agy) fn selected_models(blobs: &[(i64, Vec<u8>)]) -> Vec<String> {
    let mut models: Vec<String> = blobs
        .iter()
        .filter_map(|(_, blob)| message_field(blob, 3))
        .filter_map(|config| string_field(config, 28))
        .filter(|id| !id.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect();
    models.sort_unstable();
    models.dedup();
    models
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_takes_the_pricing_id_form() {
        assert_eq!(display_name_label("Gemini 4 Neon (High)"), "gemini-4-neon");
        assert_eq!(
            display_name_label("Gemini 3.8 Flash (Low)"),
            "gemini-3.8-flash"
        );
        assert_eq!(display_name_label(" Gemini 4  Argon "), "gemini-4-argon");
        assert_eq!(display_name_label("gemini-4-argon"), "gemini-4-argon");
        assert_eq!(display_name_label("Gemini 4 Neon [eu]"), "gemini-4-neon");
        assert_eq!(display_name_label("(High)"), "");
        // Non-ASCII whitespace is kept for the shared-output sanitizer to reject.
        assert_eq!(
            display_name_label("Gemini\u{2028}Neon"),
            "gemini\u{2028}neon"
        );
    }

    #[test]
    fn routing_tag_resolves_to_the_selected_model() {
        let selected = vec![
            "gemini-3.8-flash-high".to_owned(),
            "gemini-4-argon-medium".to_owned(),
        ];
        assert_eq!(
            served_model_label("gemini-3.8-flash-n", &selected),
            "gemini-3.8-flash"
        );
        assert_eq!(
            served_model_label("gemini-3.8-flash", &selected),
            "gemini-3.8-flash"
        );
        assert_eq!(
            served_model_label("gemini-4-argon-trial", &selected),
            "gemini-4-argon"
        );
        // The app serves under the selected id itself, mode segment included.
        let app = vec!["gemini-3.8-flash-tiered".to_owned()];
        assert_eq!(
            served_model_label("gemini-3.8-flash-tiered", &app),
            "gemini-3.8-flash"
        );
    }

    #[test]
    fn served_id_unrelated_to_the_selection_stays_as_served() {
        let selected = vec!["gemini-3.8-flash-high".to_owned()];
        assert_eq!(
            served_model_label("gemini-default", &selected),
            "gemini-default"
        );
        assert_eq!(
            served_model_label("gemini-3.8-flash-lite-n", &selected),
            "gemini-3.8-flash-lite-n"
        );
        assert_eq!(
            served_model_label("gemini-3.8-flash-n", &[]),
            "gemini-3.8-flash-n"
        );
    }
}
