//! Adversarial input for the clustering tokenizer.
//!
//! Log message content is written by whatever produces the logs, so the
//! tokenizer must treat it as untrusted. It does byte-index slicing in
//! several places, and a byte index landing inside a multi-byte UTF-8
//! character panics. A panic here kills a diagnostic run mid-incident.

use awsdiag::cluster::template::{LogEvent, cluster};
use awsdiag::cluster::tokenize::template;
use chrono::{TimeZone, Utc};

fn nasty_inputs() -> Vec<String> {
    let mut v: Vec<String> = vec![
        "café".into(),
        "(café)".into(),
        "«quoted»".into(),
        "123é".into(),
        "é123".into(),
        "12é34ms".into(),
        "—".into(),
        "…".into(),
        "🔥".into(),
        "(🔥)".into(),
        "🔥-1234".into(),
        "a🔥=1234".into(),
        "key=🔥".into(),
        "\u{0}".into(),
        "\u{feff}leading bom".into(),
        "\u{202e}reversed".into(),
        "e\u{0301}accent".into(),
        "{\"a\":".into(),
        "{{{{{{{{{{".into(),
        "]]]]]]]]]]".into(),
        "{\"é\":\"🔥\",\"n\":1}".into(),
        "<Event><EventID>é</EventID>".into(),
        "=".into(),
        "==".into(),
        "-".into(),
        "--".into(),
        ":::::".into(),
        "\"\"\"\"".into(),
        "1.2.3.4.5".into(),
        "999999999999999999999999".into(),
        "-".repeat(50),
        "0x".into(),
        "0xé".into(),
    ];
    v.push(format!("{}{}", "[".repeat(200), "]".repeat(200)));
    v.push(format!("{{\"a\":{}}}", "[".repeat(100) + &"]".repeat(100)));
    v.push("é".repeat(5000));
    v.push("🔥 ".repeat(2000));
    v.push(format!("{{{}}}", "\"k\":\"v\",".repeat(2000)));
    v
}

#[test]
fn tokenizing_adversarial_input_never_panics() {
    for input in nasty_inputs() {
        let out = template(&input);
        // The documented cap is 600 characters plus a truncation marker.
        assert!(
            out.chars().count() <= 601,
            "template of {} chars from {:?}",
            out.chars().count(),
            &input[..input.len().min(40)]
        );
    }
}

#[test]
fn every_byte_prefix_of_a_multibyte_line_is_safe() {
    // Truncated log lines are routine: pipelines clip them mid-character.
    let line = "café 🔥 {\"host\":\"wéb01-a98a\",\"code\":500} —dash— 1234ms";
    for end in 0..line.len() {
        if line.is_char_boundary(end) {
            let _ = template(&line[..end]);
        }
    }
}

#[test]
fn clustering_adversarial_input_never_panics() {
    let now = Utc.with_ymd_and_hms(2026, 9, 4, 10, 0, 0).unwrap();
    let events: Vec<LogEvent> = nasty_inputs()
        .into_iter()
        .map(|message| LogEvent {
            timestamp: now,
            stream: "s\u{fffd}".into(),
            message,
        })
        .collect();
    let n = events.len();
    let c = cluster(&events, now, now + chrono::Duration::hours(1), 60);
    assert_eq!(
        c.iter().map(|c| c.count).sum::<usize>(),
        n,
        "every event accounted for once"
    );
}
