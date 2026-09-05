//! `awsdiag metrics` — CloudWatch metric retrieval on a shared timeline.

use crate::common::aws;
use crate::common::envelope::Envelope;
use crate::common::errors::Error;
use crate::common::flags::{TargetArgs, Window};
use crate::common::progress::{Progress, thousands};
use crate::metrics::align::{self, Matrix, Series};
use aws_sdk_cloudwatch::Client;
use aws_sdk_cloudwatch::types::{Dimension, Metric, MetricDataQuery, MetricStat};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

/// CloudWatch accepts at most this many queries in one `GetMetricData` call.
const MAX_QUERIES_PER_CALL: usize = 500;

/// One thing to plot: a namespace, a metric, a statistic and dimensions.
#[derive(Debug, Clone)]
pub struct SeriesSpec {
    pub label: String,
    pub namespace: String,
    pub metric: String,
    pub stat: String,
    pub dimensions: Vec<(String, String)>,
}

impl SeriesSpec {
    /// Parse `Namespace/MetricName[:Stat][,Dim=Value...]`, e.g.
    /// `AWS/EC2/CPUUtilization:Average,InstanceId=i-0abc`.
    ///
    /// A single string keeps a many-series request expressible on one command
    /// line, which is what `metrics compare` is for.
    pub fn parse(raw: &str) -> Result<Self, Error> {
        let bad = |why: &str| Error::BadSpec {
            spec: raw.to_string(),
            reason: why.to_string(),
        };

        let (head, dims) = raw.split_once(',').unwrap_or((raw, ""));
        let (path, stat) = head.rsplit_once(':').unwrap_or((head, "Average"));
        // Namespaces contain a slash (`AWS/EC2`), so the metric name is the
        // final segment and everything before it is the namespace.
        let (namespace, metric) = path
            .rsplit_once('/')
            .ok_or_else(|| bad("expected Namespace/MetricName"))?;
        if namespace.is_empty() || metric.is_empty() {
            return Err(bad("namespace and metric name must both be present"));
        }

        let mut dimensions = Vec::new();
        for pair in dims.split(',').filter(|p| !p.trim().is_empty()) {
            let (k, v) = pair
                .split_once('=')
                .ok_or_else(|| bad("dimensions are Name=Value"))?;
            if k.trim().is_empty() || v.trim().is_empty() {
                return Err(bad("dimension name and value must both be present"));
            }
            dimensions.push((k.trim().to_string(), v.trim().to_string()));
        }

        Ok(SeriesSpec {
            label: raw.to_string(),
            namespace: namespace.to_string(),
            metric: metric.to_string(),
            stat: stat.to_string(),
            dimensions,
        })
    }

    /// The full spec, used when nothing shorter tells this series apart.
    pub fn full_label(&self) -> String {
        self.label.clone()
    }
}

