//! The findings document: the contract between the analysis and the report.
//!
//! Shapes deliberately mirror what the acquisition commands already emit, so
//! a caller pastes `metrics compare` and `logs scan` output in rather than
//! transforming it. Every field except the title is optional, because a
//! report about a log-only incident should not have to invent charts.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Findings {
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    /// Free text, shown first. This is the narrative — what happened, what
    /// broke, what to do.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub window: Option<Window>,
    #[serde(default)]
    pub sections: Vec<Section>,
    #[serde(default)]
    pub charts: Vec<Chart>,
    /// Vertical markers drawn across every chart — a deploy, a CloudTrail
    /// event. Lining a spike up against its cause is the point of the report.
    #[serde(default)]
    pub events: Vec<EventMarker>,
    #[serde(default)]
    pub clusters: Vec<Cluster>,
    #[serde(default)]
    pub appendix: Vec<Appendix>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub since: String,
    pub until: String,
}

/// A narrative section. `severity` only tints the heading; it carries no
/// behaviour, so an unfamiliar value degrades to plain rather than failing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub heading: String,
    pub body: String,
    #[serde(default)]
    pub severity: Option<String>,
}

/// One chart. `series` and `rows` are exactly `metrics compare`'s
/// `params.labels` and `data`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chart {
    pub title: String,
    #[serde(default)]
    pub unit: Option<String>,
    pub series: Vec<String>,
    /// Objects with a `ts` key plus one key per series label. A missing or
    /// null value is a gap and is drawn as one, never as zero.
    pub rows: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventMarker {
    pub ts: String,
    pub label: String,
}

/// A log cluster, as `logs scan` emits it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cluster {
    pub cluster_id: String,
    pub template: String,
    pub count: usize,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub first_seen: Option<String>,
    #[serde(default)]
    pub last_seen: Option<String>,
    #[serde(default)]
    pub histogram: Vec<u32>,
    #[serde(default)]
    pub by_stream: Vec<StreamCount>,
    #[serde(default)]
    pub other_streams: usize,
    #[serde(default)]
    pub exemplar: Option<Exemplar>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamCount {
    pub stream: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exemplar {
    pub ts: String,
    pub stream: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Appendix {
    pub title: String,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_document_needs_only_a_title() {
        // A log-only incident should not have to invent charts.
        let f: Findings = serde_json::from_str(r#"{"title":"Incident"}"#).unwrap();
        assert_eq!(f.title, "Incident");
        assert!(f.charts.is_empty() && f.clusters.is_empty() && f.sections.is_empty());
    }

    #[test]
    fn metrics_compare_output_drops_straight_into_a_chart() {
        // The shape here is exactly what `metrics compare` emits, so the
        // caller pastes rather than transforms.
        let c: Chart = serde_json::from_str(
            r#"{"title":"CPU","unit":"%","series":["a","b"],
                "rows":[{"ts":"2026-09-04T10:00:00Z","a":1.5,"b":null}]}"#,
        )
        .unwrap();
        assert_eq!(c.series, vec!["a", "b"]);
        assert_eq!(c.rows.len(), 1);
        assert!(c.rows[0]["b"].is_null(), "a gap stays a gap");
    }

    #[test]
    fn logs_scan_output_drops_straight_into_a_cluster() {
        let c: Cluster = serde_json::from_str(
            r#"{"cluster_id":"abc123def456","status":"new","count":8412,
                "first_seen":"2026-09-04T10:04:12Z","last_seen":"2026-09-04T10:41:55Z",
                "template":"Connection to <*> failed","histogram":[0,3,847],
                "by_stream":[{"stream":"s1","count":8201}],"other_streams":0,
                "exemplar":{"ts":"2026-09-04T10:04:12Z","stream":"s1","message":"raw line"}}"#,
        )
        .unwrap();
        assert_eq!(c.count, 8412);
        assert_eq!(c.by_stream[0].count, 8201);
        assert_eq!(c.exemplar.unwrap().message, "raw line");
    }

    #[test]
    fn a_misspelt_field_is_rejected_rather_than_silently_dropped() {
        // Without deny_unknown_fields a typo produces a report missing a
        // whole panel, with no indication anything went wrong.
        let e = serde_json::from_str::<Findings>(r#"{"title":"x","clusterz":[]}"#).unwrap_err();
        assert!(e.to_string().contains("clusterz"), "{e}");
    }

    #[test]
    fn a_wrongly_typed_field_reports_where_it_is() {
        let e = serde_json::from_str::<Findings>(r#"{"title":"x","charts":"nope"}"#).unwrap_err();
        assert!(
            e.line() > 0 || e.column() > 0,
            "the error locates the problem"
        );
    }

    #[test]
    fn the_document_round_trips() {
        let original = Findings {
            title: "t".into(),
            subtitle: None,
            summary: Some("s".into()),
            window: Some(Window {
                since: "a".into(),
                until: "b".into(),
            }),
            sections: vec![Section {
                heading: "h".into(),
                body: "b".into(),
                severity: None,
            }],
            charts: vec![],
            events: vec![EventMarker {
                ts: "t".into(),
                label: "deploy".into(),
            }],
            clusters: vec![],
            appendix: vec![Appendix {
                title: "a".into(),
                content: "c".into(),
            }],
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: Findings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.title, original.title);
        assert_eq!(back.events.len(), 1);
    }
}
