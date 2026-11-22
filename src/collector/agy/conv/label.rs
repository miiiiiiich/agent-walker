use super::proto::{message_field, string_field};

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
