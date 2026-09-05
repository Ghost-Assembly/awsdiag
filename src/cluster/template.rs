//! Grouping log events into clusters that keep their shape.
//!
//! Clustering is lossy in what it *shows* and lossless in what it *indexes*:
//! every cluster is a query that can be re-run to get the raw lines back.
//! Three fields carry the diagnostic weight:
//!
//! - `histogram` — the trend across the window: onset, plateau, recovery.
//! - `by_stream` — the *where*. "Everywhere, or one host?" is frequently the
//!   whole diagnosis.
//! - `status` — `new` / `spiking` / `steady` against a baseline period. What
//!   is new since yesterday is usually the answer.

use crate::cluster::tokenize;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;

/// One event as returned by CloudWatch Logs.
#[derive(Debug, Clone)]
pub struct LogEvent {
    pub timestamp: DateTime<Utc>,
    pub stream: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Exemplar {
    pub ts: DateTime<Utc>,
    pub stream: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StreamCount {
    pub stream: String,
    pub count: usize,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Absent from the baseline period entirely.
    New,
    /// Occurring materially faster than in the baseline period.
    Spiking,
    /// Present in the baseline at a comparable rate.
    Steady,
    /// No baseline was requested, so nothing can be said.
    Unknown,
}

impl Status {
    /// Ordering for display: what changed comes before what did not.
    fn rank(self) -> u8 {
        match self {
            Status::New => 0,
            Status::Spiking => 1,
            Status::Steady => 2,
            Status::Unknown => 3,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Cluster {
    // Field order is the rendered column order (serde_json preserves it), so
    // the narrow, scannable fields lead and the long template follows. In
    // text mode a leading template column pushes everything else off-screen.
    pub cluster_id: String,
    pub status: Status,
    pub count: usize,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub template: String,
    pub histogram: Vec<u32>,
    pub by_stream: Vec<StreamCount>,
    /// Number of streams beyond those listed in `by_stream`.
    pub other_streams: usize,
    pub exemplar: Exemplar,
}

/// How many streams to name per cluster before summarising the rest.
const MAX_STREAMS: usize = 10;

/// A cluster must exceed the baseline rate by this factor to count as
/// spiking. Chosen high enough that ordinary traffic variation does not
/// register as a change worth investigating.
const SPIKE_FACTOR: f64 = 3.0;

/// A stable short identifier derived from the template text, so the same
/// template yields the same id across runs and `logs drill --cluster` stays
/// meaningful between invocations.
pub fn cluster_id(template: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in template.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    // 48 bits, not 24. At 24 bits, 3,000 clusters carry roughly a 24% chance
    // of a collision -- and a 3,000-cluster scan has been measured against
    // real data. A collision makes `logs drill --cluster <id>` return another
    // cluster's lines, which is silently wrong rather than visibly broken.
    format!("{:012x}", h & 0xffff_ffff_ffff)
}

/// Group events into clusters over the window `[since, until]`.
///
/// `buckets` is the histogram resolution. Events outside the window are still
/// counted — the caller asked for them — but clamped into the end buckets
/// rather than being dropped or panicking on an out-of-range index.
pub fn cluster(
    events: &[LogEvent],
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    buckets: usize,
) -> Vec<Cluster> {
    let buckets = buckets.max(1);
    let mut groups: HashMap<String, Vec<&LogEvent>> = HashMap::new();
    for e in events {
        groups
            .entry(tokenize::template(&e.message))
            .or_default()
            .push(e);
    }

    let mut out: Vec<Cluster> = groups
        .into_iter()
        .map(|(template, members)| build(template, &members, since, until, buckets))
        .collect();
    sort_clusters(&mut out);
    out
}

/// What changed first, then by volume. A deterministic tiebreak on id keeps
/// output stable between runs over identical input.
fn sort_clusters(out: &mut [Cluster]) {
    out.sort_by(|a, b| {
        a.status
            .rank()
            .cmp(&b.status.rank())
            .then(b.count.cmp(&a.count))
            .then(a.cluster_id.cmp(&b.cluster_id))
    });
}

fn build(
    template: String,
    members: &[&LogEvent],
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    buckets: usize,
) -> Cluster {
    let mut histogram = vec![0u32; buckets];
    let span = (until - since).num_milliseconds().max(1) as f64;

    let mut per_stream: HashMap<&str, usize> = HashMap::new();
    let mut first = members[0].timestamp;
    let mut last = members[0].timestamp;

    for e in members {
        let offset = (e.timestamp - since).num_milliseconds() as f64;
        // Clamp rather than index blindly: an event exactly at `until` would
        // otherwise land one past the end of the vector.
        let idx = ((offset / span) * buckets as f64)
            .floor()
            .clamp(0.0, (buckets - 1) as f64) as usize;
        histogram[idx] += 1;
        *per_stream.entry(e.stream.as_str()).or_default() += 1;
        first = first.min(e.timestamp);
        last = last.max(e.timestamp);
    }

    let total_streams = per_stream.len();
    let mut by_stream: Vec<StreamCount> = per_stream
        .into_iter()
        .map(|(stream, count)| StreamCount {
            stream: stream.to_string(),
            count,
        })
        .collect();
    by_stream.sort_by(|a, b| b.count.cmp(&a.count).then(a.stream.cmp(&b.stream)));
    by_stream.truncate(MAX_STREAMS);

    // The earliest occurrence is the most useful single line to show: during
    // an incident the first instance is the one nearest the cause.
    let earliest = members
        .iter()
        .min_by_key(|e| e.timestamp)
        .unwrap_or(&members[0]);

    Cluster {
        cluster_id: cluster_id(&template),
        status: Status::Unknown,
        count: members.len(),
        first_seen: first,
        last_seen: last,
        template,
        histogram,
        other_streams: total_streams.saturating_sub(by_stream.len()),
        by_stream,
        exemplar: Exemplar {
            ts: earliest.timestamp,
            stream: earliest.stream.clone(),
            message: earliest.message.clone(),
        },
    }
}

/// Classify each cluster against counts from a baseline period.
///
/// Rates are normalised by duration, so a 1-hour window compares correctly
/// against a 24-hour baseline. Without that normalisation every cluster in a
/// short window would look like it had collapsed.
pub fn apply_baseline(
    clusters: &mut [Cluster],
    baseline: &HashMap<String, usize>,
    window_secs: i64,
    baseline_secs: i64,
) {
    let window_secs = window_secs.max(1) as f64;
    let baseline_secs = baseline_secs.max(1) as f64;

    for c in clusters.iter_mut() {
        c.status = match baseline.get(&c.template) {
            None => Status::New,
            Some(&prior) => {
                let now_rate = c.count as f64 / window_secs;
                let then_rate = prior as f64 / baseline_secs;
                if then_rate > 0.0 && now_rate > then_rate * SPIKE_FACTOR {
                    Status::Spiking
                } else {
                    Status::Steady
                }
            }
        };
    }
    sort_clusters(clusters);
}

/// Reduce baseline events to template counts, which is all the comparison
/// needs — holding the full events would be wasteful for a 24-hour period.
pub fn baseline_counts(events: &[LogEvent]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for e in events {
        *counts.entry(tokenize::template(&e.message)).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 4, h, m, 0).unwrap()
    }
    fn since() -> DateTime<Utc> {
        t(10, 0)
    }
    fn until() -> DateTime<Utc> {
        t(11, 0)
    }
    fn ev(h: u32, m: u32, stream: &str, msg: &str) -> LogEvent {
        LogEvent {
            timestamp: t(h, m),
            stream: stream.into(),
            message: msg.into(),
        }
    }

    /// Two hosts, one of which owns nearly all the errors — the shape of a
    /// real single-instance failure.
    fn skewed() -> Vec<LogEvent> {
        let mut v = Vec::new();
        for i in 0..9 {
            v.push(ev(
                10,
                i * 5,
                "i-0aaa",
                "Connection to 10.0.1.5:5432 failed after 12ms",
            ));
        }
        v.push(ev(
            10,
            50,
            "i-0bbb",
            "Connection to 10.0.9.9:5432 failed after 900ms",
        ));
        v.push(ev(10, 30, "i-0aaa", "Heartbeat ok"));
        v
    }

    #[test]
    fn events_sharing_a_template_collapse_into_one_cluster() {
        let c = cluster(&skewed(), since(), until(), 12);
        assert_eq!(c.len(), 2, "one connection cluster, one heartbeat cluster");
        assert_eq!(c[0].count, 10);
        assert_eq!(c[0].template, "Connection to <*> failed after <*>");
    }

    #[test]
    fn by_stream_shows_where_and_is_ordered_by_volume() {
        // This is the field that answers "fleet, or one box?".
        let c = cluster(&skewed(), since(), until(), 12);
        assert_eq!(
            c[0].by_stream,
            vec![
                StreamCount {
                    stream: "i-0aaa".into(),
                    count: 9
                },
                StreamCount {
                    stream: "i-0bbb".into(),
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn many_streams_are_capped_and_the_remainder_counted() {
        // Naming 400 streams would defeat the purpose of clustering.
        let events: Vec<LogEvent> = (0..25)
            .map(|i| ev(10, 1, &format!("i-{i:04}"), "disk full"))
            .collect();
        let c = cluster(&events, since(), until(), 12);
        assert_eq!(c[0].by_stream.len(), MAX_STREAMS);
        assert_eq!(c[0].other_streams, 15);
        assert_eq!(c[0].count, 25, "the count still reflects every event");
    }

    #[test]
    fn the_histogram_places_events_in_time_order() {
        let events = vec![
            ev(10, 0, "s", "x"),
            ev(10, 30, "s", "x"),
            ev(10, 30, "s", "x"),
            ev(10, 59, "s", "x"),
        ];
        let c = cluster(&events, since(), until(), 4);
        assert_eq!(c[0].histogram, vec![1, 0, 2, 1]);
        assert_eq!(
            c[0].histogram.iter().sum::<u32>(),
            4,
            "every event is counted once"
        );
    }

    #[test]
    fn an_event_exactly_at_the_window_end_does_not_overflow_the_histogram() {
        // Classic off-by-one: offset/span == 1.0 indexes one past the end.
        let events = vec![LogEvent {
            timestamp: until(),
            stream: "s".into(),
            message: "edge".into(),
        }];
        let c = cluster(&events, since(), until(), 4);
        assert_eq!(c[0].histogram, vec![0, 0, 0, 1]);
    }

    #[test]
    fn a_zero_length_window_does_not_divide_by_zero() {
        let events = vec![ev(10, 0, "s", "x")];
        let c = cluster(&events, since(), since(), 4);
        assert_eq!(c[0].count, 1);
        assert_eq!(c[0].histogram.iter().sum::<u32>(), 1);
    }

    #[test]
    fn first_and_last_seen_span_the_cluster() {
        let c = cluster(&skewed(), since(), until(), 12);
        assert_eq!(c[0].first_seen, t(10, 0));
        assert_eq!(c[0].last_seen, t(10, 50));
    }

    #[test]
    fn the_exemplar_is_the_earliest_real_line_from_that_cluster() {
        // The first occurrence sits nearest the cause, and it must be a
        // verbatim line rather than the masked template.
        let c = cluster(&skewed(), since(), until(), 12);
        assert_eq!(c[0].exemplar.ts, t(10, 0));
        assert_eq!(c[0].exemplar.stream, "i-0aaa");
        assert_eq!(
            c[0].exemplar.message,
            "Connection to 10.0.1.5:5432 failed after 12ms"
        );
        assert!(!c[0].exemplar.message.contains("<*>"));
    }

    #[test]
    fn no_events_yields_no_clusters() {
        assert!(cluster(&[], since(), until(), 12).is_empty());
    }

    #[test]
    fn cluster_ids_are_stable_across_runs_and_differ_per_template() {
        // `logs drill --cluster <id>` is worthless if the id moves.
        let a = cluster(&skewed(), since(), until(), 12);
        let b = cluster(&skewed(), since(), until(), 12);
        assert_eq!(a[0].cluster_id, b[0].cluster_id);
        assert_ne!(a[0].cluster_id, a[1].cluster_id);
        assert_eq!(
            a[0].cluster_id.len(),
            12,
            "48 bits: see cluster_id for why 24 was not enough"
        );
    }

    // ---- baseline classification -----------------------------------------

    fn classify(now: usize, prior: Option<usize>) -> Status {
        let events: Vec<LogEvent> = (0..now).map(|_| ev(10, 1, "s", "db timeout")).collect();
        let mut c = cluster(&events, since(), until(), 12);
        let mut base = HashMap::new();
        if let Some(p) = prior {
            base.insert("db timeout".to_string(), p);
        }
        // one-hour window against a 24-hour baseline
        apply_baseline(&mut c, &base, 3600, 86_400);
        c[0].status
    }

    #[test]
    fn a_template_absent_from_the_baseline_is_new() {
        assert_eq!(classify(10, None), Status::New);
    }

    #[test]
    fn rates_are_normalised_by_duration_not_compared_raw() {
        // 24 events over 24h is 1/hour. 10 in one hour is 10x that: spiking.
        // Comparing raw counts (10 vs 24) would call this a *decrease*.
        assert_eq!(classify(10, Some(24)), Status::Spiking);
        // 1 in an hour against 24 over 24h is the same rate: steady.
        assert_eq!(classify(1, Some(24)), Status::Steady);
    }

    #[test]
    fn ordinary_variation_does_not_register_as_a_spike() {
        // 2/hour against 1/hour is noise, not a signal worth surfacing.
        assert_eq!(classify(2, Some(24)), Status::Steady);
    }

    #[test]
    fn what_changed_is_ordered_before_what_did_not() {
        let mut events: Vec<LogEvent> =
            (0..100).map(|_| ev(10, 1, "s", "steady chatter")).collect();
        events.push(ev(10, 2, "s", "brand new failure"));
        let mut c = cluster(&events, since(), until(), 12);
        let base = HashMap::from([("steady chatter".to_string(), 2400usize)]);
        apply_baseline(&mut c, &base, 3600, 86_400);

        // The single new line outranks the 100 steady ones: during an
        // incident, what changed is what matters, not what is loudest.
        assert_eq!(c[0].status, Status::New);
        assert_eq!(c[0].count, 1);
        assert_eq!(c[1].status, Status::Steady);
    }

    #[test]
    fn without_a_baseline_status_is_unknown_rather_than_guessed() {
        let c = cluster(&skewed(), since(), until(), 12);
        assert!(c.iter().all(|x| x.status == Status::Unknown));
    }

    #[test]
    fn baseline_counts_reduce_events_to_template_totals() {
        let events = vec![
            ev(9, 0, "s", "Connection to 10.0.1.5:5432 failed after 12ms"),
            ev(9, 1, "s", "Connection to 10.0.2.6:5432 failed after 44ms"),
            ev(9, 2, "s", "Heartbeat ok"),
        ];
        let counts = baseline_counts(&events);
        assert_eq!(counts.get("Connection to <*> failed after <*>"), Some(&2));
        assert_eq!(counts.get("Heartbeat ok"), Some(&1));
    }
}
