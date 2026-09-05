//! Turning a findings document into one self-contained HTML file.

use crate::report::model::Findings;

const SHELL: &str = include_str!("../../assets/report.html");
const UPLOT_JS: &str = include_str!("../../assets/vendor/uPlot.iife.min.js");
const UPLOT_CSS: &str = include_str!("../../assets/vendor/uPlot.min.css");

/// Escape a JSON payload for embedding inside a `<script>` element.
///
/// This is the security boundary of the whole report. The document carries
/// raw log lines, and log content is written by whatever produced the logs —
/// a message containing `</script>` would otherwise close the element and
/// everything after it would be parsed as HTML, which is script injection
/// into a file people open in a browser and pass around.
///
/// `<`, `>` and `&` never appear as JSON structural characters, only inside
/// strings, so replacing them with their `\uXXXX` escapes is both safe and
/// lossless — the parsed value is identical. U+2028 and U+2029 are valid in
/// JSON strings but terminate a line in JavaScript, so they go too.
pub fn escape_for_script(json: &str) -> String {
    let mut out = String::with_capacity(json.len() + 16);
    for c in json.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape text for an HTML context.
///
/// Named for the text-node case it is used in (`<title>`), but quotes are
/// escaped too. The narrower version was correct for its one call site and
/// unsafe the moment anyone reused it in an attribute — exactly the kind of
/// latent trap a helper's name should not set.
fn escape_html_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Fill every placeholder in one pass.
///
/// Chained `String::replace` calls re-scan text that earlier calls inserted,
/// so a value containing a later placeholder gets it substituted: a report
/// titled `__DATA__` had the entire JSON payload injected into its `<title>`.
/// That was harmless only because the payload provably contains no `<` — it
/// was load-bearing by accident, and would have become a real hole the moment
/// the escaping was narrowed. Walking the template once removes the class of
/// bug rather than the instance.
fn substitute(template: &str, subs: &[(&str, &str)]) -> String {
    let extra: usize = subs.iter().map(|(_, v)| v.len()).sum();
    let mut out = String::with_capacity(template.len() + extra);
    let mut rest = template;
    while !rest.is_empty() {
        match subs.iter().find(|(pat, _)| rest.starts_with(pat)) {
            Some((pat, value)) => {
                out.push_str(value);
                rest = &rest[pat.len()..];
            }
            None => {
                let c = rest.chars().next().unwrap_or_default();
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

/// Render a findings document into a complete HTML page.
pub fn render(findings: &Findings) -> Result<String, serde_json::Error> {
    let payload = escape_for_script(&serde_json::to_string(findings)?);
    Ok(substitute(
        SHELL,
        &[
            ("__UPLOT_CSS__", UPLOT_CSS),
            ("__UPLOT_JS__", UPLOT_JS),
            ("__TITLE__", &escape_html_text(&findings.title)),
            ("__DATA__", &payload),
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::model::*;

    fn findings_with(message: &str) -> Findings {
        Findings {
            title: "Incident".into(),
            subtitle: None,
            summary: None,
            window: None,
            sections: vec![],
            charts: vec![],
            events: vec![],
            clusters: vec![Cluster {
                cluster_id: "abc123def456".into(),
                template: message.into(),
                count: 1,
                status: None,
                first_seen: None,
                last_seen: None,
                histogram: vec![],
                by_stream: vec![],
                other_streams: 0,
                exemplar: Some(Exemplar {
                    ts: "2026-09-04T10:00:00Z".into(),
                    stream: "s".into(),
                    message: message.into(),
                }),
            }],
            appendix: vec![],
        }
    }

    #[test]
    fn a_log_line_cannot_close_the_script_element() {
        // The attack: anything that can write to a watched log group emits
        // this, and the report becomes a script-injection vector in a file
        // people open in a browser and forward to colleagues.
        let hostile = "</script><img src=x onerror=alert(1)>";
        let html = render(&findings_with(hostile)).unwrap();
        assert!(
            !html.contains("</script><img"),
            "the element was closed early"
        );
        assert!(
            html.contains("\\u003c/script"),
            "the sequence is escaped instead"
        );
        // Exactly the script tags the shell itself opens, and no more.
        assert_eq!(
            html.matches("</script>").count(),
            3,
            "no extra closing tags"
        );
    }

    #[test]
    fn escaping_is_lossless() {
        // Escaping must not corrupt the data: the parsed value has to be
        // byte-identical or the report shows something other than the log.
        let tricky = "a<b>c&d \u{2028} \u{2029} </SCRIPT> <!-- -->";
        let escaped = escape_for_script(&serde_json::to_string(tricky).unwrap());
        let back: String = serde_json::from_str(&escaped).unwrap();
        assert_eq!(back, tricky);
    }

    #[test]
    fn every_angle_bracket_and_ampersand_is_escaped() {
        let out = escape_for_script("{\"k\":\"<&>\"}");
        assert!(
            !out.contains('<') && !out.contains('>') && !out.contains('&'),
            "{out}"
        );
    }

    #[test]
    fn the_title_is_escaped_where_it_lands_in_markup() {
        let mut f = findings_with("x");
        f.title = "<script>alert(1)</script>".into();
        let html = render(&f).unwrap();
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert_eq!(html.matches("</script>").count(), 3);
    }

    #[test]
    fn a_placeholder_in_the_title_is_not_substituted() {
        // Chained replaces re-scanned inserted text, so a report titled
        // `__DATA__` had the whole JSON payload injected into its <title>.
        let mut f = findings_with("x");
        f.title = "PRE__DATA__POST".into();
        let html = render(&f).unwrap();
        let open = html.find("<title>").unwrap() + "<title>".len();
        let close = html[open..].find("</title>").unwrap() + open;
        assert_eq!(
            &html[open..close],
            "PRE__DATA__POST",
            "the title is not a substitution site"
        );
    }

    #[test]
    fn every_placeholder_is_consumed_exactly_once() {
        let html = render(&findings_with("x")).unwrap();
        for p in ["__DATA__", "__TITLE__", "__UPLOT_JS__", "__UPLOT_CSS__"] {
            assert!(!html.contains(p), "{p} survived into the output");
        }
    }

    #[test]
    fn html_text_escaping_covers_quotes_too() {
        // Correct for <title> either way, but the name must not set a trap
        // for anyone who reuses it in an attribute context.
        assert_eq!(
            escape_html_text("a\"b'c<d>e&f"),
            "a&quot;b&#39;c&lt;d&gt;e&amp;f"
        );
    }

    #[test]
    fn a_placeholder_inside_log_content_is_not_substituted() {
        // Data is substituted last, so a log line containing a placeholder
        // token cannot trigger a second replacement.
        let html = render(&findings_with("__UPLOT_JS__ __DATA__ __TITLE__")).unwrap();
        assert!(
            html.contains("__UPLOT_JS__"),
            "the literal survives as data"
        );
        assert!(
            !html.contains("__DATA__\n"),
            "the real placeholder was consumed"
        );
    }

    #[test]
    fn the_page_is_self_contained() {
        let html = render(&findings_with("x")).unwrap();
        // uPlot is inlined, not linked.
        assert!(html.contains("var uPlot="), "the library is embedded");
        assert!(!html.contains("<script src="), "no external script");
        assert!(
            !html.contains("<link rel=\"stylesheet\""),
            "no external stylesheet"
        );
        // The only remote URL is uPlot's own attribution comment.
        let remotes: Vec<&str> = html
            .split_whitespace()
            .filter(|w| w.starts_with("http://") || w.starts_with("https://"))
            .filter(|w| !w.contains("github.com/leeoniya/uPlot"))
            .filter(|w| !w.contains("www.w3.org"))
            .collect();
        assert!(
            remotes.is_empty(),
            "unexpected remote references: {remotes:?}"
        );
    }

    #[test]
    fn the_payload_round_trips_through_the_page() {
        let f = findings_with("Connection to <*> failed after <*>ms");
        let html = render(&f).unwrap();
        let start =
            html.find(r#"type="application/json">"#).unwrap() + r#"type="application/json">"#.len();
        let end = html[start..].find("</script>").unwrap() + start;
        let parsed: Findings = serde_json::from_str(&html[start..end]).unwrap();
        assert_eq!(
            parsed.clusters[0].template,
            "Connection to <*> failed after <*>ms"
        );
    }

    #[test]
    fn the_heading_carries_the_title_without_running_scripts() {
        // The <h1> used to be empty in the source and filled by script on
        // load, so the document had no heading text for a screen reader --
        // or for anyone whose scripts did not run. The title is already
        // substituted server-side for <title>; putting it in the heading too
        // costs nothing and makes the page degrade instead of going blank.
        let mut f = findings_with("x");
        f.title = "Checkout latency".into();
        let html = render(&f).unwrap();
        assert!(
            html.contains("<h1 id=\"title\">Checkout latency</h1>"),
            "the heading is populated in the markup"
        );
        // Still escaped -- it lands in markup twice now, so both sites matter.
        f.title = "<img src=x onerror=alert(1)>".into();
        let html = render(&f).unwrap();
        assert!(!html.contains("<img src=x"), "the heading is escaped too");
        assert_eq!(
            html.matches("&lt;img src=x").count(),
            2,
            "title and heading"
        );
    }

    #[test]
    fn the_report_carries_uplot_attribution() {
        // uPlot is MIT and is compiled into every report, so its copyright
        // and permission notice have to travel with it. The banners on the
        // vendored files are the only thing carrying that, and nothing else
        // in the build would fail if a future refactor stripped them -- the
        // charts would still render and every other test would still pass.
        // That is precisely why this assertion exists.
        let html = render(&findings_with("x")).unwrap();
        assert_eq!(
            html.matches("Copyright (c) 2022 Leon Sorokin").count(),
            2,
            "one banner from the inlined JS, one from the inlined CSS"
        );
        assert!(html.contains("MIT License"), "the licence is named");
    }
}
