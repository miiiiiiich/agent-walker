//! Cursor collector — **opt-in**, the only provider that reaches the network.
//!
//! Cursor keeps no per-request token counts on disk (the local stores hold chat
//! text, accepted-line attribution, and the auth token — never token usage), so
//! the figures live only behind Cursor's web dashboard. We replay that
//! dashboard's own CSV export the way the browser does: read the session JWT
//! from the local Electron `state.vscdb`, build the `WorkosCursorSessionToken`
//! cookie, and `GET` the usage CSV.
//!
//! This is the one collector that sends anything off the machine (the user's own
//! session cookie, to Cursor, to read the user's own usage), so it never runs
//! unless explicitly enabled — there is no auto-detection. It is also an
//! undocumented endpoint that can change without notice.
//!
//! Cursor's usage events carry **no project/repo identifier** and its models
//! (`composer-2.5-fast`, …) aren't in the `LiteLLM` table, so the CSV's own
//! `Cost` column is carried through as `UsageEvent::reported_cost_usd`.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::time::Duration;
use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;

use crate::model::{Collection, Provider, SourceKind, TokenUsage, UsageEvent};

const CSV_URL: &str = "https://cursor.com/api/dashboard/export-usage-events-csv?strategy=tokens";
const REFERER: &str = "https://www.cursor.com/settings";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

const ACCESS_TOKEN_SQL: &str = "SELECT value FROM ItemTable WHERE key = 'cursorAuth/accessToken'";

/// Collect Cursor usage. `token_override` (from `--cursor-token` / `CURSOR_TOKEN`)
/// wins over the local DB so a relocated or unreadable store can still be used.
/// `cli_config` is `~/.cursor/cli-config.json`, the primary source for the
/// account id (the JWT `sub` is the fallback).
pub fn collect(
    state_db: &Path,
    cli_config: &Path,
    token_override: Option<&str>,
    mtime_floor: Option<SystemTime>,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::Cursor, state_db.to_path_buf());

    let jwt = if let Some(token) = token_override {
        token.trim().to_owned()
    } else {
        let Some(token) = read_access_token(state_db) else {
            collection.stats.unreadable_files += 1;
            return collection;
        };
        token
    };
    let Some(user_id) = account_id(cli_config, &jwt) else {
        collection.stats.unreadable_files += 1;
        return collection;
    };

    let cookie = format!("WorkosCursorSessionToken={user_id}%3A%3A{jwt}");
    // Auth expiry, a network failure, or an endpoint change all land here;
    // surface it as an unreadable source rather than a panic.
    let Ok(csv) = fetch_csv(&cookie) else {
        collection.stats.unreadable_files += 1;
        return collection;
    };
    collection.stats.files_seen += 1;

    let floor = mtime_floor.and_then(systemtime_to_offset);
    parse_csv(&csv, floor, local_offset, &mut collection);

    collection.stats.usage_events = collection.usage_events.len();
    collection
}

/// Read `cursorAuth/accessToken` from the Electron `state.vscdb` (read-only, so
/// SQLite never writes Cursor's live store).
fn read_access_token(state_db: &Path) -> Option<String> {
    if !state_db.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(state_db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(500));
    let token: String = conn
        .query_row(ACCESS_TOKEN_SQL, [], |row| row.get(0))
        .ok()?;
    let token = token.trim();
    if token.len() < 10 {
        return None;
    }
    Some(token.to_owned())
}

