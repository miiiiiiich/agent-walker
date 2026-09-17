use std::io::Read;
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
/// local DB so a relocated or unreadable store can still be used.
pub fn collect(
    state_db: &Path,
    cli_config: &Path,
    token_override: Option<&str>,
    mtime_floor: Option<SystemTime>,
    local_offset: UtcOffset,
) -> Collection {
    let mut collection = Collection::new(Provider::Cursor, state_db.to_path_buf());

    let jwt = if let Some(token) = token_override {
        if let Some(token) = sanitize_token(token) {
            token
        } else {
            collection.stats.unreadable_files += 1;
            return collection;
        }
    } else {
        match read_access_token(state_db) {
            Ok(Some(token)) => token,
            Ok(None) => return collection,
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
        // A present-but-unusable row is treated as signed out (silent), unlike
        // a bad CURSOR_TOKEN, which is a user error.
        Ok(token) => Ok(sanitize_token(&token)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(_) => Err(()),
    }
}

fn sanitize_token(raw: &str) -> Option<String> {
    let token = raw.trim().trim_matches('"').trim();
    let is_jwt_byte = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.');
    (token.len() >= 10 && token.bytes().all(is_jwt_byte)).then(|| token.to_owned())
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
    let tail = subject.rsplit_once('|').map_or(subject, |(_, tail)| tail);
    if let Some(rest) = tail.strip_prefix("user_")
        && !rest.is_empty()
        && tail.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Some(tail.to_owned());
    }
    match subject.split_once('|') {
        Some((provider, id)) if is_safe_subject_part(provider) && is_safe_subject_part(id) => {
            Some(subject.to_owned())
        }
        _ => None,
    }
}

fn is_safe_subject_part(part: &str) -> bool {
    !part.is_empty()
        && part.bytes().all(|byte| {
            !byte.is_ascii_control() && !matches!(byte, b' ' | b'"' | b',' | b';' | b'\\' | b'|')
        })
}

const MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;

fn fetch_csv(cookie: &str) -> Result<String, String> {
    fetch_csv_from(CSV_URL, cookie)
}

fn fetch_csv_from(url: &str, cookie: &str) -> Result<String, String> {
    // Don't follow redirects: the session cookie is attached by hand, so a 3xx
    // from the endpoint must never carry it to another host. With redirects
    // disabled the cookie only ever reaches cursor.com; a redirect is refused
    // below rather than chased.
    let agent = ureq::Agent::config_builder()
        .max_redirects(0)
        // Disable environment proxies so the cookie leaves only over a direct connection.
        .proxy(None)
        // The fetch is synchronous and the dashboard waits on it, so keep the
        // cap short — the CSV is tiny; a slow network shouldn't hang startup.
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .new_agent();
    let mut response = agent
        .get(url)
        .header("Cookie", cookie)
        .header("Referer", REFERER)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "*/*")
        .call()
        .map_err(|err| match err {
            ureq::Error::StatusCode(401 | 403) => {
                "session expired — re-login in Cursor to refresh the token".to_owned()
            }
            ureq::Error::StatusCode(code) => format!("HTTP {code} from the usage endpoint"),
            ureq::Error::Timeout(_) => "the usage endpoint did not answer in time".to_owned(),
            other => format!("network error: {other}"),
        })?;
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        return Err(format!(
            "unexpected redirect (HTTP {status}) from the usage endpoint"
        ));
    }
    // Cap the *decoded* body: ureq's own limit counts compressed bytes.
    let mut csv = String::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_string(&mut csv)
        .map_err(|err| format!("reading the response body: {err}"))?;
    if csv.len() as u64 > MAX_BODY_BYTES {
        return Err("usage export larger than expected".to_owned());
    }
    Ok(csv)
}

/// Resolve CSV columns by header name because Cursor inserts columns over time.
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

        let Some(usage) = row_usage(
            &fields,
            input_idx,
            output_idx,
            cache_read_idx,
            input_with_idx,
        ) else {
            collection.stats.parse_errors += 1;
            continue;
        };

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
            attribution_skill: None,
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

