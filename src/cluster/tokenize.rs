//! Turning a log line into a template by masking its variable parts.
//!
//! `Connection to 10.0.1.5:5432 failed after 1234ms (attempt 3)`
//! becomes
//! `Connection to <*> failed after <*> (attempt <*>)`
//!
//! # The tension this code lives in
//!
//! Mask too eagerly and distinct failures collapse into one cluster, hiding
//! the difference that matters. Mask too little and a single failure shatters
//! into thousands of clusters, which is just the raw log with extra steps.
//!
//! The rules below are therefore deliberately *specific* — each recognises a
//! shape that is unambiguously an identifier, a quantity or an address —
//! rather than one broad "looks variable" heuristic. Where a rule risks
//! catching a real word, the tests pin the boundary explicitly.
//!
//! Punctuation is preserved around a masked token, because structure is what
//! makes a template readable: `(attempt <*>)` says more than `<*> <*> <*>`.

/// The placeholder standing in for a masked value.
pub const MASK: &str = "<*>";

/// Beyond this many tokens a line is truncated. Log lines carrying a whole
/// serialised payload would otherwise produce enormous templates that are
/// slow to compare and useless to read.
const MAX_TOKENS: usize = 60;

/// Maximum rendered template length. Structured payloads can be enormous;
/// beyond this the tail adds nothing a reader or a grouping key needs.
const MAX_TEMPLATE_CHARS: usize = 600;