/// The account id for the cookie: `cli-config.json` `authInfo.authId` first,
/// then the JWT `sub`. Both are normalized the same way.
fn account_id(cli_config: &Path, jwt: &str) -> Option<String> {
    if let Some(id) = std::fs::read_to_string(cli_config)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|cfg| {
            cfg.pointer("/authInfo/authId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .and_then(|subject| normalize_subject(&subject))
    {
        return Some(id);
    }
    jwt_subject(jwt).and_then(|subject| normalize_subject(&subject))
}

/// The `sub` claim from a JWT (`header.payload.signature`, base64url, no pad).
fn jwt_subject(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("sub")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

/// Normalize a `WorkOS` subject into the id Cursor's cookie expects: a
/// `…|user_XXX` suffix collapses to `user_XXX` (native Cursor accounts), while a
/// bridged OAuth subject (`google-oauth2|<id>`, `github|<id>`, `oidc|<id>`) is
/// kept verbatim.
fn normalize_subject(subject: &str) -> Option<String> {
    if let Some((_, tail)) = subject.rsplit_once('|')
        && let Some(rest) = tail.strip_prefix("user_")
        && !rest.is_empty()
        && tail.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Some(tail.to_owned());
    }
    let bridged = ["google-oauth2|", "github|", "oidc|", "auth0|"]
        .iter()
        .any(|prefix| subject.starts_with(prefix));
    if bridged && subject.matches('|').count() == 1 {
        return Some(subject.to_owned());
    }
    None
}

/// `GET` the usage CSV with the browser-equivalent headers. A 401/403 means the
/// session expired (re-login in Cursor); any other non-2xx or transport failure
/// is returned as an error for the caller to record as unreadable.
fn fetch_csv(cookie: &str) -> Result<String, ()> {
    let response = ureq::get(CSV_URL)
        .timeout(Duration::from_secs(20))
        .set("Cookie", cookie)
        .set("Referer", REFERER)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "*/*")
        .call()
        .map_err(|_| ())?;
    response.into_string().map_err(|_| ())
}

/// Parse the dashboard CSV. Columns are resolved by header name (Cursor inserts
/// columns over time), and untrusted numeric cells are parsed leniently.
fn parse_csv(
    csv: &str,
    floor: Option<OffsetDateTime>,
    local_offset: UtcOffset,
    collection: &mut Collection,
) {
    let mut lines = csv.lines();
    let Some(header) = lines.next() else {
        return;
    };
    let columns: Vec<String> = split_csv_line(header);
    let index = |name: &str| columns.iter().position(|column| column == name);

    let (Some(date_idx), Some(model_idx), Some(input_idx), Some(cache_read_idx), Some(output_idx)) = (
        index("Date"),
        index("Model"),
        index("Input (w/o Cache Write)"),
        index("Cache Read"),
        index("Output Tokens"),
    ) else {
        // Header shape we don't recognize — treat as unreadable rather than
        // silently emitting zero events.
        collection.stats.parse_errors += 1;
        return;
    };
    let input_with_idx = index("Input (w/ Cache Write)");
    let total_idx = index("Total Tokens");
    let cost_idx = index("Cost");
    let model_label_idx = index("Max Mode"); // only used to detect the trailing layout

    let _ = model_label_idx;

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        collection.stats.lines_seen += 1;
        let fields = split_csv_line(line);
        let cell = |idx: usize| fields.get(idx).map_or("", String::as_str);

        let Some(timestamp) = OffsetDateTime::parse(cell(date_idx), &Rfc3339)
            .ok()
            .map(|ts| ts.to_offset(local_offset))
        else {
            collection.stats.parse_errors += 1;
            continue;
        };
        if floor.is_some_and(|floor| timestamp < floor) {
            continue;
        }

        let input_without = parse_u64(cell(input_idx));
        let input_with = input_with_idx.map_or(input_without, |idx| parse_u64(cell(idx)));
        let usage = TokenUsage {
            input_tokens: input_without,
            output_tokens: parse_u64(cell(output_idx)),
            // "Input (w/ Cache Write)" includes the cache-write tokens that
            // "Input (w/o Cache Write)" omits; their difference is the write.
            cache_creation_input_tokens: input_with.saturating_sub(input_without),
            cache_read_input_tokens: parse_u64(cell(cache_read_idx)),
            ..TokenUsage::default()
        };

        // The CSV's own `Total Tokens` is a checksum; a mismatch is a soft
        // warning (count it) but the row is still recorded.
        if let Some(total) = total_idx.map(|idx| parse_u64(cell(idx)))
            && total != usage.token_volume()
        {
            collection.stats.parse_errors += 1;
        }

        let reported_cost_usd = cost_idx.map(&cell).and_then(|raw| raw.parse::<f64>().ok());

        let model = {
            let value = cell(model_idx).trim();
            (!value.is_empty()).then(|| value.to_owned())
        };

        collection.usage_events.push(UsageEvent {
            timestamp: Some(timestamp),
            session_id: None,
            model,
            source_kind: SourceKind::Main,
            attribution_agent: None,
            project: None, // Cursor usage events carry no project identifier.
            usage,
            reported_cost_usd,
        });
    }
}

