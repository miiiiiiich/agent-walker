//! Ids, names and paths read from logs and stores are copied into every
//! event they apply to, so one long value multiplies: a row with a 1 MiB
//! `sessionId` and 10,000 `tool_use` blocks grew to ~9.8 GiB. A value past
//! `MAX_IDENT_BYTES` is treated as missing, like any other malformed field;
//! truncating it instead would merge unrelated sessions under one key.

/// Longest session, message or prompt id, or model, tool, agent or skill
/// name kept: these are short by format, and real ones stay far below it.
pub(in crate::collector) const MAX_IDENT_BYTES: usize = 1024;

/// Longest working-directory path kept: Linux's `PATH_MAX`, the most a cwd
/// can be.
pub(in crate::collector) const MAX_PATH_BYTES: usize = 4096;

pub(in crate::collector) fn ident(raw: &str) -> Option<&str> {
    (raw.len() <= MAX_IDENT_BYTES).then_some(raw)
}

pub(in crate::collector) fn path(raw: &str) -> Option<&str> {
    (raw.len() <= MAX_PATH_BYTES).then_some(raw)
}

pub(in crate::collector) fn ident_string(raw: String) -> Option<String> {
    (raw.len() <= MAX_IDENT_BYTES).then_some(raw)
}

#[cfg(test)]
pub(in crate::collector) mod tests {
    use super::*;

    /// An id one byte past the bound.
    pub(in crate::collector) fn overlong_ident() -> String {
        "x".repeat(MAX_IDENT_BYTES + 1)
    }

    /// An absolute path one byte past the bound.
    pub(in crate::collector) fn overlong_path() -> String {
        format!("/{}", "d".repeat(MAX_PATH_BYTES))
    }

    #[test]
    fn ids_past_the_bound_are_missing() {
        let edge = "x".repeat(MAX_IDENT_BYTES);
        assert_eq!(ident(&edge), Some(edge.as_str()));
        assert_eq!(ident(&overlong_ident()), None);
        assert_eq!(ident_string(overlong_ident()), None);
    }

    #[test]
    fn paths_may_be_as_long_as_linux_allows() {
        let deep = format!("/{}", "d".repeat(MAX_PATH_BYTES - 1));
        assert_eq!(path(&deep), Some(deep.as_str()));
        assert_eq!(path(&overlong_path()), None);
    }
}