/// Reduce a log message to its template.
///
/// Structured messages are handled first. A JSON line carries no whitespace,
/// so word-splitting alone would treat the whole object as one unmaskable
/// token and give every distinct payload its own cluster — measured against
/// JSON-formatted service logs, 3,000 events produced 3,000 clusters, which is
/// the raw log with extra steps.
pub fn template(message: &str) -> String {
    let t = match json_template(message) {
        Some(t) => t,
        None => words_template(message),
    };
    truncate_chars(&t, MAX_TEMPLATE_CHARS)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Build a template from a JSON payload's *structure*.
///
/// Keys are kept verbatim and values masked selectively, because the keys and
/// the low-cardinality values are exactly what distinguishes one failure from
/// another. Masking every value would merge an INFO reboot with a CRITICAL
/// permissions error; masking none is the bug described above.
fn json_template(message: &str) -> Option<String> {
    let trimmed = message.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    Some(render(&value, 0, None))
}

/// Nested payloads beyond this depth collapse, bounding both work and output.
const MAX_DEPTH: usize = 6;
/// Only the first few array elements shape the template; the rest repeat it.
const MAX_ARRAY_ELEMS: usize = 3;

fn render(v: &serde_json::Value, depth: usize, key: Option<&str>) -> String {
    use serde_json::Value;
    if depth > MAX_DEPTH {
        return MASK.to_string();
    }
    // `"code": 500` and `"code": 200` are different outcomes, not one shape
    // with a varying number, so an identity key keeps its value.
    let identity = key.is_some_and(is_identity_key);
    match v {
        Value::Number(n) if identity && n.to_string().len() <= MAX_IDENTITY_VALUE => n.to_string(),
        // Otherwise a number is a quantity, never identity.
        Value::Number(_) => MASK.to_string(),
        // Booleans and null are low-cardinality and meaningful; keep them.
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::String(s) if identity && s.len() <= MAX_IDENTITY_VALUE => format!("\"{s}\""),
        Value::String(s) => render_string(s),
        Value::Array(items) => {
            let mut parts: Vec<String> = items
                .iter()
                .take(MAX_ARRAY_ELEMS)
                .map(|i| render(i, depth + 1, key))
                .collect();
            if items.len() > MAX_ARRAY_ELEMS {
                parts.push("…".to_string());
            }
            format!("[{}]", parts.join(","))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, val)| format!("\"{k}\":{}", render(val, depth + 1, Some(k))))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

/// A JSON string value: mask it if it is a value, keep it if it is a label.
///
/// A string containing whitespace is treated as an embedded message and run
/// through the word tokenizer, so `"msg":"failed after 1234ms"` clusters with
/// `"msg":"failed after 9ms"` rather than splitting on the duration.
fn render_string(s: &str) -> String {
    if s.trim().is_empty() {
        return format!("\"{s}\"");
    }
    if s.split_whitespace().count() > 1 {
        return format!("\"{}\"", words_template(s));
    }
    if is_variable(s) {
        MASK.to_string()
    } else {
        format!("\"{s}\"")
    }
}

/// The whitespace-splitting path, for unstructured lines.
fn words_template(message: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    // Newlines inside one event (a stack trace, say) are flattened: the first
    // line identifies the error, and the frames below it are detail the
    // exemplar preserves.
    for (i, raw) in message.split_whitespace().enumerate() {
        if i >= MAX_TOKENS {
            out.push("…".to_string());
            break;
        }
        out.push(mask_token(raw));
    }
    out.join(" ")
}

/// Mask one whitespace-delimited token, preserving surrounding punctuation.
fn mask_token(raw: &str) -> String {
    let (lead, core, trail) = split_punctuation(raw);
    if core.is_empty() {
        return raw.to_string();
    }
    if is_variable(core) {
        return format!("{lead}{MASK}{trail}");
    }
    // logfmt: `duration=1234ms` must mask the value while keeping the key,
    // or every distinct duration becomes its own cluster.
    if let Some((key, value)) = core.split_once('=')
        && !key.is_empty()
        && !value.is_empty()
        && !key.contains('"')
        && is_variable(value.trim_matches('"'))
    {
        return format!("{lead}{key}={MASK}{trail}");
    }
    // A long token dense with structural punctuation is a payload that did
    // not parse -- truncated JSON, most often, since log pipelines clip long
    // lines. Without this it stays one unmaskable token and every distinct
    // payload becomes its own cluster.
    if let Some(t) = structural_template(core) {
        return format!("{lead}{t}{trail}");
    }
    raw.to_string()
}

/// Field names whose value *is* the identity of the record, and so must
/// survive masking even when it is a bare number.
///
/// Windows Event ID 4625 (failed logon) and 5145 (share access check) are
/// entirely different events; masking both to `<*>` merges them and destroys
/// the distinction the log exists to record. The same applies to an HTTP
/// status: 200 and 500 must never share a cluster.
///
/// Matching is exact and case-insensitive, never a substring, because
/// `EventRecordID` and `RequestId` are unique per record and *must* mask.
/// A substring test for "id" would preserve them and shatter the cluster.
const IDENTITY_KEYS: [&str; 17] = [
    "eventid",
    "event_id",
    "level",
    "severity",
    "code",
    "status",
    "statuscode",
    "status_code",
    "errorcode",
    "error_code",
    "type",
    "kind",
    "result",
    "state",
    "priority",
    "opcode",
    "task",
];

/// Values longer than this are not enum-like, whatever the key is called.
const MAX_IDENTITY_VALUE: usize = 12;

fn is_identity_key(key: &str) -> bool {
    let k = key
        .trim_matches(['/', '<', '>', '"', '\'', ' '])
        .to_ascii_lowercase();
    IDENTITY_KEYS.contains(&k.as_str())
}

/// Characters that delimit fields inside a structured payload.
///
/// `<`, `>` and `'` are here for XML. Windows Event Log records — which FSx
/// audit logs carry, and which the `host eventlog` collector will return —
/// pack every element together with no whitespace, so without these the
/// entire record is one unmaskable token. Measured on Windows Event Log XML records:
/// 3,000 events produced 3,000 clusters, no compression whatsoever.
const STRUCTURAL: [char; 11] = ['{', '}', '[', ']', ',', ':', '=', '"', '<', '>', '\''];

/// Split an unparsed payload on its structural punctuation and mask the
/// variable pieces, keeping the delimiters so the shape stays recognisable.
///
/// Returns `None` when the token is too short or too plain to be a payload,
/// leaving ordinary words untouched.
fn structural_template(t: &str) -> Option<String> {
    if t.len() < 20 || t.chars().filter(|c| STRUCTURAL.contains(c)).count() < 2 {
        return None;
    }
    let mut out = String::new();
    let mut field = String::new();
    // The element or attribute name immediately preceding the current field,
    // so `<EventID>5145</EventID>` can keep its value while
    // `<EventRecordID>1234567</EventRecordID>` masks its own.
    let mut prev = String::new();
    for ch in t.chars() {
        if STRUCTURAL.contains(&ch) {
            push_field(&mut out, &field, &prev);
            if !field.is_empty() {
                prev = field.clone();
            }
            field.clear();
            out.push(ch);
        } else {
            field.push(ch);
        }
    }
    push_field(&mut out, &field, &prev);
    Some(out)
}

fn push_field(out: &mut String, field: &str, prev: &str) {
    if field.is_empty() {
        return;
    }
    if is_identity_key(prev) && field.len() <= MAX_IDENTITY_VALUE {
        out.push_str(field);
    } else if is_variable(field) {
        out.push_str(MASK);
    } else {
        out.push_str(field);
    }
}

/// Peel leading and trailing punctuation off a token.
///
/// `(attempt` keeps its bracket; `3)` masks to `<*>)`. Characters that carry
/// meaning *inside* a value — `.`, `:`, `/`, `-`, `_` — are never peeled from
/// the front, or `10.0.1.5` would lose its shape.
fn split_punctuation(raw: &str) -> (&str, &str, &str) {
    const LEAD: [char; 6] = ['(', '[', '{', '<', '"', '\''];
    const TRAIL: [char; 10] = [')', ']', '}', '>', '"', '\'', ',', ';', '!', '?'];

    let start = raw.len() - raw.trim_start_matches(LEAD).len();
    let rest = &raw[start..];
    let end = rest.trim_end_matches(TRAIL).len();
    // A trailing period is ambiguous: sentence punctuation, or part of a
    // version or hostname. Only peel it when what remains still looks like a
    // word, so `v1.2.3.` peels but `10.0.1.5` does not.
    let mut core = &rest[..end];
    let mut trail_extra = 0;
    if core.ends_with('.')
        && !core
            .trim_end_matches('.')
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.')
    {
        core = &core[..core.len() - 1];
        trail_extra = 1;
    }
    (&raw[..start], core, &rest[end - trail_extra..])
}

/// Whether a token is a value rather than part of the message's wording.
fn is_variable(t: &str) -> bool {
    is_null_placeholder(t)
        || is_numeric(t)
        || is_uuid(t)
        || is_timestamp(t)
        || is_ip(t)
        || is_hex(t)
        || is_path(t)
        || is_url(t)
        || is_arn(t)
        || is_email(t)
        || is_resource_id(t)
        || is_long_opaque_id(t)
}

/// A lone `-`, the "no value" marker in W3C, IIS, ALB and Common Log Format.
///
/// Without this a field that is empty in one record and populated in the next
/// yields two templates for the same event. Observed on W3C-format web server logs, where
/// it left a tail of single-event clusters beside a cluster of 655.
///
/// Masking a separator that is constant across every line is harmless: it
/// renders as `<*>` everywhere and so merges nothing that was distinct.
fn is_null_placeholder(t: &str) -> bool {
    t == "-"
}

/// `1234`, `-5`, `1.5`, `1,234`, `99%`, `1234ms`, `4.2s`, `12KB`.
///
/// A trailing unit is included so `1234ms` and `2100ms` land in one cluster;
/// splitting the number from its unit would leave `ms` as literal text and
/// work either way, but keeping them together reads better in the template.
fn is_numeric(t: &str) -> bool {
    let body = t.strip_prefix(['-', '+']).unwrap_or(t);
    let digits_end = body
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != ',')
        .unwrap_or(body.len());
    if digits_end == 0 {
        return false;
    }
    let (num, unit) = body.split_at(digits_end);
    if !num.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    // Reject `1.2.3.4` here so the IP rule owns it, and reject version-like
    // strings, which are usually worth keeping literal.
    if num.matches('.').count() > 1 {
        return false;
    }
    unit.is_empty()
        || (unit.len() <= 4 && unit.chars().all(|c| c.is_ascii_alphabetic() || c == '%'))
}

fn is_uuid(t: &str) -> bool {
    let parts: Vec<&str> = t.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12]
            == [
                parts[0].len(),
                parts[1].len(),
                parts[2].len(),
                parts[3].len(),
                parts[4].len(),
            ]
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// ISO-8601-ish instants and bare clock times: `2026-09-04T10:00:00Z`,
/// `2026-09-04`, `10:04:12.331`.
fn is_timestamp(t: &str) -> bool {
    let digits = t.chars().filter(char::is_ascii_digit).count();
    if digits < 6 {
        return false;
    }
    let shaped = t
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '-' | ':' | 'T' | 'Z' | '.' | '+'));
    shaped && (t.matches('-').count() >= 2 || t.matches(':').count() >= 2)
}