/// Parse an unsigned count from a possibly-quoted CSV cell; anything malformed
/// is treated as zero rather than aborting the row.
fn parse_u64(cell: &str) -> u64 {
    cell.trim().parse::<u64>().unwrap_or(0)
}

/// Split one CSV record, honoring `"`-quoted fields with embedded commas and
/// doubled `""` escapes.
fn split_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    current.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                current.push(ch);
            }
        } else {
            match ch {
                '"' => in_quotes = true,
                ',' => fields.push(std::mem::take(&mut current)),
                _ => current.push(ch),
            }
        }
    }
    fields.push(current);
    fields
}

fn systemtime_to_offset(time: SystemTime) -> Option<OffsetDateTime> {
    let nanos = time.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    OffsetDateTime::from_unix_timestamp_nanos(i128::try_from(nanos).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CSV: &str = "Date,Cloud Agent ID,Automation ID,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost\n\
        \"2026-06-22T13:09:44.478Z\",\"\",\"\",\"free\",\"composer-2.5-fast\",\"No\",\"100\",\"76054\",\"723008\",\"8093\",\"999999\",\"0.71\"\n";

    fn parse(csv: &str, floor: Option<OffsetDateTime>) -> Collection {
        let mut collection = Collection::new(Provider::Cursor, std::path::PathBuf::from("x"));
        parse_csv(csv, floor, UtcOffset::UTC, &mut collection);
        collection
    }

    #[test]
    fn parses_tokens_and_reported_cost() {
        let collection = parse(CSV, None);
        assert_eq!(collection.usage_events.len(), 1);
        let event = &collection.usage_events[0];
        assert_eq!(event.usage.input_tokens, 76054);
        assert_eq!(event.usage.output_tokens, 8093);
        assert_eq!(event.usage.cache_read_input_tokens, 723_008);
        // 100 (w/ cache write) - 76054 (w/o) saturates to 0, not a wraparound.
        assert_eq!(event.usage.cache_creation_input_tokens, 0);
        assert_eq!(event.model.as_deref(), Some("composer-2.5-fast"));
        assert!(event.project.is_none());
        assert_eq!(event.reported_cost_usd, Some(0.71));
    }

    #[test]
    fn cache_write_is_the_difference_of_the_two_input_columns() {
        let csv = "Date,Model,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens\n\
            \"2026-06-22T13:09:44Z\",\"m\",\"500\",\"200\",\"10\",\"5\"\n";
        let event = &parse(csv, None).usage_events[0];
        assert_eq!(event.usage.input_tokens, 200);
        assert_eq!(event.usage.cache_creation_input_tokens, 300);
    }

    #[test]
    fn total_tokens_mismatch_is_a_soft_warning_not_a_drop() {
        // Total says 999 but the parts sum to 807155 → counted, row still kept.
        let collection = parse(CSV, None);
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.stats.parse_errors, 1);
    }

    #[test]
    fn floor_drops_older_rows() {
        let floor = OffsetDateTime::parse("2027-01-01T00:00:00Z", &Rfc3339).unwrap();
        assert!(parse(CSV, Some(floor)).usage_events.is_empty());
    }

    #[test]
    fn unknown_header_is_recorded_not_silently_empty() {
        let collection = parse("Something,Else\n\"a\",\"b\"\n", None);
        assert!(collection.usage_events.is_empty());
        assert_eq!(collection.stats.parse_errors, 1);
    }

    #[test]
    fn native_subject_collapses_to_user_id() {
        assert_eq!(
            normalize_subject("auth0|user_01ABC").as_deref(),
            Some("user_01ABC")
        );
        assert_eq!(
            normalize_subject("github|user_01ABC").as_deref(),
            Some("user_01ABC")
        );
    }

    #[test]
    fn bridged_oauth_subject_is_kept_verbatim() {
        assert_eq!(
            normalize_subject("google-oauth2|209269195").as_deref(),
            Some("google-oauth2|209269195")
        );
        assert_eq!(normalize_subject("weird-value"), None);
    }
}
