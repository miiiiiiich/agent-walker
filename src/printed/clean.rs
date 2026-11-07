use crate::format::must_escape;

/// Tool, skill and subagent names reach this text from logs a cloned repo
/// controls, and it goes straight to a terminal, so nothing here may move a
/// cursor or reorder what is read. Stripped rather than escaped as `--json`
/// does: these strings are for reading, and no one parses them back.
pub(crate) fn clean(text: &str) -> String {
    text.chars()
        .filter(|&ch| !ch.is_control() && !must_escape(ch))
        .collect()
}