/// `10.0.1.5` and `10.0.1.5:5432`.
fn is_ip(t: &str) -> bool {
    let host = t.split_once(':').map_or(t, |(h, p)| {
        if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() {
            h
        } else {
            t
        }
    });
    let octets: Vec<&str> = host.split('.').collect();
    octets.len() == 4
        && octets
            .iter()
            .all(|o| !o.is_empty() && o.len() <= 3 && o.chars().all(|c| c.is_ascii_digit()))
}

/// `0x1f4a` or a bare hex run long enough not to be a word.
fn is_hex(t: &str) -> bool {
    if let Some(rest) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_hexdigit());
    }
    // 16 is deliberately conservative: `deadbeef` is 8 and could be a word,
    // `decaf` is a word at 5. Hashes and request ids are longer than this.
    t.len() >= 16 && t.chars().all(|c| c.is_ascii_hexdigit())
}

/// An absolute Unix path or a Windows drive path.
fn is_path(t: &str) -> bool {
    (t.starts_with('/') && t.matches('/').count() >= 2 && t.len() > 2)
        || (t.len() > 3
            && t.as_bytes()[1] == b':'
            && (t.contains('\\'))
            && t.as_bytes()[0].is_ascii_alphabetic())
}

fn is_url(t: &str) -> bool {
    t.contains("://") && t.len() > 8
}