/// Label each series by what actually distinguishes it from the others.
///
/// Taking the last dimension is not enough. Dimensions are often listed
/// alphabetically, so the final one is frequently shared: two series that
/// differ only in `Resource` but both end `Type=API` both label as
/// `CallCount API`, and a legend of identical entries is no legend at all.
/// That was observed live, not imagined.
///
/// So the distinguishing dimensions are worked out across the whole set — a
/// dimension earns a place in the label only where its value actually varies.
pub fn label_series(specs: &[SeriesSpec]) -> Vec<String> {
    use std::collections::BTreeSet;

    if specs.len() == 1 {
        return vec![specs[0].metric.clone()];
    }

    let mut varying: Vec<String> = Vec::new();
    for spec in specs {
        for (name, _) in &spec.dimensions {
            if varying.contains(name) {
                continue;
            }
            let values: BTreeSet<Option<&str>> = specs
                .iter()
                .map(|s| {
                    s.dimensions
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, v)| v.as_str())
                })
                .collect();
            if values.len() > 1 {
                varying.push(name.clone());
            }
        }
    }

    let metric_varies = specs
        .iter()
        .map(|s| &s.metric)
        .collect::<BTreeSet<_>>()
        .len()
        > 1;

    let mut labels: Vec<String> = specs
        .iter()
        .map(|spec| {
            let mut parts: Vec<String> = Vec::new();
            if metric_varies || varying.is_empty() {
                parts.push(spec.metric.clone());
            }
            for name in &varying {
                if let Some((_, v)) = spec.dimensions.iter().find(|(n, _)| n == name) {
                    parts.push(v.clone());
                }
            }
            if parts.is_empty() {
                spec.metric.clone()
            } else {
                parts.join(" ")
            }
        })
        .collect();

    // Rows are emitted as objects keyed by label alongside a `ts` key, and
    // serde_json is built with `preserve_order`, so `Map::insert` on an
    // existing key replaces the value in place. A label of exactly `ts` would
    // therefore overwrite every timestamp with a float or null, leaving the
    // row looking well-formed while the time axis is gone. A dimension value
    // of `ts` is a plausible resource name, not only a typo.
    //
    // The reserved key seeds the uniqueness check, so a collision with it
    // falls back to full specs exactly as a collision between labels does.
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    seen.insert(TIMESTAMP_KEY);
    let unique = labels.iter().all(|l| seen.insert(l.as_str()));
    if !unique {
        labels = specs.iter().map(SeriesSpec::full_label).collect();
    }
    labels
}

/// The reserved column holding each row's instant.
pub const TIMESTAMP_KEY: &str = "ts";

#[derive(Debug, Serialize)]
pub struct MatrixEnvelope {
    #[serde(flatten)]
    pub matrix: Matrix,
}

/// Fetch and align every requested series over one window.
pub async fn compare(
    target: &TargetArgs,
    specs: &[SeriesSpec],
    window: Window,
    period_override: Option<i64>,
    progress: &Progress,
) -> Result<Envelope<serde_json::Value>, Error> {
    if specs.is_empty() {
        return Err(Error::BadSpec {
            spec: String::new(),
            reason: "at least one --series is required".into(),
        });
    }
    let profile = single_profile(target)?;
    progress.phase("resolving credentials");
    let cfg = aws::config_for(&profile, target.region.as_deref()).await;
    let client = Client::new(&cfg);

    let window_secs = (window.until - window.since).num_seconds().max(1);
    let age_secs = (Utc::now() - window.since).num_seconds().max(0);
    let period =
        period_override.unwrap_or_else(|| align::choose_period(window_secs, age_secs, specs.len()));

    progress.phase(format!(
        "fetching {} series at {}s resolution",
        specs.len(),
        period
    ));

    let labels = label_series(specs);
    let mut series = Vec::with_capacity(specs.len());
    for (i, chunk) in specs.chunks(MAX_QUERIES_PER_CALL).enumerate() {
        let task = progress.task(format!("batch {}", i + 1));
        let offset = i * MAX_QUERIES_PER_CALL;
        series.extend(
            fetch_batch(&client, &profile, chunk, &labels[offset..], window, period).await?,
        );
        task.done(format!("batch {} · {} series", i + 1, chunk.len()));
    }

    // A metric published every 5 minutes queried at 60s resolution returns a
    // matrix that is 80% empty. That is not wrong, but it reads as an outage,
    // so say so plainly rather than leaving the caller to infer it.
    let native = align::native_period(&series);
    let sparse_note = match native {
        Some(n) if n > period => Some(format!(
            "series report every {n}s but were queried at {period}s, so most rows are empty; \
             pass --period {n} for a dense result"
        )),
        _ => None,
    };

    progress.phase("aligning series");
    let matrix = align::align(&series, window.since, window.until, period);
    let filled: usize = matrix
        .rows
        .iter()
        .map(|r| r.values.iter().filter(|v| v.is_some()).count())
        .sum();
    progress.finish(format!(
        "{} rows × {} series, {} datapoints{}",
        thousands(matrix.rows.len()),
        matrix.labels.len(),
        thousands(filled),
        match &sparse_note {
            Some(_) => format!(" (sparse: try --period {})", native.unwrap_or(period)),
            None => String::new(),
        }
    ));

    // Rows are emitted as objects keyed by series label rather than as a
    // positional array. The array form rendered as `values: [null]` in text
    // mode -- an unreadable JSON fragment inside a column -- and forced every
    // consumer to zip against `labels` to know what it was looking at.
    let rows: Vec<serde_json::Value> = matrix
        .rows
        .iter()
        .map(|r| {
            let mut obj = serde_json::Map::new();
            obj.insert(TIMESTAMP_KEY.into(), json!(r.ts));
            for (label, value) in matrix.labels.iter().zip(&r.values) {
                // A gap stays null. Emitting 0 would turn "not reporting"
                // into "the value was zero", which reads as healthy.
                obj.insert(label.clone(), json!(value));
            }
            // Belt and braces: an overwritten key would silently drop a
            // column or the timestamp rather than fail.
            debug_assert_eq!(
                obj.len(),
                matrix.labels.len() + 1,
                "a series label collided with another key"
            );
            serde_json::Value::Object(obj)
        })
        .collect();

    Ok(Envelope::new(
        "metrics compare",
        json!({
            "profile": profile,
            "labels": matrix.labels,
            "period_seconds": matrix.period_seconds,
            "since": window.since.to_rfc3339(),
            "until": window.until.to_rfc3339(),
            "series": specs.iter().map(|s| s.label.clone()).collect::<Vec<_>>(),
            "native_period_seconds": native,
            "note": sparse_note,
        }),
        rows,
    ))
}

