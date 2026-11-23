use super::plausible_ms;
use super::proto::{message_field, proto_timestamp_ms, string_field};
use crate::collector::store::Store;

#[derive(Default)]
pub(in crate::collector::agy) struct Trajectory {
    pub(in crate::collector::agy) started_ms: i64,
    pub(in crate::collector::agy) project: Option<String>,
    pub(in crate::collector::agy) subagent: Option<Subagent>,
}

/// A conversation run as another's subagent. Its work belongs to the
/// top-level conversation's session, the way Claude logs a subagent's turns
/// under the parent session; counted on its own, its tokens showed as Main
/// work in a separate session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::collector::agy) struct Subagent {
    pub(in crate::collector::agy) session_id: String,
    pub(in crate::collector::agy) name: Option<String>,
}

pub(in crate::collector::agy) fn trajectory_meta(
    store: &Store,
    conversation_id: &str,
) -> Trajectory {
    if !store.has_table("trajectory_metadata_blob", &["data"]) {
        return Trajectory::default();
    }
    let blob: Option<Vec<u8>> = store
        .query_row(
            "SELECT data FROM trajectory_metadata_blob LIMIT 1",
            [],
            |row| {
                store.charge(row, 1)?;
                row.get(0)
            },
        )
        .ok();
    let Some(blob) = blob else {
        return Trajectory::default();
    };
    let started_ms = message_field(&blob, 2)
        .and_then(proto_timestamp_ms)
        .filter(|&ms| plausible_ms(ms))
        .unwrap_or(0);
    // A path, held to the path bound by `project_from_cwd`, not the id bound.
    let project = message_field(&blob, 1)
        .and_then(|folder| message_field(folder, 1))
        .and_then(|uri| std::str::from_utf8(uri).ok())
        .map(file_uri_to_path);
    Trajectory {
        started_ms,
        project,
        subagent: subagent(&blob, conversation_id),
    }
}

/// Only the parent id (`#5`) marks a subagent: a top-level conversation
/// carries the root id (`#6`) too, set to its own id. The session is the root,
/// so a nested subagent joins the top level rather than its direct parent.
fn subagent(blob: &[u8], conversation_id: &str) -> Option<Subagent> {
    let other = |id: &&str| !id.trim().is_empty() && *id != conversation_id;
    let parent = string_field(blob, 5).filter(other)?;
    let session_id = string_field(blob, 6).filter(other).unwrap_or(parent);
    let name = message_field(blob, 8)
        .and_then(|spec| string_field(spec, 1))
        .filter(|name| !name.trim().is_empty());
    Some(Subagent {
        session_id: session_id.to_owned(),
        name: name.map(ToOwned::to_owned),
    })
}

fn file_uri_to_path(uri: &str) -> String {
    let path = uri.trim_start_matches("file://");
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[2] == b':' && bytes[1].is_ascii_alphabetic() {
        path[1..].to_owned()
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::agy::conv::tests::lenf;

    #[test]
    fn windows_file_uri_drops_the_drive_slash() {
        assert_eq!(
            file_uri_to_path("file:///C:/Users/me/repo"),
            "C:/Users/me/repo"
        );
        assert_eq!(file_uri_to_path("file:///Users/me/repo"), "/Users/me/repo");
    }

    #[test]
    fn only_a_parent_id_marks_a_subagent() {
        let top_level = lenf(6, b"c1");
        assert_eq!(subagent(&top_level, "c1"), None);

        let mut child = lenf(5, b"c1");
        child.extend(lenf(6, b"c1"));
        child.extend(lenf(8, &lenf(1, b"research")));
        assert_eq!(
            subagent(&child, "c2"),
            Some(Subagent {
                session_id: "c1".to_owned(),
                name: Some("research".to_owned()),
            })
        );
    }

    #[test]
    fn nested_subagent_joins_the_root() {
        let mut nested = lenf(5, b"c2");
        nested.extend(lenf(6, b"c1"));
        assert_eq!(
            subagent(&nested, "c3").map(|sub| sub.session_id),
            Some("c1".to_owned())
        );

        let no_root = lenf(5, b"c2");
        assert_eq!(
            subagent(&no_root, "c3"),
            Some(Subagent {
                session_id: "c2".to_owned(),
                name: None,
            })
        );
    }
}