/// The input columns are disjoint: subtracting cache writes from fresh input would undercount.
fn row_usage(
    fields: &[String],
    input_idx: usize,
    output_idx: usize,
    cache_read_idx: usize,
    input_with_idx: Option<usize>,
) -> Option<TokenUsage> {
    // A row truncated before the token columns would read its missing cells as
    // "" and parse to 0; require the row to actually reach every required column
    // so a short row is a parse error, not a silent zero.
    let required_max = [
        Some(input_idx),
        Some(output_idx),
        Some(cache_read_idx),
        input_with_idx,
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0);
    if fields.len() <= required_max {
        return None;
    }
    let cell = |idx: usize| fields.get(idx).map_or("", String::as_str);
    Some(TokenUsage {
        input_tokens: parse_required(cell(input_idx))?,
        output_tokens: parse_required(cell(output_idx))?,
        cache_read_input_tokens: parse_required(cell(cache_read_idx))?,
        cache_creation_input_tokens: match input_with_idx {
            Some(idx) => parse_required(cell(idx))?,
            None => 0,
        },
        ..TokenUsage::default()
    })
}

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
        assert_eq!(event.usage.cache_creation_input_tokens, 100);
        assert_eq!(event.model.as_deref(), Some("composer-2.5-fast"));
        assert!(event.project.is_none());
        assert_eq!(event.reported_cost_usd, Some(0.71));
        assert_eq!(collection.stats.parse_errors, 0);
    }

    #[test]
    fn cache_write_comes_from_the_w_cache_write_column() {
        let csv = "Date,Model,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens\n\
            \"2026-06-22T13:09:44Z\",\"m\",\"500\",\"200\",\"10\",\"5\"\n";
        let event = &parse(csv, None).usage_events[0];
        assert_eq!(event.usage.input_tokens, 200);
        assert_eq!(event.usage.cache_creation_input_tokens, 500);
    }

    #[test]
    fn free_cost_label_is_reported_zero_not_litellm() {
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
        let csv = "Date,Model,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens\n\
            \"2026-06-22T13:09:44Z\",\"m\",\"100\",\"76054\",\"723008\",\"8093\",\"999\"\n";
        let collection = parse(csv, None);
        assert_eq!(collection.usage_events.len(), 1);
        assert_eq!(collection.stats.parse_errors, 1);
    }

    #[test]
    fn unparseable_token_cell_drops_row_not_records_zero() {
        let csv = "Date,Model,Input (w/o Cache Write),Cache Read,Output Tokens\n\
            \"2026-06-22T13:09:44Z\",\"m\",\"1,234\",\"0\",\"5\"\n";
        let collection = parse(csv, None);
        assert!(collection.usage_events.is_empty());
        assert_eq!(collection.stats.parse_errors, 1);
    }

    #[test]
    fn truncated_row_is_a_parse_error_not_a_zero() {
        let csv = "Date,Model,Input (w/o Cache Write),Cache Read,Output Tokens\n\
            \"2026-06-22T13:09:44Z\",\"m\"\n";
        let collection = parse(csv, None);
        assert!(collection.usage_events.is_empty());
        assert_eq!(collection.stats.parse_errors, 1);
    }

    #[test]
    fn empty_token_cell_is_zero_not_a_drop() {
        let csv = "Date,Model,Input (w/o Cache Write),Cache Read,Output Tokens\n\
            \"2026-06-22T13:09:44Z\",\"m\",\"\",\"0\",\"5\"\n";
        let event = &parse(csv, None).usage_events[0];
        assert_eq!(event.usage.input_tokens, 0);
        assert_eq!(event.usage.output_tokens, 5);
    }

    #[test]
    fn only_jwt_chars_in_token_are_accepted() {
        assert_eq!(
            sanitize_token("eyJhbGci.eyJzdWIi.sig-na_ture").as_deref(),
            Some("eyJhbGci.eyJzdWIi.sig-na_ture")
        );
        assert_eq!(
            sanitize_token("  \"abcdefghij\"  ").as_deref(),
            Some("abcdefghij")
        );
        // A CR/LF would inject extra Cookie headers; a ';'/'='/space could split
        // or confuse the cookie — all rejected by the JWT-charset allowlist.
        assert!(sanitize_token("abcdefghij\r\nInjected: 1").is_none());
        assert!(sanitize_token("abcdefghij; evil=1").is_none());
        assert!(sanitize_token("abcdefghij=padding").is_none());
        assert!(sanitize_token("short").is_none());
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
        assert_eq!(
            normalize_subject("microsoft|abc123").as_deref(),
            Some("microsoft|abc123")
        );
        assert_eq!(
            normalize_subject("okta|user@company.com").as_deref(),
            Some("okta|user@company.com")
        );
        assert_eq!(
            normalize_subject("saml|alice+tag@example.com").as_deref(),
            Some("saml|alice+tag@example.com")
        );
        assert_eq!(normalize_subject("weird-value"), None);
        assert_eq!(normalize_subject("a|b|c"), None);
    }

    #[test]
    fn bridged_subject_with_unsafe_chars_is_rejected() {
        // The subject is decoded from the JWT payload, so a `sub` carrying a
        // CR/LF or a cookie metacharacter must not reach the Cookie header.
        assert_eq!(normalize_subject("google-oauth2|123\r\nInjected: 1"), None);
        assert_eq!(normalize_subject("google-oauth2|123; evil=1"), None);
        assert_eq!(normalize_subject("prov ider|123"), None);
    }

    fn one_shot_server(response: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/usage.csv");
        let response = response.replace("{addr}", &addr.to_string());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (url, rx)
    }

    /// A 3xx is refused, not followed: the cookie must never travel to the
    /// redirect target. Exactly one request reaches the server.
    #[test]
    fn redirects_are_refused_without_being_followed() {
        let (url, rx) = one_shot_server(
            "HTTP/1.1 302 Found\r\nLocation: http://{addr}/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let error = fetch_csv_from(&url, "WorkosCursorSessionToken=secret").unwrap_err();
        assert!(error.contains("unexpected redirect (HTTP 302)"), "{error}");
        let request = rx.recv().unwrap().to_ascii_lowercase();
        assert!(request.contains("cookie: workoscursorsessiontoken=secret"));
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "redirect was followed"
        );
    }

    #[test]
    fn an_expired_session_is_named_as_such() {
        let (url, _rx) = one_shot_server(
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let error = fetch_csv_from(&url, "x").unwrap_err();
        assert!(error.contains("session expired"), "{error}");
    }
}
