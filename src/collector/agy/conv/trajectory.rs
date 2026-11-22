use super::plausible_ms;
use super::proto::{message_field, proto_timestamp_ms};
use crate::collector::store::Store;

pub(in crate::collector::agy) fn trajectory_meta(store: &Store) -> (i64, Option<String>) {
    if !store.has_table("trajectory_metadata_blob", &["data"]) {
        return (0, None);
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
        return (0, None);
    };
    let ts = message_field(&blob, 2)
        .and_then(proto_timestamp_ms)
        .filter(|&ms| plausible_ms(ms))
        .unwrap_or(0);
    // A path, held to the path bound by `project_from_cwd`, not the id bound.
    let project = message_field(&blob, 1)
        .and_then(|folder| message_field(folder, 1))
        .and_then(|uri| std::str::from_utf8(uri).ok())
        .map(file_uri_to_path);
    (ts, project)
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

    #[test]
    fn windows_file_uri_drops_the_drive_slash() {
        assert_eq!(
            file_uri_to_path("file:///C:/Users/me/repo"),
            "C:/Users/me/repo"
        );
        assert_eq!(file_uri_to_path("file:///Users/me/repo"), "/Users/me/repo");
    }
}
