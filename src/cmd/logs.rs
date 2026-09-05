//! `awsdiag logs` — CloudWatch Logs discovery, clustered scanning, drill-down.

use crate::cluster::template::{self, Cluster, LogEvent};
use crate::cluster::tokenize;
use crate::common::aws;
use crate::common::envelope::Envelope;
use crate::common::errors::Error;
use crate::common::flags::{TargetArgs, Window};
use crate::common::progress::{Progress, thousands};
use aws_sdk_cloudwatchlogs::Client;
use chrono::{DateTime, Utc};
use futures::stream::{FuturesUnordered, StreamExt};
use serde::Serialize;
use serde_json::json;

/// Histogram resolution. Roughly one bucket per minute over an hour, and
/// still legible as a sparkline for a 24-hour window.
const BUCKETS: usize = 60;

/// Safety rail on glob fan-out. Scanning every log group in an account by
/// accident is slow and expensive, so a wide glob truncates rather than
/// quietly running for minutes.
pub const DEFAULT_MAX_GROUPS: usize = 20;

#[derive(Debug, Serialize)]
pub struct LogGroup {
    pub name: String,
    pub retention_days: Option<i32>,
    pub stored_bytes: Option<i64>,
    pub created: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct RawEvent {
    pub ts: DateTime<Utc>,
    pub group: String,
    pub stream: String,
    pub message: String,
}

fn client(cfg: &aws_types::SdkConfig) -> Client {
    Client::new(cfg)
}

fn is_glob(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

fn to_utc(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_else(Utc::now)
}

// ---------------------------------------------------------------------------
// groups
// ---------------------------------------------------------------------------

/// List log groups, optionally filtered by prefix or glob.
pub async fn groups(
    target: &TargetArgs,
    pattern: Option<&str>,
    limit: Option<usize>,
    progress: &Progress,
) -> Result<Envelope<LogGroup>, Error> {
    let profile = single_profile(target)?;
    progress.phase("resolving credentials");
    let cfg = aws::config_for(&profile, target.region.as_deref()).await;
    let c = client(&cfg);

    progress.phase("listing log groups");
    let (names, truncated) = discover(&c, pattern, limit.unwrap_or(usize::MAX), &profile).await?;
    progress.finish(format!("{} log groups", thousands(names.len())));

    Ok(Envelope::new(
        "logs groups",
        json!({ "profile": profile, "pattern": pattern }),
        names,
    )
    .truncated(truncated))
}

/// Fetch log group metadata, applying a prefix server-side where possible and
/// a glob client-side otherwise.
async fn discover(
    c: &Client,
    pattern: Option<&str>,
    limit: usize,
    profile: &str,
) -> Result<(Vec<LogGroup>, bool), Error> {
    // A glob's literal prefix still narrows the server-side query, so
    // `/aws/lambda/*` does not page through every group in the account.
    let server_prefix = pattern.map(|p| match p.find(['*', '?', '[']) {
        Some(i) => &p[..i],
        None => p,
    });
    let matcher = pattern
        .filter(|p| is_glob(p))
        .map(|p| {
            globset::Glob::new(p)
                .map(|g| g.compile_matcher())
                .map_err(|e| Error::Aws {
                    operation: "logs:DescribeLogGroups".into(),
                    message: format!("bad group pattern {p:?}: {e}"),
                })
        })
        .transpose()?;

    let mut out = Vec::new();
    let mut token: Option<String> = None;
    let mut truncated = false;

    loop {
        let mut req = c.describe_log_groups();
        if let Some(p) = server_prefix.filter(|p| !p.is_empty()) {
            req = req.log_group_name_prefix(p);
        }
        if let Some(t) = &token {
            req = req.next_token(t);
        }
        let page = req
            .send()
            .await
            .map_err(|e| aws::map_sdk_error(e, profile, "logs:DescribeLogGroups", true))?;

        for g in page.log_groups() {
            let Some(name) = g.log_group_name() else {
                continue;
            };
            if let Some(m) = &matcher
                && !m.is_match(name)
            {
                continue;
            }
            if out.len() >= limit {
                truncated = true;
                break;
            }
            out.push(LogGroup {
                name: name.to_string(),
                retention_days: g.retention_in_days(),
                stored_bytes: g.stored_bytes(),
                created: g.creation_time().map(to_utc),
            });
        }
        token = page.next_token().map(ToString::to_string);
        if token.is_none() || truncated {
            break;
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((out, truncated))
}

// ---------------------------------------------------------------------------
// scan
// ---------------------------------------------------------------------------

pub struct ScanRequest<'a> {
    pub group: &'a str,
    pub window: Window,
    pub filter: Option<&'a str>,
    pub limit: Option<usize>,
    pub max_groups: usize,
}

pub struct ScanResult {
    pub clusters: Vec<Cluster>,
    pub events: Vec<RawEvent>,
    pub groups_scanned: Vec<String>,
    pub truncated: bool,
}

impl ScanResult {
    /// The time range actually covered by the returned events.
    ///
    /// CloudWatch returns events ascending from the window start, so hitting
    /// a limit yields the *oldest* slice of the window, not the newest. A
    /// caller that assumed otherwise would analyse stale data and conclude
    /// the wrong thing; reporting real coverage makes the shortfall visible.
    pub fn coverage(&self) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        let first = self.events.iter().map(|e| e.ts).min()?;
        let last = self.events.iter().map(|e| e.ts).max()?;
        Some((first, last))
    }

    /// Coverage rendered for the envelope's `params`, with an explicit note
    /// when the window was not fully covered.
    pub fn coverage_json(&self, window: Window) -> serde_json::Value {
        match self.coverage() {
            None => json!({ "events_from": null, "events_to": null }),
            Some((from, to)) => {
                let mut v = json!({
                    "events_from": from.to_rfc3339(),
                    "events_to": to.to_rfc3339(),
                });
                if self.truncated {
                    v["note"] = json!(format!(
                        "truncated: events cover {} to {} of the requested window ending {}. \
                         CloudWatch returns oldest first, so the newest events are NOT included \
                         -- narrow --since or raise --limit.",
                        from.to_rfc3339(),
                        to.to_rfc3339(),
                        window.until.to_rfc3339()
                    ));
                }
                v
            }
        }
    }
}

/// Fetch events for a scan, fanning out across every group the pattern
/// selects. Returns raw events; clustering is applied by the caller so
/// `--no-cluster` can reuse the same fetch.
pub async fn fetch(
    target: &TargetArgs,
    req: &ScanRequest<'_>,
    progress: &Progress,
) -> Result<(ScanResult, String), Error> {
    let profile = single_profile(target)?;
    // A cold credential cache is a real pause that otherwise looks like a hang.
    progress.phase("resolving credentials");
    let cfg = aws::config_for(&profile, target.region.as_deref()).await;
    let c = client(&cfg);

    let mut truncated = false;
    let group_names: Vec<String> = if is_glob(req.group) {
        let (found, more) = discover(&c, Some(req.group), req.max_groups, &profile).await?;
        truncated |= more;
        if found.is_empty() {
            return Err(Error::Aws {
                operation: "logs:DescribeLogGroups".into(),
                message: format!("no log group matches {:?}", req.group),
            });
        }
        found.into_iter().map(|g| g.name).collect()
    } else {
        vec![req.group.to_string()]
    };

    // Per-group budget, so one noisy group cannot consume the whole limit and
    // leave the others unrepresented.
    let per_group = req.limit.map(|l| l.div_ceil(group_names.len()).max(1));

    let mut tasks = FuturesUnordered::new();
    for name in &group_names {
        let c = c.clone();
        let profile = profile.clone();
        let filter = req.filter.map(ToString::to_string);
        let window = req.window;
        let name = name.clone();
        tasks.push(async move {
            fetch_group(&c, &profile, &name, window, filter.as_deref(), per_group).await
        });
    }

    let mut events = Vec::new();
    while let Some(result) = tasks.next().await {
        let (mut got, more) = result?;
        truncated |= more;
        events.append(&mut got);
    }
    events.sort_by_key(|e| e.ts);

    Ok((
        ScanResult {
            clusters: Vec::new(),
            events,
            groups_scanned: group_names,
            truncated,
        },
        profile,
    ))
}

async fn fetch_group(
    c: &Client,
    profile: &str,
    group: &str,
    window: Window,
    filter: Option<&str>,
    limit: Option<usize>,
) -> Result<(Vec<RawEvent>, bool), Error> {
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    let cap = limit.unwrap_or(usize::MAX);

    loop {
        let mut req = c
            .filter_log_events()
            .log_group_name(group)
            .start_time(window.since.timestamp_millis())
            .end_time(window.until.timestamp_millis());
        if let Some(f) = filter {
            req = req.filter_pattern(f);
        }
        if let Some(t) = &token {
            req = req.next_token(t);
        }
        let page = req
            .send()
            .await
            .map_err(|e| aws::map_sdk_error(e, profile, "logs:FilterLogEvents", true))?;

        for e in page.events() {
            if out.len() >= cap {
                // More matched than was asked for. Reporting this is the
                // difference between a partial answer and a wrong one.
                return Ok((out, true));
            }
            out.push(RawEvent {
                ts: e.timestamp().map(to_utc).unwrap_or_else(Utc::now),
                group: group.to_string(),
                stream: e.log_stream_name().unwrap_or_default().to_string(),
                message: e.message().unwrap_or_default().to_string(),
            });
        }
        token = page.next_token().map(ToString::to_string);
        if token.is_none() {
            return Ok((out, false));
        }
    }
}

/// Cluster fetched events, optionally classifying against a baseline period.
pub async fn cluster_events(
    target: &TargetArgs,
    req: &ScanRequest<'_>,
    events: &[RawEvent],
    baseline: Option<Window>,
    progress: &Progress,
) -> Result<Vec<Cluster>, Error> {
    progress.phase(format!("clustering {} events", thousands(events.len())));
    let log_events: Vec<LogEvent> = events.iter().map(to_log_event).collect();
    let mut clusters = template::cluster(&log_events, req.window.since, req.window.until, BUCKETS);

    if let Some(base_window) = baseline {
        // The baseline scans its own window, roughly doubling the work; a
        // distinct phase explains why the command suddenly takes twice as long.
        progress.phase("scanning baseline period");
        let base_req = ScanRequest {
            window: base_window,
            ..*req
        };
        let (base, _) = fetch(target, &base_req, progress).await?;
        progress.phase("classifying against baseline");
        let base_events: Vec<LogEvent> = base.events.iter().map(to_log_event).collect();
        let counts = template::baseline_counts(&base_events);
        template::apply_baseline(
            &mut clusters,
            &counts,
            (req.window.until - req.window.since).num_seconds(),
            (base_window.until - base_window.since).num_seconds(),
        );
    }
    Ok(clusters)
}

fn to_log_event(e: &RawEvent) -> LogEvent {
    LogEvent {
        timestamp: e.ts,
        stream: e.stream.clone(),
        message: e.message.clone(),
    }
}

// ---------------------------------------------------------------------------
// drill
// ---------------------------------------------------------------------------

/// Return the verbatim events behind one cluster.
///
/// Cluster ids are a hash of the template and cannot be inverted, so this
/// re-runs the scan and keeps the events whose template hashes to the same
/// id. That means the same `--group` and a window overlapping the original.
pub fn select_cluster<'a>(
    events: &'a [RawEvent],
    cluster_id: &str,
    stream: Option<&str>,
) -> Vec<&'a RawEvent> {
    events
        .iter()
        .filter(|e| stream.is_none_or(|s| e.stream == s))
        .filter(|e| template::cluster_id(&tokenize::template(&e.message)) == cluster_id)
        .collect()
}

