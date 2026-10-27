//! JSON parsing bounded by the structure of the input, shared by every
//! collector that reads JSON lines or JSON columns.

use serde_json::Value;

use super::FileEvents;

/// Array elements and object entries one document may hold, counted as the
/// `[`, `{` and `,` outside strings. Parsed into a `Value`, each one costs
/// tens of bytes however short its text, so a line of `[0,0,…]` grew ~18× in
/// memory and four 5 KB `.zst` rollouts reached 6.5 GB across threads. Real
/// logs stay far below it; text and other scalars are not limited by it,
/// only by the line cap.
const MAX_STRUCTURE: usize = 1 << 18;

/// `None` for malformed JSON and for a document past `MAX_STRUCTURE`, which
/// is rejected before anything is built.
pub(in crate::collector) fn parse_bounded(text: &str) -> Option<Value> {
    if !within_structure_budget(text.as_bytes()) {
        return None;
    }
    serde_json::from_str(text).ok()
}

/// One JSONL line: `None` for a blank line, or, counted as a parse error, for
/// invalid UTF-8 and for anything `parse_bounded` rejects.
pub(in crate::collector) fn parse_line(line: &[u8], events: &mut FileEvents) -> Option<Value> {
    let Ok(text) = std::str::from_utf8(line) else {
        events.parse_errors += 1;
        return None;
    };
    if text.trim().is_empty() {
        return None;
    }
    let value = parse_bounded(text);
    if value.is_none() {
        events.parse_errors += 1;
    }
    value
}

fn within_structure_budget(text: &[u8]) -> bool {
    // Each counted byte is one byte of the text, so a shorter text can't
    // exceed the budget and ordinary lines skip the scan.
    if text.len() <= MAX_STRUCTURE {
        return true;
    }
    let mut count = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in text {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' | b'{' | b',' => {
                count += 1;
                if count > MAX_STRUCTURE {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

#[cfg(test)]
pub(in crate::collector) mod tests {
    use super::*;

    /// A valid JSON array whose structure is just past the budget, yet only
    /// about half a megabyte long.
    pub(in crate::collector) fn over_structured_line() -> String {
        format!("[{}0]", "0,".repeat(MAX_STRUCTURE))
    }

    #[test]
    fn structure_past_the_budget_is_rejected_before_parsing() {
        let at_budget = format!("[{}0]", "0,".repeat(MAX_STRUCTURE - 1));
        assert!(parse_bounded(&at_budget).is_some());
        assert!(parse_bounded(&over_structured_line()).is_none());
    }

    #[test]
    fn separators_inside_strings_do_not_count() {
        let text = format!(r#"{{"text":"{}\"[{{,"}}"#, ",[{".repeat(MAX_STRUCTURE));
        let value = parse_bounded(&text).expect("one string field is within budget");
        assert_eq!(
            value["text"].as_str().map(str::len),
            Some(3 * MAX_STRUCTURE + 4)
        );
    }

    #[test]
    fn blank_lines_are_skipped_and_bad_lines_counted() {
        let mut events = FileEvents::default();

        assert!(parse_line(b"  \t", &mut events).is_none());
        assert!(parse_line(b"{\"a\":\xff}", &mut events).is_none());
        assert!(parse_line(b"not json", &mut events).is_none());
        assert!(parse_line(over_structured_line().as_bytes(), &mut events).is_none());
        assert_eq!(
            parse_line(b"{\"a\":1}", &mut events).map(|v| v["a"].clone()),
            Some(1.into())
        );
        assert_eq!(events.parse_errors, 3);
    }
}
