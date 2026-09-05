//! `awsdiag ec2` — instance inventory, detail and health.

use crate::common::aws;
use crate::common::envelope::Envelope;
use crate::common::errors::Error;
use crate::common::flags::TargetArgs;
use crate::common::progress::{Progress, thousands};
use aws_sdk_ec2::Client;
use aws_sdk_ec2::types::{Filter, Instance};
use chrono::{DateTime, Utc};

/// Render an SDK timestamp as a readable instant.
fn iso(t: &aws_smithy_types::DateTime) -> String {
    DateTime::from_timestamp(t.secs(), 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "?".into())
}
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Serialize)]
pub struct InstanceRow {
    pub instance_id: String,
    pub name: Option<String>,
    pub state: Option<String>,
    pub instance_type: Option<String>,
    pub az: Option<String>,
    pub private_ip: Option<String>,
    pub launched: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthRow {
    pub instance_id: String,
    pub name: Option<String>,
    pub state: Option<String>,
    /// `ok`, `impaired`, `insufficient-data`, `not-applicable`, `initializing`.
    pub system_status: Option<String>,
    pub instance_status: Option<String>,
    /// Scheduled retirements, reboots and maintenance. Present here because
    /// an instance can be perfectly healthy right now and still be scheduled
    /// for a stop tonight, which is exactly the thing a status check hides.
    pub events: Vec<String>,
}

/// The `Name` tag, which is what a human calls the instance.
fn name_of(i: &Instance) -> Option<String> {
    i.tags()
        .iter()
        .find(|t| t.key() == Some("Name"))
        .and_then(|t| t.value())
        .map(ToString::to_string)
}

fn row(i: &Instance) -> InstanceRow {
    InstanceRow {
        instance_id: i.instance_id().unwrap_or_default().to_string(),
        name: name_of(i),
        state: i
            .state()
            .and_then(|s| s.name())
            .map(|n| n.as_str().to_string()),
        instance_type: i.instance_type().map(|t| t.as_str().to_string()),
        az: i
            .placement()
            .and_then(|p| p.availability_zone())
            .map(ToString::to_string),
        private_ip: i.private_ip_address().map(ToString::to_string),
        launched: i
            .launch_time()
            .and_then(|t| DateTime::from_timestamp(t.secs(), 0)),
    }
}

/// Build the server-side filters for `ls`.
///
/// Filtering server-side matters: an account with thousands of instances
/// would otherwise page all of them back just to discard most locally.
pub fn build_filters(state: Option<&str>, name: Option<&str>) -> Vec<Filter> {
    let mut filters = Vec::new();
    if let Some(s) = state {
        filters.push(
            Filter::builder()
                .name("instance-state-name")
                .values(s)
                .build(),
        );
    }
    if let Some(n) = name {
        // EC2 filter values take `*` and `?` wildcards natively, so a glob
        // works without fetching everything and matching locally.
        filters.push(Filter::builder().name("tag:Name").values(n).build());
    }
    filters
}

pub async fn ls(
    target: &TargetArgs,
    state: Option<&str>,
    name: Option<&str>,
    limit: Option<usize>,
    progress: &Progress,
) -> Result<Envelope<InstanceRow>, Error> {
    let profile = single_profile(target)?;
    progress.phase("resolving credentials");
    let cfg = aws::config_for(&profile, target.region.as_deref()).await;
    let client = Client::new(&cfg);

    progress.phase("describing instances");
    let (rows, truncated) = describe(
        &client,
        &profile,
        build_filters(state, name),
        &[],
        limit,
        progress,
    )
    .await?;
    progress.finish(format!("{} instances", thousands(rows.len())));

    Ok(Envelope::new(
        "ec2 ls",
        json!({ "profile": profile, "state": state, "name": name }),
        rows,
    )
    .truncated(truncated))
}

async fn describe(
    client: &Client,
    profile: &str,
    filters: Vec<Filter>,
    ids: &[String],
    limit: Option<usize>,
    progress: &Progress,
) -> Result<(Vec<InstanceRow>, bool), Error> {
    let cap = limit.unwrap_or(usize::MAX);
    let mut rows = Vec::new();
    let mut token: Option<String> = None;
    let mut page = 0usize;

    loop {
        page += 1;
        progress.phase(format!("describing instances · page {page}"));
        let mut req = client.describe_instances();
        if !filters.is_empty() {
            req = req.set_filters(Some(filters.clone()));
        }
        // Narrow server-side when specific instances were asked for; fetching
        // every instance in the account just to read a handful of Name tags
        // is slow in a large estate and pointless in any.
        if !ids.is_empty() {
            req = req.set_instance_ids(Some(ids.to_vec()));
        }
        if let Some(t) = &token {
            req = req.next_token(t);
        }
        let out = req
            .send()
            .await
            .map_err(|e| aws::map_sdk_error(e, profile, "ec2:DescribeInstances", true))?;

        for reservation in out.reservations() {
            for instance in reservation.instances() {
                rows.push(row(instance));
            }
        }
        token = out.next_token().map(ToString::to_string);
        if token.is_none() {
            break;
        }
    }
    // Sort first, then cap. Capping mid-page returned an arbitrary subset in
    // API order, so `--limit 5` gave five different instances run to run and
    // a different ordering from the uncapped command. `--limit` bounds the
    // output, and a diagnostic tool that answers differently each time is
    // worth an extra page of paging to avoid.
    sort_rows(&mut rows);
    let truncated = rows.len() > cap;
    rows.truncate(cap);
    Ok((rows, truncated))
}

#[derive(Debug, Serialize)]
pub struct VolumeRef {
    pub device: Option<String>,
    pub volume_id: Option<String>,
    pub delete_on_termination: Option<bool>,
    pub attach_status: Option<String>,
}

/// One instance in full: what `ls` shows, plus what it is made of.
#[derive(Debug, Serialize)]
pub struct InstanceDetail {
    #[serde(flatten)]
    pub summary: InstanceRow,
    pub vpc_id: Option<String>,
    pub subnet_id: Option<String>,
    pub image_id: Option<String>,
    pub key_name: Option<String>,
    pub iam_profile: Option<String>,
    pub public_ip: Option<String>,
    /// Whether CloudWatch receives 1-minute rather than 5-minute data. This
    /// decides what resolution `metrics compare` can actually return for the
    /// instance, so it belongs next to the instance rather than in a doc.
    pub detailed_monitoring: bool,
    pub security_groups: Vec<String>,
    pub volumes: Vec<VolumeRef>,
    pub tags: std::collections::BTreeMap<String, String>,
}

fn detail_of(i: &Instance) -> InstanceDetail {
    InstanceDetail {
        summary: row(i),
        vpc_id: i.vpc_id().map(ToString::to_string),
        subnet_id: i.subnet_id().map(ToString::to_string),
        image_id: i.image_id().map(ToString::to_string),
        key_name: i.key_name().map(ToString::to_string),
        iam_profile: i
            .iam_instance_profile()
            .and_then(|p| p.arn())
            .map(ToString::to_string),
        public_ip: i.public_ip_address().map(ToString::to_string),
        detailed_monitoring: i
            .monitoring()
            .and_then(|m| m.state())
            .is_some_and(|st| st.as_str() == "enabled"),
        security_groups: i
            .security_groups()
            .iter()
            .filter_map(|g| g.group_name().or_else(|| g.group_id()))
            .map(ToString::to_string)
            .collect(),
        volumes: i
            .block_device_mappings()
            .iter()
            .map(|m| VolumeRef {
                device: m.device_name().map(ToString::to_string),
                volume_id: m.ebs().and_then(|e| e.volume_id()).map(ToString::to_string),
                delete_on_termination: m.ebs().and_then(|e| e.delete_on_termination()),
                attach_status: m
                    .ebs()
                    .and_then(|e| e.status())
                    .map(|s| s.as_str().to_string()),
            })
            .collect(),
        tags: i
            .tags()
            .iter()
            .filter_map(|t| {
                Some((
                    t.key()?.to_string(),
                    t.value().unwrap_or_default().to_string(),
                ))
            })
            .collect(),
    }
}

/// `ec2 show` — full detail for named instances.
pub async fn show(
    target: &TargetArgs,
    instances: &[String],
    name: Option<&str>,
    progress: &Progress,
) -> Result<Envelope<InstanceDetail>, Error> {
    if instances.is_empty() && name.is_none() {
        return Err(Error::BadSpec {
            spec: String::new(),
            reason: "give --instance or --name; showing every instance is what `ls` is for".into(),
        });
    }
    let profile = single_profile(target)?;
    progress.phase("resolving credentials");
    let cfg = aws::config_for(&profile, target.region.as_deref()).await;
    let client = Client::new(&cfg);

    progress.phase("describing instances");
    let mut req = client.describe_instances();
    if !instances.is_empty() {
        req = req.set_instance_ids(Some(instances.to_vec()));
    }
    let filters = build_filters(None, name);
    if !filters.is_empty() {
        req = req.set_filters(Some(filters));
    }
    let out = req
        .send()
        .await
        .map_err(|e| aws::map_sdk_error(e, &profile, "ec2:DescribeInstances", true))?;

    let mut rows: Vec<InstanceDetail> = out
        .reservations()
        .iter()
        .flat_map(|r| r.instances())
        .map(detail_of)
        .collect();
    rows.sort_by(|a, b| {
        a.summary
            .name
            .cmp(&b.summary.name)
            .then(a.summary.instance_id.cmp(&b.summary.instance_id))
    });
    progress.finish(format!("{} instances", rows.len()));

    Ok(Envelope::new(
        "ec2 show",
        json!({ "profile": profile, "instances": instances, "name": name }),
        rows,
    ))
}

fn sort_rows(rows: &mut [InstanceRow]) {
    rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.instance_id.cmp(&b.instance_id)));
}