fn single_profile(target: &TargetArgs) -> Result<String, Error> {
    let profiles = aws::resolve_targets(target)?;
    // Log scans are heavy; fanning one across accounts silently would be a
    // surprising amount of work from a single flag.
    profiles
        .into_iter()
        .next()
        .ok_or_else(|| Error::NoProfileMatch {
            pattern: target.selector().to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(stream: &str, msg: &str) -> RawEvent {
        RawEvent {
            ts: Utc::now(),
            group: "/g".into(),
            stream: stream.into(),
            message: msg.into(),
        }
    }

    #[test]
    fn glob_detection_distinguishes_patterns_from_literal_names() {
        assert!(is_glob("/aws/lambda/*"));
        assert!(is_glob("/aws/?"));
        assert!(!is_glob("/aws/lambda/my-function"));
        // A literal name containing a hyphen or dot is not a glob.
        assert!(!is_glob("/aws/rds/instance/db-1/error.log"));
    }

    #[test]
    fn drill_selects_only_events_belonging_to_the_cluster() {
        let events = vec![
            ev("i-a", "Connection to 10.0.1.5:5432 failed after 12ms"),
            ev("i-b", "Connection to 10.0.9.9:5432 failed after 90ms"),
            ev("i-a", "Heartbeat ok"),
        ];
        let id = template::cluster_id(&tokenize::template(&events[0].message));
        let picked = select_cluster(&events, &id, None);
        assert_eq!(
            picked.len(),
            2,
            "both connection failures, not the heartbeat"
        );
        assert!(picked.iter().all(|e| e.message.starts_with("Connection")));
    }

    #[test]
    fn drill_can_narrow_to_a_single_stream() {
        // "Is it everywhere or one host" is answered by by_stream; this is
        // how you then read that one host's actual lines.
        let events = vec![
            ev("i-a", "Connection to 10.0.1.5:5432 failed after 12ms"),
            ev("i-b", "Connection to 10.0.9.9:5432 failed after 90ms"),
        ];
        let id = template::cluster_id(&tokenize::template(&events[0].message));
        let picked = select_cluster(&events, &id, Some("i-b"));
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].stream, "i-b");
    }

    #[test]
    fn drill_returns_verbatim_lines_not_templates() {
        let events = vec![ev("i-a", "Connection to 10.0.1.5:5432 failed after 12ms")];
        let id = template::cluster_id(&tokenize::template(&events[0].message));
        let picked = select_cluster(&events, &id, None);
        assert_eq!(
            picked[0].message,
            "Connection to 10.0.1.5:5432 failed after 12ms"
        );
    }

    fn at(h: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 9, 4, h, 0, 0).unwrap()
    }

    fn scan_of(hours: &[u32], truncated: bool) -> ScanResult {
        ScanResult {
            clusters: vec![],
            events: hours
                .iter()
                .map(|h| RawEvent {
                    ts: at(*h),
                    group: "/g".into(),
                    stream: "s".into(),
                    message: "m".into(),
                })
                .collect(),
            groups_scanned: vec!["/g".into()],
            truncated,
        }
    }

    #[test]
    fn coverage_reports_the_range_actually_returned() {
        assert_eq!(
            scan_of(&[10, 11, 12], false).coverage(),
            Some((at(10), at(12)))
        );
        assert_eq!(scan_of(&[], false).coverage(), None);
    }

    #[test]
    fn a_truncated_scan_says_the_newest_events_are_missing() {
        // The dangerous failure is silent: analysing the oldest slice of a
        // wide window and reporting it as current.
        let w = Window {
            since: at(0),
            until: at(23),
        };
        let v = scan_of(&[1, 2], true).coverage_json(w);
        let note = v["note"].as_str().expect("a truncated scan carries a note");
        assert!(note.contains("newest events are NOT included"), "{note}");
        assert!(note.contains("narrow --since"), "{note}");
    }

    #[test]
    fn a_complete_scan_carries_no_alarming_note() {
        let w = Window {
            since: at(0),
            until: at(23),
        };
        let v = scan_of(&[1, 2], false).coverage_json(w);
        assert!(v.get("note").is_none());
        assert_eq!(v["events_from"], json!(at(1).to_rfc3339()));
    }

    #[test]
    fn an_unknown_cluster_id_selects_nothing_rather_than_everything() {
        let events = vec![ev("i-a", "anything")];
        assert!(select_cluster(&events, "ffffff", None).is_empty());
    }
}
