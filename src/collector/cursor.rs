//! Cursor collector — auto-detected, and the only provider that reaches the
//! network.
//!
//! Cursor keeps no per-request token counts on disk (the local stores hold chat
//! text, accepted-line attribution, and the auth token — never token usage), so
//! the figures live only behind Cursor's web dashboard. We replay that
//! dashboard's own CSV export the way the browser does: read the session JWT
//! from the local Electron `state.vscdb`, build the `WorkosCursorSessionToken`
//! cookie, and `GET` the usage CSV.
//!
//! This is the one collector that sends anything off the machine (the user's own
//! session cookie, to Cursor, to read the user's own usage). It's auto-detected
//! from a signed-in `state.vscdb` like the other providers, and — like them —
//! simply skips (no tab, no error) when the pieces aren't all there: no Cursor
//! store, signed out (no token), or the fetch doesn't come back. So no request
//! is made unless you're actually signed in. It is also an undocumented endpoint
//! that can change without notice.
//!
//! Cursor's usage events carry **no project/repo identifier** and its models
//! (`composer-2.5-fast`, …) aren't in the `LiteLLM` table, so the CSV's own
//! `Cost` column is carried through as `UsageEvent::reported_cost_usd`.

use std::path::Path;
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;
use tracing::debug;

use crate::model::{Collection, Provider, SourceKind, TokenUsage, UsageEvent};

const CSV_URL: &str = "https://cursor.com/api/dashboard/export-usage-events-csv?strategy=tokens";
const REFERER: &str = "https://www.cursor.com/settings";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

const ACCESS_TOKEN_SQL: &str = "SELECT value FROM ItemTable WHERE key = 'cursorAuth/accessToken'";

/// Collect Cursor usage. `token_override` (from `CURSOR_TOKEN`) wins over the
/// local DB so a relocated or unreadable store can still be used. `cli_config`
/// is `~/.cursor/cli-config.json`, a fallback source for the account id (the JWT
/// `sub` is authoritative).
pub fn collect(
    state_db: &Path,
    cli_config: &Path,
    token_override: Option<&str>,
    mtime_floor: Option<SystemTime>,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::Cursor, state_db.to_path_buf());

    let jwt = if let Some(token) = token_override {
        // An explicit CURSOR_TOKEN that's too short or carries control
        // characters is unusable — a real failure, not signed-out.
        if let Some(token) = sanitize_token(token) {
            token
        } else {
            collection.stats.unreadable_files += 1;
            return collection;
        }
    } else {
        match read_access_token(state_db) {
            Ok(Some(token)) => token,
            // Signed out (no token row): stay silent — no tab, no request, and
            // not counted as unreadable.
            Ok(None) => return collection,
            // The store exists but couldn't be opened/read — a real failure.
            Err(()) => {
                collection.stats.unreadable_files += 1;
                return collection;
            }
        }
    };
    let Some(user_id) = account_id(cli_config, &jwt) else {
        collection.stats.unreadable_files += 1;
        return collection;
    };

    // Percent-encode the account id: bridged-OAuth ids contain `|`
    // (`google-oauth2|123`), which a strict cookie parser / CDN in front of
    // cursor.com can reject. The server percent-decodes the value (the `::`
    // separator is sent as `%3A%3A`), so `%7C` round-trips back to `|`.
    let cookie = format!(
        "WorkosCursorSessionToken={}%3A%3A{jwt}",
        user_id.replace('|', "%7C")
    );
    // Auth expiry, a network failure, or an endpoint change all land here;
    // surface it as an unreadable source rather than a panic, and log the reason
    // since this is an undocumented endpoint that's hard to debug blind.
    let csv = match fetch_csv(&cookie) {
        Ok(csv) => csv,
        Err(reason) => {
            debug!("cursor: usage fetch failed: {reason}");
            collection.stats.unreadable_files += 1;
            return collection;
        }
    };
    collection.stats.files_seen += 1;

    let floor = mtime_floor.map(OffsetDateTime::from);
    parse_csv(&csv, floor, local_offset, &mut collection);

    collection.stats.usage_events = collection.usage_events.len();
    collection
}

