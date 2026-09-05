//! Rendering an envelope in the three output formats.

use crate::common::envelope::Envelope;
use crate::common::flags::OutputFormat;
use serde::Serialize;
use serde_json::Value;

pub fn render<T: Serialize>(env: &Envelope<T>, format: OutputFormat) -> String {
    match format {
        OutputFormat::Json => serde_json::to_string_pretty(env).unwrap_or_default(),
        OutputFormat::Ndjson => env
            .data
            .iter()
            .filter_map(|row| serde_json::to_string(row).ok())
            .collect::<Vec<_>>()
            .join("\n"),
        OutputFormat::Text => {
            let value = serde_json::to_value(&env.data).unwrap_or(Value::Null);
            let mut out = table(value.as_array().map(Vec::as_slice).unwrap_or(&[]));
            // Truncation is invisible in a bare table, and a human scanning
            // text output is exactly who would otherwise miss it.
            if env.truncated {
                out.push_str(&format!(
                    "\n({} rows shown; result truncated — narrow the window or raise --limit)",
                    env.count
                ));
            }
            out
        }
    }
}

/// Render rows of flat JSON objects as aligned columns.
///
/// Columns come from the first row and are then held fixed, so every line has
/// the same shape even when later rows carry extra or missing keys.
fn table(rows: &[Value]) -> String {
    let Some(first) = rows.first().and_then(Value::as_object) else {
        return String::new();
    };
    let headers: Vec<&str> = first.keys().map(String::as_str).collect();

    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| headers.iter().map(|h| scalar(row.get(*h))).collect())
        .collect();

    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            cells
                .iter()
                .map(|r| r[i].chars().count())
                .chain([h.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut out = String::new();
    for (i, h) in headers.iter().enumerate() {
        out.push_str(&pad(h, widths[i], i + 1 == headers.len()));
    }
    out.push('\n');
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            out.push_str(&pad(c, widths[i], i + 1 == row.len()));
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn pad(s: &str, width: usize, last: bool) -> String {
    if last {
        s.to_string()
    } else {
        format!("{s:<width$}  ")
    }
}

/// Flatten a cell to a single-line string. Nested structures collapse to
/// compact JSON rather than being dropped, so text mode never silently hides
/// a field that JSON mode would show.
fn scalar(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "-".to_string(),
        Some(Value::String(s)) => flatten(s),
        Some(other) => flatten(&other.to_string()),
    }
}

/// Make one field safe to render on a terminal.
///
/// Two hazards from the same source: log message content is written by
/// whatever produces the logs, and text mode renders it straight to a TTY.
///
/// - Embedded newlines break column alignment for every row after them.
/// - Control characters are *interpreted* by the terminal. A line carrying
///   `ESC[2K` or `ESC[A` erases or overwrites what is already on screen, so
///   an operator reading `logs tail` during an incident can be shown a
///   materially different picture than the log contains. OSC sequences reach
///   the clipboard on some terminals.
///
/// JSON and NDJSON are unaffected -- `serde_json` escapes every byte below
/// 0x20 -- so this gap existed only on the path rendered to a person.
///
/// Control characters become a visible `\xNN` rather than being dropped, so
/// their presence is evident instead of the text quietly reading as something
/// it is not.
fn flatten(s: &str) -> String {
    if !s.chars().any(char::is_control) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for c in s.chars() {
        match c {
            '\n' | '\r' | '\t' => {
                if !out.is_empty() && !last_was_space {
                    out.push(' ');
                    last_was_space = true;
                }
            }
            c if c.is_control() => {
                out.push_str(&format!("\\x{:02x}", c as u32));
                last_was_space = false;
            }
            c => {
                out.push(c);
                last_was_space = false;
            }
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Serialize)]
    struct Row {
        profile: &'static str,
        account: &'static str,
    }

    fn env(rows: Vec<Row>) -> Envelope<Row> {
        Envelope::new("whoami", json!({}), rows)
    }

    fn sample() -> Envelope<Row> {
        env(vec![
            Row {
                profile: "gamma-admin",
                account: "123456789012",
            },
            Row {
                profile: "beta-power",
                account: "111122223333",
            },
        ])
    }

    #[test]
    fn json_emits_the_whole_envelope_not_just_the_rows() {
        let out = render(&sample(), OutputFormat::Json);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["count"], json!(2));
        assert_eq!(v["data"][0]["profile"], json!("gamma-admin"));
    }

    #[test]
    fn ndjson_emits_one_parseable_object_per_row_and_no_envelope() {
        let out = render(&sample(), OutputFormat::Ndjson);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        for l in lines {
            serde_json::from_str::<Value>(l).expect("each line parses alone");
        }
        assert!(!out.contains("\"ok\""));
    }

    #[test]
    fn text_aligns_columns_under_headers() {
        let out = render(&sample(), OutputFormat::Text);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "header plus one line per row");
        // Declaration order, not alphabetical: `preserve_order` is enabled on
        // serde_json precisely so `profile` leads and the table reads naturally.
        assert!(lines[0].starts_with("profile"), "got header: {}", lines[0]);
        assert!(lines[0].find("profile").unwrap() < lines[0].find("account").unwrap());
        // "gamma-admin" is the widest value, so the account column starts
        // at the same offset on every line.
        let col = lines[0].find("account").unwrap();
        assert_eq!(lines[1].find("123456789012").unwrap(), col);
        assert_eq!(lines[2].find("111122223333").unwrap(), col);
    }

    #[test]
    fn text_announces_truncation_because_a_table_cannot_show_it() {
        let out = render(
            &env(vec![Row {
                profile: "a",
                account: "b",
            }])
            .truncated(true),
            OutputFormat::Text,
        );
        assert!(out.contains("truncated"), "got: {out}");
    }

    #[test]
    fn empty_results_render_without_panicking_in_every_format() {
        let e = env(vec![]);
        assert_eq!(render(&e, OutputFormat::Text), "");
        assert_eq!(render(&e, OutputFormat::Ndjson), "");
        assert_eq!(
            serde_json::from_str::<Value>(&render(&e, OutputFormat::Json)).unwrap()["count"],
            json!(0)
        );
    }

    #[test]
    fn a_multiline_value_cannot_break_the_table() {
        // A multi-line AWS error chain in one cell used to spill across rows
        // and destroy alignment for everything below it.
        let rows = vec![json!({"a": "one\ntwo\nthree", "b": "x"})];
        let out = table(&rows);
        assert_eq!(out.lines().count(), 2, "header + one row: {out:?}");
        assert!(out.contains("one two three"));
    }

    #[test]
    fn terminal_control_sequences_in_log_content_are_neutralised() {
        // Log content is written by whatever produces the logs. Rendered raw
        // to a TTY, `ESC[2K` and `ESC[A` erase and overwrite lines already on
        // screen -- so an operator reading `logs tail` mid-incident can be
        // shown a different picture than the log contains.
        let hostile = "before\u{1b}[2K\u{1b}[Aerased";
        let out = table(&[json!({ "message": hostile })]);
        assert!(
            !out.contains('\u{1b}'),
            "no ESC reaches the terminal: {out:?}"
        );
        assert!(out.contains("\\x1b"), "its presence stays visible: {out:?}");
        assert!(
            out.contains("before") && out.contains("erased"),
            "text is preserved"
        );
    }

    #[test]
    fn other_control_bytes_are_neutralised_too() {
        // Backspace, NUL, and the C1 range are all interpreted by terminals.
        for c in ['\u{0}', '\u{8}', '\u{7}', '\u{9b}'] {
            let out = table(&[json!({ "m": format!("a{c}b") })]);
            assert!(
                !out.contains(c),
                "control {:#x} survived: {out:?}",
                c as u32
            );
        }
    }

    #[test]
    fn json_output_was_never_affected_and_still_is_not() {
        // serde_json escapes every byte below 0x20, so the machine-readable
        // paths were always safe; this pins that.
        let env = Envelope::new("logs tail", json!({}), vec!["a\u{1b}[2Kb"]);
        let out = render(&env, OutputFormat::Json);
        assert!(!out.contains('\u{1b}'));
        let nd = render(&env, OutputFormat::Ndjson);
        assert!(!nd.contains('\u{1b}'));
    }

    #[test]
    fn ordinary_text_is_returned_unchanged() {
        // The fast path must not disturb normal content, including unicode.
        for t in ["plain", "café 🔥", "a-b_c.d", ""] {
            assert_eq!(flatten(t), t);
        }
    }

    #[test]
    fn missing_and_nested_values_survive_text_rendering() {
        let rows = vec![json!({"a": 1, "b": {"n": 2}}), json!({"a": null})];
        let out = table(&rows);
        assert!(out.contains("{\"n\":2}"), "nested value kept: {out}");
        assert!(out.contains('-'), "missing value marked: {out}");
    }
}