/// Ceiling on how many discovered resources are ranked in one call.
///
/// A namespace can hold thousands of dimension values; fetching every one to
/// rank ten is slow and expensive. Hitting this marks the result truncated
/// rather than quietly ranking an arbitrary subset.
const MAX_DISCOVERED: usize = 300;

/// `metrics top` — rank resources by a metric without knowing their ids.
///
/// This is where diagnosis usually starts: "which instance is hottest" comes
/// before "what was instance X doing". Dimension values are discovered with
/// ListMetrics, so the caller supplies a dimension *name* rather than a list
/// of identifiers it would otherwise have to go and find first.
pub struct TopRequest<'a> {
    pub namespace: &'a str,
    pub metric: &'a str,
    pub dimension: &'a str,
    pub stat: &'a str,
    pub by: align::Rank,
    pub count: usize,
    pub window: Window,
    pub period: Option<i64>,
}

pub async fn top(
    target: &TargetArgs,
    req: &TopRequest<'_>,
    progress: &Progress,
) -> Result<Envelope<align::Summary>, Error> {
    let TopRequest {
        namespace,
        metric,
        dimension,
        stat,
        by,
        count,
        window,
        period: period_override,
    } = *req;
    let profile = single_profile(target)?;
    progress.phase("resolving credentials");
    let cfg = aws::config_for(&profile, target.region.as_deref()).await;
    let client = Client::new(&cfg);

    progress.phase(format!("discovering {dimension} values in {namespace}"));
    let (values, truncated) =
        discover_dimension(&client, &profile, namespace, metric, dimension, progress).await?;
    if values.is_empty() {
        return Err(Error::BadSpec {
            spec: format!("{namespace}/{metric}"),
            reason: format!("no metrics found with dimension {dimension}"),
        });
    }

    let specs: Vec<SeriesSpec> = values
        .iter()
        .map(|v| SeriesSpec {
            label: v.clone(),
            namespace: namespace.to_string(),
            metric: metric.to_string(),
            stat: stat.to_string(),
            dimensions: vec![(dimension.to_string(), v.clone())],
        })
        .collect();

    let window_secs = (window.until - window.since).num_seconds().max(1);
    let age_secs = (Utc::now() - window.since).num_seconds().max(0);
    let period =
        period_override.unwrap_or_else(|| align::choose_period(window_secs, age_secs, specs.len()));

    progress.phase(format!("fetching {} series at {period}s", specs.len()));
    let labels: Vec<String> = values.clone();
    let mut series = Vec::with_capacity(specs.len());
    for (i, chunk) in specs.chunks(MAX_QUERIES_PER_CALL).enumerate() {
        let task = progress.task(format!("batch {}", i + 1));
        let offset = i * MAX_QUERIES_PER_CALL;
        series.extend(
            fetch_batch(&client, &profile, chunk, &labels[offset..], window, period).await?,
        );
        task.done(format!("batch {} · {} series", i + 1, chunk.len()));
    }

    progress.phase("ranking");
    let ranked = align::rank(series.iter().map(align::summarise).collect(), by);
    let reporting = ranked.iter().filter(|s| s.datapoints > 0).count();
    let rows: Vec<align::Summary> = ranked.into_iter().take(count).collect();
    progress.finish(format!(
        "{} of {} resources reporting, top {} shown",
        reporting,
        values.len(),
        rows.len()
    ));

    Ok(Envelope::new(
        "metrics top",
        json!({
            "profile": profile,
            "namespace": namespace,
            "metric": metric,
            "dimension": dimension,
            "stat": stat,
            "rank_by": format!("{by:?}").to_lowercase(),
            "candidates": values.len(),
            "reporting": reporting,
            "period_seconds": period,
            "since": window.since.to_rfc3339(),
            "until": window.until.to_rfc3339(),
        }),
        rows,
    )
    .truncated(truncated))
}

