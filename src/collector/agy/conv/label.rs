use super::proto::{message_field, string_field};

/// The display name (`Gemini 4 Neon (High)`) in the pricing id form
/// (`gemini-4-neon`). Rows on a `-tiered` selection carry no display name, so
/// labelling the rest by it raw split one model into two labels. The steps are
/// the pricing lookup's (lowercase, cut a `[deploy]` or `(tier)` suffix,
/// hyphenate spaces), so a label is the key it prices under.
///
/// A name holding a control character or whitespace other than the space is
/// not used, checked before anything is cut: hyphenating a tab turned
/// `Gemini\tprivate-repo` into `gemini-private-repo`, and cutting the tier off
/// `Gemini private-repo\t(High)` left a trailing tab the shared-output
/// sanitizer trims, so both reached the share card. Such a name, and one with
/// nothing left, is `None`, and the row is labelled from its served id as if
/// it had no display name.
pub(in crate::collector::agy) fn display_name_label(display: &str) -> Option<String> {
    if display
        .chars()
        .any(|ch| ch.is_control() || (ch.is_whitespace() && ch != ' '))
    {
        return None;
    }
    let lower = display.to_ascii_lowercase();
    let no_deploy = lower
        .split_once('[')
        .map_or(lower.as_str(), |(head, _)| head);
    let no_tier = no_deploy
        .split_once('(')
        .map_or(no_deploy, |(head, _)| head);
    let label = no_tier
        .split(' ')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    (!label.is_empty()).then_some(label)
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
        let label = |display: &str| display_name_label(display);
        assert_eq!(
            label("Gemini 4 Neon (High)").as_deref(),
            Some("gemini-4-neon")
        );
        assert_eq!(
            label("Gemini 3.8 Flash (Low)").as_deref(),
            Some("gemini-3.8-flash")
        );
        assert_eq!(
            label(" Gemini 4  Argon ").as_deref(),
            Some("gemini-4-argon")
        );
        assert_eq!(label("gemini-4-argon").as_deref(), Some("gemini-4-argon"));
        assert_eq!(
            label("Gemini 4 Neon [eu]").as_deref(),
            Some("gemini-4-neon")
        );
        assert_eq!(label("(High)"), None);
        // Neither is what the sanitizer lets through, so neither labels a row.
        assert_eq!(label("Gemini 4 Neon\u{a0}(High)"), None);
        assert_eq!(label("Gemini\u{2028}Neon"), None);
    }

    /// A tab or line break anywhere, the cut suffix included, keeps the name
    /// off the row, so the share card can't show a smuggled one.
    #[test]
    fn a_name_with_other_whitespace_or_a_control_character_is_not_used() {
        for display in [
            "Gemini\tprivate-repo",
            "Gemini\nprivate-repo",
            "Gemini\rprivate-repo",
            "Gemini\u{c}private-repo",
            "Gemini private-repo\t(High)",
            "Gemini private-repo\n[eu]",
            "Gemini private-repo (\u{1b}High)",
        ] {
            assert_eq!(display_name_label(display), None, "{display:?}");
        }
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