/// Read `cursorAuth/accessToken` from the Electron `state.vscdb` (read-only, so
/// SQLite never writes Cursor's live store). `Ok(None)` is the signed-out state
/// (store present, no token row) — distinct from `Err(())`, an actual open/read
/// failure — so the caller can stay silent when signed out instead of reporting
/// an unreadable file.
fn read_access_token(state_db: &Path) -> Result<Option<String>, ()> {
    if !state_db.exists() {
        return Ok(None);
    }
    let conn =
        Connection::open_with_flags(state_db, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|_| ())?;
    let _ = conn.busy_timeout(Duration::from_millis(500));
    match conn.query_row(ACCESS_TOKEN_SQL, [], |row| row.get::<_, String>(0)) {
        // A present row that doesn't sanitize to a usable token (too short, or
        // control characters) reads as `None` — effectively signed out — rather
        // than a hard error.
        Ok(token) => Ok(sanitize_token(&token)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(_) => Err(()),
    }
}

/// A usable session token. Cursor stores the JWT raw, but other VS Code
/// `ItemTable` values are JSON-serialized strings, so surrounding quotes are
/// stripped defensively (a JWT never contains `"`). The token must be long
/// enough to be a JWT and free of ASCII control characters — a CR/LF would let a
/// crafted store or a malformed `CURSOR_TOKEN` inject extra `Cookie` headers.
fn sanitize_token(raw: &str) -> Option<String> {
    let token = raw.trim().trim_matches('"').trim();
    (token.len() >= 10 && token.bytes().all(|byte| !byte.is_ascii_control()))
        .then(|| token.to_owned())
}

/// The account id for the cookie. The JWT `sub` is authoritative — it's the
/// account that owns this very token, so it always matches — and `cli-config.json`
/// `authInfo.authId` is only a fallback for a malformed JWT (a stale or
/// different-account cli-config would otherwise 401 a perfectly good token).
fn account_id(cli_config: &Path, jwt: &str) -> Option<String> {
    if let Some(id) = jwt_subject(jwt).and_then(|subject| normalize_subject(&subject)) {
        return Some(id);
    }
    std::fs::read_to_string(cli_config)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|cfg| {
            cfg.pointer("/authInfo/authId")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .and_then(|subject| normalize_subject(&subject))
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
/// `…|user_XXX` suffix collapses to `user_XXX` (native Cursor accounts), while
/// any other bridged-OAuth subject (`<provider>|<id>`, for any provider) is kept
/// verbatim — no provider allowlist, so Microsoft / GitLab / SAML logins work too.
fn normalize_subject(subject: &str) -> Option<String> {
    // A `…|user_XXX` suffix or an already-bare `user_XXX` collapses to `user_XXX`.
    let tail = subject.rsplit_once('|').map_or(subject, |(_, tail)| tail);
    if let Some(rest) = tail.strip_prefix("user_")
        && !rest.is_empty()
        && tail.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Some(tail.to_owned());
    }
    // Otherwise accept any single-pipe `<provider>|<id>` with non-empty halves.
    match subject.split_once('|') {
        Some((provider, id)) if !provider.is_empty() && !id.is_empty() && !id.contains('|') => {
            Some(subject.to_owned())
        }
        _ => None,
    }
}

/// `GET` the usage CSV with the browser-equivalent headers. The `Err` carries a
/// short reason for the log: a 401/403 means the session expired (re-login in
/// Cursor), other statuses and transport failures pass their own message.
fn fetch_csv(cookie: &str) -> Result<String, String> {
    // Don't follow redirects: the session cookie is attached by hand, so a 3xx
    // from the endpoint must never carry it to another host. With redirects
    // disabled the cookie only ever reaches cursor.com; a redirect is refused
    // below rather than chased.
    let agent = ureq::builder().redirects(0).build();
    let response = agent
        .get(CSV_URL)
        // The fetch is synchronous and the dashboard waits on it, so keep the
        // cap short — the CSV is tiny; a slow network shouldn't hang startup.
        .timeout(Duration::from_secs(5))
        .set("Cookie", cookie)
        .set("Referer", REFERER)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "*/*")
        .call()
        .map_err(|err| match err {
            ureq::Error::Status(401 | 403, _) => {
                "session expired — re-login in Cursor to refresh the token".to_owned()
            }
            ureq::Error::Status(code, _) => format!("HTTP {code} from the usage endpoint"),
            ureq::Error::Transport(transport) => format!("network error: {transport}"),
        })?;
    // With redirects disabled a 3xx returns as `Ok`; refuse it instead of
    // reading a redirect target's body (the cookie was never sent there).
    let status = response.status();
    if (300..400).contains(&status) {
        return Err(format!(
            "unexpected redirect (HTTP {status}) from the usage endpoint"
        ));
    }
    response
        .into_string()
        .map_err(|err| format!("reading the response body: {err}"))
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
    // Strip a UTF-8 BOM so the first column name still matches "Date".
    let header = header.trim_start_matches('\u{feff}');
    // Trim each header cell so `"Date, Model"`-style spacing still resolves.
    let columns: Vec<String> = split_csv_line(header)
        .into_iter()
        .map(|column| column.trim().to_owned())
        .collect();
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

        // The two input columns are disjoint, not nested: `Total Tokens` is
        // `Input (w/o Cache Write)` + `Input (w/ Cache Write)` + `Cache Read` +
        // `Output Tokens`. So `Input (w/ Cache Write)` *is* the cache-write count
        // (default 0 if the column is absent), not a superset to subtract from.
        //
        // Required token cells parse strictly: an empty cell is a legitimate 0,
        // but a present-but-unparseable value (e.g. a thousands-separated
        // "1,234" after a format change) drops the whole row rather than
        // silently recording 0 tokens — degrade to less data, never a wrong one.
        let cache_creation = match input_with_idx {
            Some(idx) => parse_required(cell(idx)),
            None => Some(0),
        };
        let (Some(input_tokens), Some(output_tokens), Some(cache_read), Some(cache_creation)) = (
            parse_required(cell(input_idx)),
            parse_required(cell(output_idx)),
            parse_required(cell(cache_read_idx)),
            cache_creation,
        ) else {
            collection.stats.parse_errors += 1;
            continue;
        };
        let usage = TokenUsage {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens: cache_creation,
            cache_read_input_tokens: cache_read,
            ..TokenUsage::default()
        };

        // The CSV's own `Total Tokens` is a checksum; a mismatch is a soft
        // warning (count it) but the row is still recorded.
        if let Some(total) = total_idx.map(|idx| parse_u64(cell(idx)))
            && total != usage.token_volume()
        {
            collection.stats.parse_errors += 1;
        }

        // Every Cursor row carries an authoritative cost. A non-numeric label
        // (e.g. "Free"/"Included" for in-plan requests) is a reported $0, not a
        // missing value — so a present Cost column always yields `Some`, never a
        // `None` that would wrongly route the row to LiteLLM pricing.
        let reported_cost_usd = cost_idx.map(|idx| {
            cell(idx)
                .trim()
                .trim_start_matches('$')
                .parse::<f64>()
                .ok()
                // Reject NaN / inf / negative from a malformed cell; a label like
                // "Free" lands here too and reads as a reported $0.
                .filter(|cost| cost.is_finite() && *cost >= 0.0)
                .unwrap_or(0.0)
        });

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
/// is treated as zero rather than aborting the row. Used for the `Total Tokens`
/// checksum, which is advisory — a bad value there shouldn't drop the row.
fn parse_u64(cell: &str) -> u64 {
    cell.trim().parse::<u64>().unwrap_or(0)
}

/// Parse a *required* token cell. An empty cell is a legitimate 0; a
/// present-but-unparseable value returns `None` so the caller drops the row
/// instead of recording a wrong 0.
fn parse_required(cell: &str) -> Option<u64> {
    let cell = cell.trim();
    if cell.is_empty() {
        return Some(0);
    }
    cell.parse::<u64>().ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    // Total = 100 (w/ cache write) + 76054 (w/o) + 723008 (cache read) + 8093
    // (output) = 807255, so this row checksums cleanly.
    const CSV: &str = "Date,Cloud Agent ID,Automation ID,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost\n\
        \"2026-06-22T13:09:44.478Z\",\"\",\"\",\"free\",\"composer-2.5-fast\",\"No\",\"100\",\"76054\",\"723008\",\"8093\",\"807255\",\"0.71\"\n";

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
        // "Input (w/ Cache Write)" is the cache-write count directly.
        assert_eq!(event.usage.cache_creation_input_tokens, 100);
        assert_eq!(event.model.as_deref(), Some("composer-2.5-fast"));
        assert!(event.project.is_none());
        assert_eq!(event.reported_cost_usd, Some(0.71));
        // The row checksums against Total Tokens, so no parse warning.
        assert_eq!(collection.stats.parse_errors, 0);
    }

    #[test]
    fn cache_write_comes_from_the_w_cache_write_column() {
        // The two input columns are disjoint: cache-write is the "w/ Cache Write"
        // value itself, not the difference between the columns.
        let csv = "Date,Model,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens\n\
            \"2026-06-22T13:09:44Z\",\"m\",\"500\",\"200\",\"10\",\"5\"\n";
        let event = &parse(csv, None).usage_events[0];
        assert_eq!(event.usage.input_tokens, 200);
        assert_eq!(event.usage.cache_creation_input_tokens, 500);
    }

    #[test]
    fn free_cost_label_is_reported_zero_not_litellm() {
        // A non-numeric Cost ("Free") is a reported $0, not a missing value.
        let csv = "Date,Model,Input (w/o Cache Write),Cache Read,Output Tokens,Cost\n\
            \"2026-06-22T13:09:44Z\",\"claude-sonnet\",\"10\",\"0\",\"5\",\"Free\"\n";
        let event = &parse(csv, None).usage_events[0];
        assert_eq!(event.reported_cost_usd, Some(0.0));
    }

    #[test]
    fn cost_with_dollar_sign_parses() {
        let csv = "Date,Model,Input (w/o Cache Write),Cache Read,Output Tokens,Cost\n\
            \"2026-06-22T13:09:44Z\",\"m\",\"10\",\"0\",\"5\",\"$0.42\"\n";
        let event = &parse(csv, None).usage_events[0];
        assert_eq!(event.reported_cost_usd, Some(0.42));
    }

    #[test]
    fn total_tokens_mismatch_is_a_soft_warning_not_a_drop() {
        // Total claims 999 but the parts sum to more → counted, row still kept.
        let csv = "Date,Model,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens\n\
            \"2026-06-22T13:09:44Z\",\"m\",\"100\",\"76054\",\"723008\",\"8093\",\"999\"\n";
        let collection = parse(csv, None);
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.stats.parse_errors, 1);
    }

    #[test]
    fn unparseable_token_cell_drops_row_not_records_zero() {
        // A thousands-separated "1,234" (a plausible format change) is NOT a 0 —
        // the row is dropped and counted as a parse error rather than silently
        // recording wrong token counts.
        let csv = "Date,Model,Input (w/o Cache Write),Cache Read,Output Tokens\n\
            \"2026-06-22T13:09:44Z\",\"m\",\"1,234\",\"0\",\"5\"\n";
        let collection = parse(csv, None);
        assert!(collection.usage_events.is_empty());
        assert_eq!(collection.stats.parse_errors, 1);
    }

    #[test]
    fn empty_token_cell_is_zero_not_a_drop() {
        // An absent value is a legitimate 0; the row is still recorded.
        let csv = "Date,Model,Input (w/o Cache Write),Cache Read,Output Tokens\n\
            \"2026-06-22T13:09:44Z\",\"m\",\"\",\"0\",\"5\"\n";
        let event = &parse(csv, None).usage_events[0];
        assert_eq!(event.usage.input_tokens, 0);
        assert_eq!(event.usage.output_tokens, 5);
    }

    #[test]
    fn control_chars_in_token_are_rejected() {
        // A CR/LF in the stored token would let a crafted store inject extra
        // Cookie headers; sanitize_token refuses it.
        assert!(sanitize_token("abcdefghij\r\nInjected: 1").is_none());
        assert!(sanitize_token("short").is_none());
        assert_eq!(
            sanitize_token("  \"abcdefghij\"  ").as_deref(),
            Some("abcdefghij")
        );
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
        // An already-bare id (no provider prefix) is accepted as-is.
        assert_eq!(
            normalize_subject("user_01ABC").as_deref(),
            Some("user_01ABC")
        );
    }

    #[test]
    fn bridged_oauth_subject_is_kept_verbatim() {
        assert_eq!(
            normalize_subject("google-oauth2|209269195").as_deref(),
            Some("google-oauth2|209269195")
        );
        // No provider allowlist: any single-pipe subject is kept verbatim.
        assert_eq!(
            normalize_subject("microsoft|abc123").as_deref(),
            Some("microsoft|abc123")
        );
        assert_eq!(normalize_subject("weird-value"), None);
        assert_eq!(normalize_subject("a|b|c"), None);
    }
}