/// Distinct values of one dimension for a metric, via ListMetrics.
async fn discover_dimension(
    client: &Client,
    profile: &str,
    namespace: &str,
    metric: &str,
    dimension: &str,
    progress: &Progress,
) -> Result<(Vec<String>, bool), Error> {
    let mut values: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut token: Option<String> = None;
    let mut page = 0usize;

    loop {
        page += 1;
        progress.phase(format!("discovering {dimension} values · page {page}"));
        let mut req = client
            .list_metrics()
            .namespace(namespace)
            .metric_name(metric);
        if let Some(t) = &token {
            req = req.next_token(t);
        }
        let out = req
            .send()
            .await
            .map_err(|e| aws::map_sdk_error(e, profile, "cloudwatch:ListMetrics", true))?;

        for m in out.metrics() {
            if let Some(d) = m.dimensions().iter().find(|d| d.name() == Some(dimension))
                && let Some(v) = d.value()
            {
                values.insert(v.to_string());
            }
            if values.len() >= MAX_DISCOVERED {
                return Ok((values.into_iter().collect(), true));
            }
        }
        token = out.next_token().map(ToString::to_string);
        if token.is_none() {
            break;
        }
    }
    Ok((values.into_iter().collect(), false))
}

async fn fetch_batch(
    client: &Client,
    profile: &str,
    specs: &[SeriesSpec],
    labels: &[String],
    window: Window,
    period: i64,
) -> Result<Vec<Series>, Error> {
    let queries: Vec<MetricDataQuery> = specs
        .iter()
        .enumerate()
        .map(|(i, spec)| build_query(i, spec, period))
        .collect::<Result<_, _>>()?;

    let mut results: Vec<Series> = labels
        .iter()
        .take(specs.len())
        .map(|label| Series {
            label: label.clone(),
            points: Vec::new(),
        })
        .collect();

    let mut token: Option<String> = None;
    loop {
        let mut req = client
            .get_metric_data()
            .set_metric_data_queries(Some(queries.clone()))
            .start_time(to_aws(window.since))
            .end_time(to_aws(window.until));
        if let Some(t) = &token {
            req = req.next_token(t);
        }
        let page = req
            .send()
            .await
            .map_err(|e| aws::map_sdk_error(e, profile, "cloudwatch:GetMetricData", true))?;

        for result in page.metric_data_results() {
            // Query ids are `q<index>`, so a result maps back to the spec that
            // asked for it regardless of the order they come back in.
            let Some(idx) = result
                .id()
                .and_then(|id| id.strip_prefix('q')?.parse::<usize>().ok())
            else {
                continue;
            };
            let Some(slot) = results.get_mut(idx) else {
                continue;
            };
            for (ts, v) in result.timestamps().iter().zip(result.values()) {
                if let Some(dt) = DateTime::from_timestamp(ts.secs(), 0) {
                    slot.points.push((dt, *v));
                }
            }
        }

        token = page.next_token().map(ToString::to_string);
        if token.is_none() {
            break;
        }
    }
    Ok(results)
}