/// Whether a row deserves an operator's attention.
///
/// One predicate drives both the ordering and the summary count. They were
/// separate and disagreed: the sort treated a scheduled event as concerning
/// while the count looked only at status checks, so an instance with a
/// pending retirement sorted to the top of a table headed `0 not ok`.
///
/// A stopped instance is not concerning. AWS reports `not-applicable` or
/// `insufficient-data` for anything not running, and counting that as a
/// failure sorted every deliberately-stopped instance above the running ones,
/// burying a genuinely impaired host -- the opposite of what "worst first" is
/// for.
pub fn is_concerning(row: &HealthRow) -> bool {
    if row.state.as_deref() != Some("running") {
        return false;
    }
    row.system_status.as_deref() != Some("ok")
        || row.instance_status.as_deref() != Some("ok")
        || !row.events.is_empty()
}

pub async fn health(
    target: &TargetArgs,
    instances: &[String],
    limit: Option<usize>,
    progress: &Progress,
) -> Result<Envelope<HealthRow>, Error> {
    let profile = single_profile(target)?;
    progress.phase("resolving credentials");
    let cfg = aws::config_for(&profile, target.region.as_deref()).await;
    let client = Client::new(&cfg);

    // Names come from DescribeInstances; status checks come from
    // DescribeInstanceStatus. Both are needed for a row a person can read.
    progress.phase("reading instance names");
    let (named, _) = describe(&client, &profile, Vec::new(), instances, None, progress).await?;
    // A map, not a linear scan per row: the scan made name resolution O(n·m)
    // over an account-sized list.
    let names: std::collections::HashMap<&str, &String> = named
        .iter()
        .filter_map(|r| r.name.as_ref().map(|n| (r.instance_id.as_str(), n)))
        .collect();
    let name_for = |id: &str| names.get(id).map(|n| (*n).clone());

    progress.phase("reading status checks");
    let cap = limit.unwrap_or(usize::MAX);
    let mut statuses = Vec::new();
    let mut token: Option<String> = None;
    let mut truncated = false;
    let mut page = 0usize;

    // DescribeInstanceStatus pages at 1000. A single un-paginated call in a
    // fleet larger than that returned a page-one view labelled complete, and
    // the worst-first sort could only rank what had been fetched -- an
    // impaired instance on page two was invisible.
    loop {
        page += 1;
        progress.phase(format!("reading status checks · page {page}"));
        let mut req = client
            .describe_instance_status()
            .include_all_instances(true);
        if !instances.is_empty() {
            req = req.set_instance_ids(Some(instances.to_vec()));
        }
        if let Some(t) = &token {
            req = req.next_token(t);
        }
        let out = req.send().await.map_err(|e| {
            aws::map_sdk_error(e, profile.as_str(), "ec2:DescribeInstanceStatus", true)
        })?;

        for status in out.instance_statuses() {
            if statuses.len() >= cap {
                truncated = true;
                break;
            }
            statuses.push(status.clone());
        }
        token = out.next_token().map(ToString::to_string);
        if token.is_none() || truncated {
            break;
        }
    }

    let mut rows: Vec<HealthRow> = statuses
        .iter()
        .map(|s| {
            let id = s.instance_id().unwrap_or_default().to_string();
            HealthRow {
                name: name_for(&id),
                state: s
                    .instance_state()
                    .and_then(|st| st.name())
                    .map(|n| n.as_str().to_string()),
                system_status: s
                    .system_status()
                    .and_then(|x| x.status())
                    .map(|x| x.as_str().to_string()),
                instance_status: s
                    .instance_status()
                    .and_then(|x| x.status())
                    .map(|x| x.as_str().to_string()),
                // The window is the point: "scheduled for a stop" cannot be
                // triaged without knowing whether that means tonight or in
                // three months.
                events: s
                    .events()
                    .iter()
                    .map(|e| {
                        let when = match (e.not_before(), e.not_after()) {
                            (Some(a), Some(b)) => format!(" [{} to {}]", iso(a), iso(b)),
                            (Some(a), None) => format!(" [from {}]", iso(a)),
                            (None, Some(b)) => format!(" [by {}]", iso(b)),
                            (None, None) => String::new(),
                        };
                        format!(
                            "{}: {}{when}",
                            e.code().map(|c| c.as_str()).unwrap_or("event"),
                            e.description().unwrap_or_default()
                        )
                    })
                    .collect(),
                instance_id: id,
            }
        })
        .collect();

    // Anything concerning first: a healthy fleet should need no scrolling to
    // find the one box that is not.
    rows.sort_by_key(|r| (!is_concerning(r), r.name.clone(), r.instance_id.clone()));
    let concerning = rows.iter().filter(|r| is_concerning(r)).count();
    progress.finish(format!(
        "{} instances, {concerning} needing attention",
        rows.len()
    ));

    Ok(Envelope::new(
        "ec2 health",
        json!({ "profile": profile, "instances": instances }),
        rows,
    ))
}