fn is_arn(t: &str) -> bool {
    t.starts_with("arn:") && t.matches(':').count() >= 4
}

fn is_email(t: &str) -> bool {
    match t.split_once('@') {
        Some((user, host)) => !user.is_empty() && host.contains('.') && !host.starts_with('.'),
        None => false,
    }
}

/// AWS-style resource identifiers: `i-0abc123def456`, `vol-0123abcd`,
/// `eni-...`, `snap-...`. A short lowercase prefix, a hyphen, then hex.
fn is_resource_id(t: &str) -> bool {
    match t.split_once('-') {
        Some((prefix, rest)) => {
            !prefix.is_empty()
                && prefix.len() <= 8
                && prefix.chars().all(|c| c.is_ascii_lowercase())
                && rest.len() >= 8
                && rest.chars().all(|c| c.is_ascii_hexdigit())
        }
        None => false,
    }
}

/// A long token mixing letters and digits — request ids, trace ids, tokens.
///
/// The length floor is high on purpose. At 16 characters this cannot catch an
/// English word, and it will not catch `application/json` (no digits) or
/// `HTTP/1.1` (too short).
fn is_long_opaque_id(t: &str) -> bool {
    t.len() >= 16
        && t.chars().any(|c| c.is_ascii_digit())
        && t.chars().any(|c| c.is_ascii_alphabetic())
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert two lines share a template — the same failure, one cluster.
    fn same(a: &str, b: &str) {
        assert_eq!(
            template(a),
            template(b),
            "\n  {a}\n  {b}\nshould cluster together"
        );
    }

    /// Assert two lines do *not* — different failures, kept apart.
    fn differ(a: &str, b: &str) {
        assert_ne!(
            template(a),
            template(b),
            "\n  {a}\n  {b}\nshould stay separate"
        );
    }

    #[test]
    fn the_documented_example_masks_exactly_the_variable_parts() {
        assert_eq!(
            template("Connection to 10.0.1.5:5432 failed after 1234ms (attempt 3)"),
            "Connection to <*> failed after <*> (attempt <*>)"
        );
    }

    #[test]
    fn the_same_failure_with_different_values_is_one_cluster() {
        same(
            "Connection to 10.0.1.5:5432 failed after 1234ms (attempt 3)",
            "Connection to 10.0.9.71:5432 failed after 87ms (attempt 11)",
        );
    }

    #[test]
    fn different_failures_are_not_merged() {
        // The whole point of clustering is losing volume, not distinctions.
        differ(
            "Connection to 10.0.1.5:5432 failed",
            "Connection to 10.0.1.5:5432 refused",
        );
        differ("Timeout reading from disk", "Timeout writing to disk");
    }

    #[test]
    fn aws_resource_ids_and_uuids_are_masked() {
        // Deliberately sequential rather than realistic. The masking only
        // cares about the shape -- `i-` plus 17 hex -- and a fixture that
        // looks like a real instance id makes every future PII scan of this
        // repository report a false positive that someone has to re-triage.
        same(
            "Instance i-0123456789abcdef0 entered state stopping",
            "Instance i-0fedcba987654321f entered state stopping",
        );
        same(
            "request 3f2504e0-4f89-11d3-9a0c-0305e82c3301 failed",
            "request 7c9e6679-7425-40de-944b-e07fc1f90ae7 failed",
        );
        assert_eq!(
            template("volume vol-0123abcd detached"),
            "volume <*> detached"
        );
    }

    #[test]
    fn timestamps_paths_urls_arns_and_emails_are_masked() {
        assert_eq!(template("at 2026-09-04T10:00:00Z start"), "at <*> start");
        assert_eq!(template("read /var/log/httpd/error_log ok"), "read <*> ok");
        assert_eq!(
            template("GET https://api.example.com/v1/x 200"),
            "GET <*> <*>"
        );
        assert_eq!(
            template("denied arn:aws:iam::123456789012:role/App now"),
            "denied <*> now"
        );
        assert_eq!(template("user user@example.com in"), "user <*> in");
    }

    #[test]
    fn windows_paths_are_masked_for_the_iis_and_coldfusion_collectors() {
        assert_eq!(
            template("opening C:\\inetpub\\logs\\LogFiles\\W3SVC1 now"),
            "opening <*> now"
        );
    }

    #[test]
    fn numbers_keep_their_units_so_durations_cluster() {
        same("took 1234ms", "took 87ms");
        same("freed 12KB", "freed 900KB");
        same("cpu at 99%", "cpu at 3%");
        same("delta -5", "delta 42");
    }

    // ---- the over-masking edge: real words must survive -------------------

    #[test]
    fn ordinary_words_are_never_masked() {
        let msg = "Connection refused by upstream server during handshake";
        assert_eq!(template(msg), msg, "no token here is a value");
    }

    #[test]
    fn short_mixed_tokens_that_look_like_words_survive() {
        // These are the tokens most at risk from a loose "has a digit" rule.
        for t in ["HTTP/1.1", "utf-8", "s3", "ec2", "sha256", "base64", "v2"] {
            assert_eq!(template(t), t, "{t} should not be masked");
        }
    }

    #[test]
    fn mime_types_and_versions_stay_literal() {
        assert_eq!(
            template("Accept: application/json"),
            "Accept: application/json"
        );
        assert_eq!(template("nginx/1.24.0 ready"), "nginx/1.24.0 ready");
    }

    #[test]
    fn the_w3c_no_value_marker_clusters_with_a_populated_field() {
        // An IIS field that is `-` on one request and a value on the next is
        // the same event, not two.
        same("GET /health - 200 12", "GET /health 10.0.1.5 200 12");
        // A hyphen inside a word is untouched.
        assert_eq!(template("well-formed request"), "well-formed request");
    }

    #[test]
    fn an_unrecognised_identifier_is_left_alone_by_the_tokenizer() {
        // Host names have no format -- `db-master`, `srv12345`, `web01-a98a`
        // are all plausible -- so there is no shape to match on, and guessing
        // at one masks ordinary words. Lines that differ only by such a token
        // are reunited by variant families in `cluster::template`, which uses
        // how a token varies across the corpus rather than how it looks.
        differ(
            "W3SVC1 web01-a98a GET /health",
            "W3SVC1 web01-ea46 GET /health",
        );
    }

    #[test]
    fn hyphenated_words_are_not_mistaken_for_identifiers() {
        // `face`, `beef` and `cafe` are all valid hexadecimal, which is why
        // matching on "looks like hex" is a trap for ordinary English.
        for t in [
            "well-face",
            "add-beef",
            "grand-cafe",
            "stone-dead",
            "utf-8",
            "sha-256",
        ] {
            assert_eq!(template(t), t, "{t} should not be masked");
        }
    }

    #[test]
    fn a_short_hex_looking_word_is_not_treated_as_a_hash() {
        // `deadbeef` and `decaf` are hex-valid but read as words; the 16-char
        // floor is what keeps them out.
        assert_eq!(template("cafe deadbeef decaf"), "cafe deadbeef decaf");
    }

    // ---- the under-masking edge -------------------------------------------

    #[test]
    fn long_opaque_request_ids_are_masked() {
        same(
            "x-amzn-RequestId a1b2c3d4e5f6a7b8c9d0 handled",
            "x-amzn-RequestId 9f8e7d6c5b4a39281706 handled",
        );
    }

    #[test]
    fn a_lambda_stack_trace_collapses_to_its_first_line_shape() {
        let a = "ERROR Invoke Error {\"errorType\":\"TypeError\"}\n    at handler (/var/task/index.js:12:9)\n    at Runtime";
        let b = "ERROR Invoke Error {\"errorType\":\"TypeError\"}\n    at handler (/var/task/index.js:88:3)\n    at Runtime";
        same(a, b);
    }

    #[test]
    fn an_extremely_long_line_is_truncated_rather_than_producing_a_vast_template() {
        let long = (0..500)
            .map(|i| format!("tok{i}x"))
            .collect::<Vec<_>>()
            .join(" ");
        let t = template(&long);
        assert!(t.ends_with('…'), "truncation marker present");
        assert!(t.split_whitespace().count() <= MAX_TOKENS + 1);
    }

    #[test]
    fn empty_and_whitespace_messages_are_handled() {
        assert_eq!(template(""), "");
        assert_eq!(template("   \n\t "), "");
    }

    #[test]
    fn punctuation_structure_is_preserved_around_masks() {
        // `<*> <*> <*>` would be unreadable; the brackets carry the meaning.
        assert_eq!(
            template("retry (attempt 3) of [5] max"),
            "retry (attempt <*>) of [<*>] max"
        );
        assert_eq!(
            template("failed after 30s, retrying"),
            "failed after <*>, retrying"
        );
    }

    // ---- structured payloads ---------------------------------------------
    //
    // Representative JSON service-log lines. Word-splitting
    // alone gave 3,000 clusters for 3,000 events, because a JSON object has
    // no whitespace to split on.

    const DRIFT_A: &str = r#"{"severity":"ERROR","gatewayTimeDriftSeconds":"12300","source":"sgw-0A1B2C3D","type":"GatewayClockOutOfSync","gateway":"sgw-0A1B2C3D","timestamp":"1700000000000"}"#;
    const DRIFT_B: &str = r#"{"severity":"ERROR","gatewayTimeDriftSeconds":"6300","source":"sgw-0A1B2C3D","type":"GatewayClockOutOfSync","gateway":"sgw-0A1B2C3D","timestamp":"1700000600000"}"#;
    const REBOOT: &str = r#"{"severity":"INFO","source":"AvailabilityMonitor","type":"Reboot","gateway":"sgw-0A1B2C3D","timestamp":"1700000000000"}"#;

    #[test]
    fn json_events_differing_only_in_values_form_one_cluster() {
        same(DRIFT_A, DRIFT_B);
    }

    #[test]
    fn json_templates_keep_keys_and_low_cardinality_labels() {
        // Keeping `severity` and `type` literal is what preserves the
        // distinction between failures; the numbers carry no identity.
        let t = template(DRIFT_A);
        assert!(t.contains(r#""severity":"ERROR""#), "{t}");
        assert!(t.contains(r#""type":"GatewayClockOutOfSync""#), "{t}");
        assert!(t.contains(r#""gatewayTimeDriftSeconds":<*>"#), "{t}");
        assert!(t.contains(r#""timestamp":<*>"#), "{t}");
        assert!(
            !t.contains("12300"),
            "the drifting value must not survive: {t}"
        );
    }

    #[test]
    fn different_json_event_types_are_never_merged() {
        // An INFO reboot and an ERROR clock drift are different incidents.
        differ(DRIFT_A, REBOOT);
    }

    #[test]
    fn a_message_embedded_in_a_json_field_is_itself_templated() {
        same(
            r#"{"level":"error","msg":"upstream failed after 1234ms"}"#,
            r#"{"level":"error","msg":"upstream failed after 9ms"}"#,
        );
        differ(
            r#"{"level":"error","msg":"upstream failed after 1234ms"}"#,
            r#"{"level":"error","msg":"upstream refused after 1234ms"}"#,
        );
    }

    #[test]
    fn booleans_and_nulls_are_kept_because_they_carry_meaning() {
        let t = template(r#"{"retryable":true,"cause":null,"attempts":3}"#);
        assert!(t.contains("\"retryable\":true"), "{t}");
        assert!(t.contains("\"cause\":null"), "{t}");
        assert!(t.contains("\"attempts\":<*>"), "{t}");
    }

    #[test]
    fn nested_objects_and_long_arrays_are_bounded() {
        let t = template(r#"{"a":{"b":{"c":{"d":{"e":{"f":{"g":1}}}}}},"list":[1,2,3,4,5,6,7]}"#);
        assert!(t.contains('…'), "long array collapses: {t}");
        assert!(t.len() < 200, "bounded: {t}");
    }

    /// Shape of a Windows Security audit record (Event ID 5145, a file share
    /// access check). Real records are far longer; this keeps the structure.
    fn win_event(event_id: &str, ip: &str, guid: &str) -> String {
        format!(
            "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'>\
             <System><Provider Name='Microsoft-Windows-Security-Auditing' Guid='{{{guid}}}'/>\
             <EventID>{event_id}</EventID><Version>0</Version><Level>0</Level>\
             <EventRecordID>1234567</EventRecordID></System>\
             <EventData><Data Name='IpAddress'>{ip}</Data></EventData></Event>"
        )
    }

    #[test]
    fn windows_event_xml_clusters_by_event_id() {
        // Same event id from different addresses is one cluster ...
        same(
            &win_event("5145", "10.0.1.5", "54849625-5478-4994-a5ba-3e3b0328c30d"),
            &win_event("5145", "10.0.9.71", "54849625-5478-4994-a5ba-3e3b0328c30d"),
        );
        // ... but a different event id is a different event, and must not be
        // masked away: EventID is the identity of a Windows log record.
        differ(
            &win_event("5145", "10.0.1.5", "54849625-5478-4994-a5ba-3e3b0328c30d"),
            &win_event("4625", "10.0.1.5", "54849625-5478-4994-a5ba-3e3b0328c30d"),
        );
    }

    #[test]
    fn xml_element_names_survive_while_their_values_are_masked() {
        let t = template(&win_event(
            "5145",
            "10.0.1.5",
            "54849625-5478-4994-a5ba-3e3b0328c30d",
        ));
        assert!(
            t.contains("EventID"),
            "element names identify the record: {t}"
        );
        assert!(!t.contains("10.0.1.5"), "the address is a value: {t}");
        assert!(!t.contains("1234567"), "the record id is a value: {t}");
    }

    #[test]
    fn a_malformed_json_line_falls_back_to_word_splitting() {
        // Truncated JSON is common in real logs; it must still cluster.
        let a = r#"{"severity":"ERROR","drift":"12300"#;
        let b = r#"{"severity":"ERROR","drift":"6300"#;
        assert!(!template(a).is_empty());
        same(a, b);
    }

    #[test]
    fn logfmt_pairs_mask_the_value_and_keep_the_key() {
        assert_eq!(
            template("level=error duration=1234ms host=10.0.1.5 msg=timeout"),
            "level=error duration=<*> host=<*> msg=timeout"
        );
        same("level=error duration=1234ms", "level=error duration=7ms");
        differ("level=error duration=1234ms", "level=warn duration=1234ms");
    }

    #[test]
    fn a_very_long_structured_payload_is_truncated() {
        let big: String = format!(
            "{{{}}}",
            (0..200)
                .map(|i| format!(r#""key{i}":"value{i}""#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let t = template(&big);
        assert!(
            t.chars().count() <= MAX_TEMPLATE_CHARS + 1,
            "len {}",
            t.chars().count()
        );
        assert!(t.ends_with('…'));
    }

    #[test]
    fn an_alb_access_log_line_clusters_by_shape() {
        let a = "https 2026-09-04T10:00:00.123456Z app/my-alb/abc123 10.0.1.5:41234 10.0.2.9:8080 0.001 0.030 0.000 502 502 340 1200";
        let b = "https 2026-09-04T10:00:31.998811Z app/my-alb/abc123 10.0.7.2:55010 10.0.2.9:8080 0.002 0.041 0.000 502 502 512 900";
        same(a, b);
        // A different status code must not merge with the 502s.
        let c = "https 2026-09-04T10:00:31.998811Z app/my-alb/abc123 10.0.7.2:55010 10.0.2.9:8080 0.002 0.041 0.000 200 200 512 900";
        assert_eq!(template(&b.replace("502 502", "200 200")), template(c));
    }
}
