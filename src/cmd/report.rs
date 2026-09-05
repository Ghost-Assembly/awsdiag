//! `awsdiag report` — render a findings document into a single HTML file.

use crate::common::errors::Error;
use crate::common::progress::Progress;
use crate::report::{model::Findings, render};
use std::path::Path;

/// A worked example, printed by `--schema`.
///
/// A schema without a filled-in example leaves the caller guessing at shapes;
/// this one is a valid document that renders, so it doubles as a smoke test.
pub const EXAMPLE: &str = r##"{
  "title": "Checkout latency incident",
  "subtitle": "prod · investigated 2026-09-04",
  "window": { "since": "2026-09-04T09:30:00Z", "until": "2026-09-04T11:00:00Z" },
  "summary": "Checkout p99 rose from 120ms to 4.2s for 38 minutes.\nOne database host ran out of connections; the other two were unaffected.",
  "sections": [
    { "heading": "What happened", "body": "At 10:04 connection failures began...", "severity": "high" },
    { "heading": "Recommendation", "body": "Raise the pool ceiling and alert on saturation." }
  ],
  "charts": [
    {
      "title": "CPU utilisation",
      "unit": "%",
      "series": ["web-a", "web-b"],
      "rows": [
        { "ts": "2026-09-04T10:00:00Z", "web-a": 12.4, "web-b": 11.8 },
        { "ts": "2026-09-04T10:01:00Z", "web-a": 91.2, "web-b": null }
      ]
    }
  ],
  "events": [ { "ts": "2026-09-04T10:03:00Z", "label": "deploy 4f21a" } ],
  "clusters": [
    {
      "cluster_id": "c7f3a19b2e04",
      "status": "new",
      "count": 8412,
      "first_seen": "2026-09-04T10:04:12Z",
      "last_seen": "2026-09-04T10:41:55Z",
      "template": "Connection to <*> failed after <*>ms (attempt <*>)",
      "histogram": [0, 0, 3, 847, 2210, 1904, 12, 0],
      "by_stream": [ { "stream": "db-a", "count": 8201 }, { "stream": "db-b", "count": 211 } ],
      "other_streams": 0,
      "exemplar": {
        "ts": "2026-09-04T10:04:12Z",
        "stream": "db-a",
        "message": "Connection to 10.0.1.5:5432 failed after 1234ms (attempt 3)"
      }
    }
  ],
  "appendix": [ { "title": "Raw scan parameters", "content": "logs scan --group /aws/lambda/checkout --since 2h" } ]
}"##;

/// Parse a findings document, reporting *where* it is wrong.
///
/// serde's message names the offending field and `serde_json` supplies the
/// line and column, so a caller can fix the document rather than bisect it.
pub fn read_findings(path: &std::path::Path) -> Result<Findings, Error> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::File {
        action: "read",
        path: path.display().to_string(),
        source,
    })?;
    parse(&text, &path.display().to_string())
}

pub fn parse(json: &str, source: &str) -> Result<Findings, Error> {
    serde_json::from_str(json).map_err(|e| Error::BadFindings {
        source_name: source.to_string(),
        detail: format!("{e}"),
    })
}

pub fn write_report(
    findings: &Findings,
    out: &Path,
    progress: &Progress,
) -> Result<ReportSummary, Error> {
    progress.phase("rendering report");
    let html = render::render(findings).map_err(|e| Error::BadFindings {
        source_name: "findings".into(),
        detail: e.to_string(),
    })?;
    // Name the file. `read` and `write` failures were previously
    // indistinguishable -- both surfaced a bare "No such file or directory"
    // with no path, leaving the caller unable to tell which one was wrong.
    std::fs::write(out, &html).map_err(|source| Error::File {
        action: "write",
        path: out.display().to_string(),
        source,
    })?;
    let summary = ReportSummary {
        path: out.display().to_string(),
        bytes: html.len(),
        charts: findings.charts.len(),
        series: findings.charts.iter().map(|c| c.series.len()).sum(),
        clusters: findings.clusters.len(),
        events: findings.events.len(),
    };
    progress.finish(format!(
        "{} · {} KB · {} charts, {} clusters",
        summary.path,
        summary.bytes / 1024,
        summary.charts,
        summary.clusters
    ));
    Ok(summary)
}

#[derive(Debug, serde::Serialize)]
pub struct ReportSummary {
    pub path: String,
    pub bytes: usize,
    pub charts: usize,
    pub series: usize,
    pub clusters: usize,
    pub events: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_example_is_a_valid_document() {
        // A schema example that does not parse is worse than none: it teaches
        // the wrong shape and is copied verbatim.
        let f = parse(EXAMPLE, "example").expect("the example must parse");
        assert_eq!(f.charts.len(), 1);
        assert_eq!(f.clusters[0].count, 8412);
        assert_eq!(f.events.len(), 1);
    }

    #[test]
    fn the_documented_example_renders() {
        let f = parse(EXAMPLE, "example").unwrap();
        let html = render::render(&f).unwrap();
        assert!(html.contains("Checkout latency incident"));
        assert!(html.len() > 50_000, "uPlot is inlined");
    }

    #[test]
    fn a_broken_document_names_the_file_and_the_problem() {
        let e = parse(r#"{"title": 5}"#, "findings.json").unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("findings.json"), "names the source: {msg}");
        assert!(
            msg.contains("line") || msg.contains("column"),
            "locates it: {msg}"
        );
        assert_eq!(e.kind(), "bad_findings");
    }

    #[test]
    fn a_misspelt_key_is_reported_rather_than_ignored() {
        let e = parse(r#"{"title":"x","clusterz":[]}"#, "f.json").unwrap_err();
        assert!(e.to_string().contains("clusterz"), "{e}");
    }
}
