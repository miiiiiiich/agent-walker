//! Working-directory to project-label normalization, shared by every
//! collector.
/// Normalize a working-directory path into a project label: strip the home
/// prefix and (on Windows) normalize separators to `/` so the same repo
/// collapses to one project key regardless of native (`\`) vs npm/Node-style
/// (`/`) cwd. "/Users/me/code/app" -> "code/app",
/// "C:\\Users\\me\\code\\app" -> "code/app". A session whose cwd is exactly
/// the home directory renders as "~" rather than an empty label so the
/// PROJECTS row has something readable.
pub fn project_from_cwd(cwd: &str) -> String {
    let home = crate::paths::home_dir()
        .ok()
        .and_then(|home| home.to_str().map(ToOwned::to_owned));
    let stripped = home
        .and_then(|home| strip_home_prefix(cwd, &home))
        .unwrap_or_else(|| cwd.trim_start_matches(['/', '\\']).to_owned());
    if stripped.is_empty() {
        "~".to_owned()
    } else {
        stripped
    }
}

/// Strip the home prefix and the single separator that follows it. Windows
/// filesystems are case-insensitive, so a cwd recorded as `c:\Users\me\…`
/// must still match a home of `C:\Users\me`; compare case-insensitively but
/// slice the original cwd so the rest of the path keeps its real casing.
/// Backslashes and forward slashes are equivalent on Windows, so the prefix
/// match normalizes both to `/` before comparing — npm/Node tooling often
/// records cwds with forward slashes even on Windows. Requires a path-
/// component boundary after the prefix so that `C:\Users\metadata` does not
/// get stripped against home `C:\Users\me`.
#[cfg(windows)]
fn strip_home_prefix(cwd: &str, home: &str) -> Option<String> {
    // Trim any trailing separator on `home` (e.g. a drive-root home like
    // `D:\`) so `home.len()` doesn't include the separator and the boundary
    // check below stays meaningful.
    let home = home.trim_end_matches(['/', '\\']);
    if home.is_empty() {
        return None;
    }
    // `get` keeps us safe if `home.len()` lands inside a multi-byte UTF-8
    // character in `cwd`; `split_at` would panic there.
    let head = cwd.get(..home.len())?;
    let rest = cwd.get(home.len()..)?;
    if !head
        .replace('\\', "/")
        .eq_ignore_ascii_case(&home.replace('\\', "/"))
    {
        return None;
    }
    if !rest.is_empty() && !rest.starts_with(['/', '\\']) {
        return None;
    }
    // Normalize backslashes to forward slashes in the remainder so the same
    // repository visited as `C:\Users\me\code\app` (native) and
    // `C:/Users/me/code/app` (npm/Node tooling) collapses to one project key
    // (`code/app`) instead of splitting the totals across two labels.
    Some(rest.trim_start_matches(['/', '\\']).replace('\\', "/"))
}

/// Same shape on Unix: require a path-component boundary so that home
/// `/Users/me` does not silently strip a cwd like `/Users/metadata/app` into
/// `tadata/app` (an attribution bug the previous `{home}/` prefix avoided).
#[cfg(not(windows))]
fn strip_home_prefix(cwd: &str, home: &str) -> Option<String> {
    // Trim any trailing slash on `home` so the boundary check below isn't
    // defeated when `dirs::home_dir` returns `/home/me/`.
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return None;
    }
    let rest = cwd.strip_prefix(home)?;
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    Some(rest.trim_start_matches('/').to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_home_prefix_unix() {
        // Posix-only test: case-sensitive byte comparison with boundary check.
        if !cfg!(windows) {
            assert_eq!(
                strip_home_prefix("/Users/me/code/app", "/Users/me"),
                Some("code/app".to_owned()),
            );
            // No prefix match → None so the caller falls back.
            assert_eq!(strip_home_prefix("/var/log/x", "/Users/me"), None);
            // Non-boundary sibling does not strip (would otherwise yield
            // "tadata/app" for "/Users/metadata/app").
            assert_eq!(strip_home_prefix("/Users/metadata/app", "/Users/me"), None);
            assert_eq!(strip_home_prefix("/Users/me-work/app", "/Users/me"), None);
            // Trailing slash on home still strips cleanly.
            assert_eq!(
                strip_home_prefix("/Users/me/code", "/Users/me/"),
                Some("code".to_owned()),
            );
            // Cwd equal to home returns empty (the caller substitutes "~").
            assert_eq!(
                strip_home_prefix("/Users/me", "/Users/me"),
                Some(String::new())
            );
        }
    }

    #[test]
    fn project_from_cwd_renames_home_to_tilde() {
        // When the cwd resolves to the home directory itself, the project
        // label is "~" rather than the empty string.
        if !cfg!(windows) {
            // Cannot easily inject a fake home; only verify the empty-string
            // fallback path through a leading-slash cwd that the home strip
            // would not match (so the trim-only branch runs) is never empty.
            assert_eq!(project_from_cwd("/"), "~");
        }
    }

    #[test]
    fn strip_home_prefix_windows_case_insensitive() {
        // Windows-only test: case-insensitive prefix match with separator
        // normalization and boundary check.
        if cfg!(windows) {
            // Lowercase drive letter still strips, remainder normalized.
            assert_eq!(
                strip_home_prefix(r"c:\users\me\code\app", r"C:\Users\me"),
                Some("code/app".to_owned()),
            );
            // Mixed separators (forward-slashed cwd from npm/Node tooling,
            // backslashed home from dirs::home_dir) — and the remainder is
            // returned with normalized forward slashes so a native-style
            // visit to the same repo also lands at `code/app`, not `code\app`.
            assert_eq!(
                strip_home_prefix("C:/Users/me/code/app", r"C:\Users\me"),
                Some("code/app".to_owned()),
            );
            assert_eq!(
                strip_home_prefix(r"C:\Users\me\code\app", r"C:\Users\me"),
                Some("code/app".to_owned()),
            );
            // Non-boundary sibling does not strip.
            assert_eq!(
                strip_home_prefix(r"C:\Users\metadata", r"C:\Users\me"),
                None
            );
            // Drive-root home with a trailing separator still strips (and
            // the remainder is normalized to forward slashes).
            assert_eq!(
                strip_home_prefix(r"D:\code\app", r"D:\"),
                Some("code/app".to_owned()),
            );
        }
    }
}
