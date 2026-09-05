//! Properties clustering must satisfy however it is implemented.
//!
//! These were written from findings in a review of a variant-family merge
//! pass that has since been reverted. Every one of them failed against that
//! implementation. They are expressed against the public clustering API
//! rather than its internals, so they stay meaningful when the feature is
//! rebuilt — a merge pass that reintroduces any of these fails here.

use awsdiag::cluster::template::{Cluster, LogEvent, cluster, cluster_id};
use awsdiag::cluster::tokenize;
use chrono::{DateTime, Duration, TimeZone, Utc};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 4, 10, 0, 0).unwrap()
}

fn clusters_of(messages: &[&str]) -> Vec<Cluster> {
    let events: Vec<LogEvent> = messages
        .iter()
        .map(|m| LogEvent {
            timestamp: now(),
            stream: "s".into(),
            message: (*m).into(),
        })
        .collect();
    cluster(&events, now(), now() + Duration::hours(1), 12)
}

#[test]
fn distinct_json_events_never_collapse_into_one_row() {
    // A JSON line contains no whitespace, so it is a single token. A merge
    // pass keyed on "blank one token" therefore blanked the *entire*
    // template, and every distinct JSON event fused into one row reading
    // `<*>`. JSON service logs are a primary target, so this destroyed the
    // output for a large share of real input.
    let c = clusters_of(&[
        r#"{"level":"WARN","state":"draining"}"#,
        r#"{"level":"INFO","event":"started"}"#,
        r#"{"level":"ERROR","code":500}"#,
    ]);
    assert_eq!(c.len(), 3, "three distinct events, three rows: {c:#?}");
    assert!(
        !c.iter().any(|x| x.template.trim() == "<*>"),
        "a template of nothing but a mask carries no information"
    );
}

#[test]
fn values_under_identity_keys_are_never_merged_away() {
    // The tokenizer preserves these deliberately: Windows Event ID 4625 is a
    // failed logon and 5145 is a share access check. Any later pass that
    // merges them destroys the distinction the log exists to record.
    let c = clusters_of(&[
        "Security <Event><EventID>4625</EventID></Event> audit",
        "Security <Event><EventID>5145</EventID></Event> audit",
    ]);
    assert_eq!(c.len(), 2, "different event ids are different events");

    let levels = clusters_of(&[
        "level=ERROR disk write failed",
        "level=INFO disk write failed",
    ]);
    assert_eq!(levels.len(), 2, "ERROR and INFO are different outcomes");
}

#[test]
fn cluster_ids_are_unique_within_one_result() {
    // Two rows sharing an id make `logs drill --cluster <id>` ambiguous.
    let c = clusters_of(&[
        "node alpha ready",
        "node bravo ready",
        "node 10.0.0.1 ready",
        "node 10.0.0.2 ready",
    ]);
    let mut ids: Vec<&str> = c.iter().map(|x| x.cluster_id.as_str()).collect();
    let before = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(before, ids.len(), "duplicate cluster ids in {c:#?}");
}

#[test]
fn every_cluster_id_is_reachable_from_its_own_events() {
    // This is the scan -> drill contract. `logs drill` recomputes the
    // per-event template and matches its hash, so a row whose id no event can
    // produce is undrillable and the documented workflow silently returns
    // nothing.
    let messages = [
        "node alpha ready",
        "node alpha ready",
        "node bravo ready",
        r#"{"level":"ERROR","code":500}"#,
        "Connection to 10.0.1.5:5432 failed after 12ms",
    ];
    for c in clusters_of(&messages) {
        let reachable = messages
            .iter()
            .any(|m| cluster_id(&tokenize::template(m)) == c.cluster_id);
        assert!(
            reachable,
            "cluster {} ({}) is not reachable by drill",
            c.cluster_id, c.template
        );
    }
}

#[test]
fn cluster_ids_are_wide_enough_that_collisions_are_not_expected() {
    // At 24 bits, 3,000 clusters carry a ~24% chance of a collision -- and a
    // 3,000-cluster scan has been observed against real data. A collision
    // makes drill return another cluster's lines.
    let id = cluster_id("any template at all");
    assert!(
        id.len() >= 12,
        "id {id:?} is {} hex chars; 12 keeps collisions negligible at realistic cluster counts",
        id.len()
    );
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn every_event_is_counted_exactly_once() {
    let messages: Vec<String> = (0..50)
        .map(|i| format!("worker {} finished batch", i % 7))
        .collect();
    let refs: Vec<&str> = messages.iter().map(String::as_str).collect();
    let c = clusters_of(&refs);
    assert_eq!(c.iter().map(|x| x.count).sum::<usize>(), 50);
}