fn single_profile(target: &TargetArgs) -> Result<String, Error> {
    aws::resolve_targets(target)?
        .into_iter()
        .next()
        .ok_or_else(|| Error::NoProfileMatch {
            pattern: target.selector().to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(filters: &[Filter]) -> Vec<&str> {
        filters.iter().filter_map(|f| f.name()).collect()
    }

    fn row_of(state: &str, sys: &str, inst: &str, events: &[&str]) -> HealthRow {
        HealthRow {
            instance_id: "i-placeholder".into(),
            name: Some("placeholder".into()),
            state: Some(state.into()),
            system_status: Some(sys.into()),
            instance_status: Some(inst.into()),
            events: events.iter().map(|e| (*e).to_string()).collect(),
        }
    }

    #[test]
    fn a_healthy_running_instance_is_not_concerning() {
        assert!(!is_concerning(&row_of("running", "ok", "ok", &[])));
    }

    #[test]
    fn an_impaired_running_instance_is_concerning() {
        assert!(is_concerning(&row_of("running", "impaired", "ok", &[])));
        assert!(is_concerning(&row_of("running", "ok", "impaired", &[])));
    }

    #[test]
    fn a_scheduled_event_is_concerning_even_when_every_check_passes() {
        // The reason events are collected at all: an instance can pass every
        // check and still be scheduled for a stop tonight.
        assert!(is_concerning(&row_of(
            "running",
            "ok",
            "ok",
            &["instance-stop: retiring"]
        )));
    }

    #[test]
    fn a_stopped_instance_is_not_concerning() {
        // AWS reports not-applicable / insufficient-data for anything not
        // running. Treating that as a failure sorted every deliberately
        // stopped instance above the running ones, burying a genuinely
        // impaired host -- the opposite of what "worst first" is for.
        for state in ["stopped", "terminated", "shutting-down", "stopping"] {
            assert!(
                !is_concerning(&row_of(state, "not-applicable", "not-applicable", &[])),
                "{state} should not be flagged"
            );
            assert!(
                !is_concerning(&row_of(
                    state,
                    "insufficient-data",
                    "insufficient-data",
                    &[]
                )),
                "{state} should not be flagged"
            );
        }
    }

    #[test]
    fn the_ordering_and_the_count_cannot_disagree() {
        // They were separate predicates and diverged: a pending retirement
        // sorted to the top of a table headed `0 not ok`. One function now
        // drives both, so this holds by construction -- and stays held.
        let rows = vec![
            row_of("running", "ok", "ok", &[]),
            row_of("running", "ok", "ok", &["instance-reboot: scheduled"]),
            row_of("stopped", "not-applicable", "not-applicable", &[]),
            row_of("running", "impaired", "ok", &[]),
        ];
        let mut sorted = rows.clone();
        sorted.sort_by_key(|r| (!is_concerning(r), r.name.clone(), r.instance_id.clone()));
        let count = rows.iter().filter(|r| is_concerning(r)).count();
        assert_eq!(count, 2, "the scheduled reboot and the impaired host");
        assert!(
            sorted.iter().take(count).all(is_concerning),
            "everything counted must sort first"
        );
        assert!(
            !sorted.iter().skip(count).any(is_concerning),
            "nothing after the counted rows may be concerning"
        );
    }

    #[test]
    fn no_criteria_produces_no_filters() {
        assert!(build_filters(None, None).is_empty());
    }

    #[test]
    fn state_and_name_become_server_side_filters() {
        // Filtering server-side matters: an account with thousands of
        // instances would otherwise page all of them back to discard most.
        let f = build_filters(Some("running"), Some("web-*"));
        assert_eq!(names(&f), vec!["instance-state-name", "tag:Name"]);
        assert_eq!(f[0].values(), ["running"]);
        assert_eq!(f[1].values(), ["web-*"]);
    }

    #[test]
    fn either_criterion_works_alone() {
        assert_eq!(
            names(&build_filters(Some("stopped"), None)),
            vec!["instance-state-name"]
        );
        assert_eq!(names(&build_filters(None, Some("db-?"))), vec!["tag:Name"]);
    }

    #[test]
    fn a_name_glob_is_passed_through_for_ec2_to_match() {
        // EC2 filter values support `*` and `?` natively; expanding them here
        // would mean fetching everything first, which is the cost this avoids.
        let f = build_filters(None, Some("app-*-01"));
        assert_eq!(f[0].values(), ["app-*-01"]);
    }
}