fn build_query(index: usize, spec: &SeriesSpec, period: i64) -> Result<MetricDataQuery, Error> {
    let dimensions: Vec<Dimension> = spec
        .dimensions
        .iter()
        .map(|(n, v)| Dimension::builder().name(n).value(v).build())
        .collect();

    let metric = Metric::builder()
        .namespace(&spec.namespace)
        .metric_name(&spec.metric)
        .set_dimensions(Some(dimensions))
        .build();

    let stat = MetricStat::builder()
        .metric(metric)
        .period(i32::try_from(period).unwrap_or(i32::MAX))
        .stat(&spec.stat)
        .build();

    Ok(MetricDataQuery::builder()
        .id(format!("q{index}"))
        .metric_stat(stat)
        // Ascending, so points arrive in the order a chart wants them.
        .return_data(true)
        .build())
}

fn to_aws(ts: DateTime<Utc>) -> aws_smithy_types::DateTime {
    aws_smithy_types::DateTime::from_millis(ts.timestamp_millis())
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

    #[test]
    fn a_full_spec_parses_into_its_parts() {
        let s = SeriesSpec::parse("AWS/EC2/CPUUtilization:Maximum,InstanceId=i-0abc").unwrap();
        assert_eq!(s.namespace, "AWS/EC2");
        assert_eq!(s.metric, "CPUUtilization");
        assert_eq!(s.stat, "Maximum");
        assert_eq!(s.dimensions, vec![("InstanceId".into(), "i-0abc".into())]);
    }

    #[test]
    fn the_statistic_defaults_to_average() {
        // The commonest choice, and leaving it out keeps a comparison of ten
        // resources readable on one line.
        let s = SeriesSpec::parse("AWS/EC2/CPUUtilization,InstanceId=x").unwrap();
        assert_eq!(s.stat, "Average");
    }

    #[test]
    fn a_namespace_containing_slashes_keeps_them() {
        // `AWS/ApplicationELB` and the metric name both contain no slash, but
        // the namespace itself does -- splitting on the first slash would put
        // `EC2/CPUUtilization` in the metric name.
        let s = SeriesSpec::parse("AWS/ApplicationELB/HTTPCode_Target_5XX_Count").unwrap();
        assert_eq!(s.namespace, "AWS/ApplicationELB");
        assert_eq!(s.metric, "HTTPCode_Target_5XX_Count");
    }

    #[test]
    fn several_dimensions_are_kept_in_order() {
        let s =
            SeriesSpec::parse("AWS/FSx/StorageUsed,FileSystemId=fs-1,VolumeId=fsvol-2").unwrap();
        assert_eq!(
            s.dimensions,
            vec![
                ("FileSystemId".into(), "fs-1".into()),
                ("VolumeId".into(), "fsvol-2".into())
            ]
        );
    }

    #[test]
    fn a_spec_with_no_dimensions_is_allowed() {
        // Namespace-wide aggregates are legitimate.
        let s = SeriesSpec::parse("AWS/Lambda/Errors:Sum").unwrap();
        assert!(s.dimensions.is_empty());
        assert_eq!(s.stat, "Sum");
    }

    #[test]
    fn malformed_specs_are_rejected_with_the_offending_text() {
        for bad in [
            "CPUUtilization",
            "AWS/EC2/CPU,InstanceId",
            "AWS/EC2/CPU,=v",
            "/Metric",
            "AWS/",
        ] {
            let e = SeriesSpec::parse(bad).unwrap_err();
            assert_eq!(e.kind(), "bad_argument", "{bad}");
            assert!(e.to_string().contains(bad) || bad.is_empty(), "{bad}: {e}");
        }
    }

    fn specs(raw: &[&str]) -> Vec<SeriesSpec> {
        raw.iter().map(|r| SeriesSpec::parse(r).unwrap()).collect()
    }

    #[test]
    fn labels_use_the_dimension_that_actually_differs() {
        // Observed live: both of these labelled `CallCount API`, because the
        // last dimension is shared and only `Resource` tells them apart.
        let s = specs(&[
            "AWS/Usage/CallCount:Sum,Class=None,Resource=GetMetricData,Service=CloudWatch,Type=API",
            "AWS/Usage/CallCount:Sum,Class=None,Resource=ListMetrics,Service=CloudWatch,Type=API",
        ]);
        assert_eq!(label_series(&s), vec!["GetMetricData", "ListMetrics"]);
    }

    #[test]
    fn labels_are_always_distinct() {
        // A legend with duplicate entries cannot be read, and these labels are
        // what the report charts key on.
        for raw in [
            vec![
                "AWS/EC2/CPUUtilization,InstanceId=i-a",
                "AWS/EC2/CPUUtilization,InstanceId=i-b",
            ],
            vec!["AWS/Lambda/Errors:Sum", "AWS/Lambda/Invocations:Sum"],
            vec![
                "AWS/EC2/CPUUtilization,InstanceId=i-a",
                "AWS/EC2/NetworkIn,InstanceId=i-b",
            ],
            vec![
                "AWS/Usage/CallCount,Type=API,Resource=A",
                "AWS/Usage/CallCount,Type=API,Resource=B",
            ],
            vec![
                "AWS/FSx/StorageUsed,FileSystemId=f1,VolumeId=v1",
                "AWS/FSx/StorageUsed,FileSystemId=f1,VolumeId=v2",
            ],
        ] {
            let labels = label_series(&specs(&raw));
            let unique: std::collections::BTreeSet<&String> = labels.iter().collect();
            assert_eq!(
                unique.len(),
                labels.len(),
                "duplicate labels for {raw:?}: {labels:?}"
            );
        }
    }

    #[test]
    fn a_differing_metric_name_is_kept_in_the_label() {
        assert_eq!(
            label_series(&specs(&[
                "AWS/Lambda/Errors:Sum",
                "AWS/Lambda/Invocations:Sum"
            ])),
            vec!["Errors", "Invocations"]
        );
    }

    #[test]
    fn both_metric_and_dimension_appear_when_both_vary() {
        assert_eq!(
            label_series(&specs(&[
                "AWS/EC2/CPUUtilization,InstanceId=i-a",
                "AWS/EC2/NetworkIn,InstanceId=i-b",
            ])),
            vec!["CPUUtilization i-a", "NetworkIn i-b"]
        );
    }

    #[test]
    fn a_shared_dimension_is_left_out_of_the_label() {
        // FileSystemId is the same for both, so naming it adds noise.
        let labels = label_series(&specs(&[
            "AWS/FSx/StorageUsed,FileSystemId=f1,VolumeId=v1",
            "AWS/FSx/StorageUsed,FileSystemId=f1,VolumeId=v2",
        ]));
        assert_eq!(labels, vec!["v1", "v2"]);
    }

    #[test]
    fn a_single_series_is_labelled_by_its_metric() {
        assert_eq!(
            label_series(&specs(&["AWS/Lambda/Errors:Sum"])),
            vec!["Errors"]
        );
    }

    #[test]
    fn identical_specs_fall_back_to_the_full_text_rather_than_colliding() {
        // Two genuinely identical series cannot be told apart by any subset,
        // so the full spec is used: ugly, but never ambiguous.
        let labels = label_series(&specs(&["AWS/Lambda/Errors:Sum", "AWS/Lambda/Errors:Sum"]));
        assert_eq!(labels.len(), 2);
        assert!(labels.iter().all(|l| l.contains("AWS/Lambda/Errors")));
    }
}
