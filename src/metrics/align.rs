//! Putting several metric series onto one shared timeline.
//!
//! "What was happening on all of these at 10:04" is the question metrics get
//! asked during an incident, and answering it means every series sharing one
//! time axis. CloudWatch does not provide that: each query returns its own
//! timestamps, a series is missing timestamps where it had no data, and two
//! series over the same window can disagree about which instants exist.
//!
//! Aligning once here, in one place, is the difference between a caller
//! comparing series and a caller re-deriving the timeline every time.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::BTreeMap;

/// One metric series as CloudWatch returns it: paired timestamps and values,
/// with gaps wherever no data existed.
#[derive(Debug, Clone)]
pub struct Series {
    pub label: String,
    pub points: Vec<(DateTime<Utc>, f64)>,
}

/// A dense matrix: one row per instant on the grid, one column per series.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Row {
    pub ts: DateTime<Utc>,
    /// One entry per series, in the order the series were supplied. `None`
    /// where that series had no datapoint at this instant — a real gap, not a
    /// zero. Reporting a gap as `0` would turn "the instance was not
    /// reporting" into "the value was zero", which reads as healthy.
    pub values: Vec<Option<f64>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Matrix {
    pub labels: Vec<String>,
    pub period_seconds: i64,
    pub rows: Vec<Row>,
}

/// CloudWatch's retention tiers: a period finer than the tier for a window's
/// age returns nothing at all.
///
/// Getting this wrong is the classic CloudWatch trap — a query for 1-minute
/// data over the last 30 days succeeds and returns an empty set, which is
/// indistinguishable from "the metric was flat". These bounds are asserted
/// against the live API by the integration checks rather than trusted.
const RETENTION: [(i64, i64); 3] = [
    // (period seconds, maximum age in seconds the period is retained for)
    (60, 15 * 86_400),
    (300, 63 * 86_400),
    (3_600, 455 * 86_400),
];

/// The API's cap on datapoints returned for a single request.
const MAX_DATAPOINTS: i64 = 100_800;

/// Choose the finest period that both covers the window's age and keeps the
/// result under the datapoint cap.
///
/// `age` is how far back the *start* of the window reaches from now.
pub fn choose_period(window_seconds: i64, age_seconds: i64, series: usize) -> i64 {
    let series = series.max(1) as i64;
    for (period, max_age) in RETENTION {
        if age_seconds > max_age {
            continue;
        }
        // A period that would blow the datapoint cap returns an error, so
        // step coarser rather than fail.
        if (window_seconds / period) * series <= MAX_DATAPOINTS {
            return period;
        }
    }
    // Beyond the coarsest tier there is nothing finer to fall back to.
    let coarsest = RETENTION[RETENTION.len() - 1].0;
    let needed = (window_seconds * series).div_euclid(MAX_DATAPOINTS).max(1);
    // Round up to a whole minute: CloudWatch requires a multiple of 60 above
    // one minute, and rejects anything else.
    let rounded = (needed + 59) / 60 * 60;
    coarsest.max(rounded)
}

/// A series reduced to the numbers you rank and scan by.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Summary {
    pub label: String,
    pub datapoints: usize,
    pub min: Option<f64>,
    pub mean: Option<f64>,
    pub max: Option<f64>,
    /// The most recent value, which answers "is it still happening".
    pub latest: Option<f64>,
}

/// How to rank series against each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum Rank {
    /// Highest peak. Finds the box that spiked.
    #[default]
    Max,
    /// Highest sustained level. Finds the box that is always busy.
    Mean,
    /// Highest current value. Finds what is wrong *now*.
    Latest,
}

/// Reduce a series to its summary.
///
/// A series with no datapoints yields `None` for every statistic rather than
/// zero: "not reporting" and "reported zero" are different findings, and only
/// one of them is good news.
pub fn summarise(series: &Series) -> Summary {
    let mut points = series.points.clone();
    points.sort_by_key(|(t, _)| *t);
    let values: Vec<f64> = points
        .iter()
        .map(|(_, v)| *v)
        .filter(|v| v.is_finite())
        .collect();

    Summary {
        label: series.label.clone(),
        datapoints: values.len(),
        min: values.iter().copied().reduce(f64::min),
        mean: (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64),
        max: values.iter().copied().reduce(f64::max),
        latest: points.last().map(|(_, v)| *v).filter(|v| v.is_finite()),
    }
}

/// Rank summaries highest-first by the chosen statistic.
///
/// Series with no data sort last regardless. They are not "zero" and must not
/// displace a series that actually reported something.
pub fn rank(mut summaries: Vec<Summary>, by: Rank) -> Vec<Summary> {
    let key = move |s: &Summary| match by {
        Rank::Max => s.max,
        Rank::Mean => s.mean,
        Rank::Latest => s.latest,
    };
    summaries.sort_by(|a, b| {
        match (key(a), key(b)) {
            (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            // Deterministic tiebreak, so equal values do not reorder run to run.
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then(a.label.cmp(&b.label))
    });
    summaries
}

/// The interval a series actually reports at, inferred from its datapoints.
///
/// A metric's publishing interval is not discoverable up front: EC2 basic
/// monitoring emits every 5 minutes, detailed monitoring every minute, and a
/// custom metric whatever its publisher chose. Asking for a finer period than
/// the metric supports is not an error — it returns a mostly-empty matrix,
/// which looks like an outage rather than a resolution mismatch.
///
/// The smallest gap between consecutive points is the best available estimate.
/// Returns `None` for a series with fewer than two points, where there is
/// nothing to infer from.
pub fn native_period(series: &[Series]) -> Option<i64> {
    use std::collections::HashMap;

    let mut gaps: HashMap<i64, usize> = HashMap::new();
    let mut total = 0usize;
    for s in series {
        let mut times: Vec<i64> = s.points.iter().map(|(t, _)| t.timestamp()).collect();
        times.sort_unstable();
        for pair in times.windows(2) {
            let gap = pair[1] - pair[0];
            if gap > 0 {
                *gaps.entry(gap).or_default() += 1;
                total += 1;
            }
        }
    }

    // Too few gaps is not evidence. Two datapoints an hour apart say nothing
    // about a metric's publishing interval.
    if total < MIN_GAPS_TO_INFER {
        return None;
    }

    // The *most common* gap, not the smallest. A metric that publishes only
    // when non-zero -- Lambda `Errors`, ALB 5xx counts -- has large gaps
    // between sparse events, and taking the minimum of an otherwise regular
    // series lets one close pair suppress the finding entirely.
    gaps.into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(gap, _)| gap)
}

/// Below this many intervals there is not enough evidence to infer anything.
const MIN_GAPS_TO_INFER: usize = 3;

/// Snap an instant down to the start of its period bucket.
fn bucket(ts: DateTime<Utc>, period: i64) -> DateTime<Utc> {
    let secs = ts.timestamp();
    let floored = secs - secs.rem_euclid(period);
    DateTime::from_timestamp(floored, 0).unwrap_or(ts)
}

/// Build a dense matrix over `[since, until)` at `period` resolution.
///
/// Every instant on the grid appears exactly once, in order, whether or not
/// any series had data there — a chart with an implicit gap misleads, and a
/// caller cannot tell a missing row from a missing series.
pub fn align(series: &[Series], since: DateTime<Utc>, until: DateTime<Utc>, period: i64) -> Matrix {
    let period = period.max(1);
    let labels: Vec<String> = series.iter().map(|s| s.label.clone()).collect();

    // Index each series by bucket. Later points in the same bucket overwrite
    // earlier ones, which cannot happen when CloudWatch returns one point per
    // period, and is the least surprising resolution if it ever does.
    let indexed: Vec<BTreeMap<DateTime<Utc>, f64>> = series
        .iter()
        .map(|s| {
            s.points
                .iter()
                .map(|(ts, v)| (bucket(*ts, period), *v))
                .collect()
        })
        .collect();

    let start = bucket(since, period);
    let step = Duration::seconds(period);
    let mut rows = Vec::new();
    let mut ts = start;
    while ts < until {
        rows.push(Row {
            ts,
            values: indexed.iter().map(|m| m.get(&ts).copied()).collect(),
        });
        ts += step;
    }

    Matrix {
        labels,
        period_seconds: period,
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 4, 10, m, 0).unwrap()
    }

    fn series(label: &str, points: &[(u32, f64)]) -> Series {
        Series {
            label: label.into(),
            points: points.iter().map(|(m, v)| (t(*m), *v)).collect(),
        }
    }

    #[test]
    fn two_series_share_one_timeline() {
        // The whole point: read down a row to see every series at one instant.
        let m = align(
            &[
                series("a", &[(0, 1.0), (1, 2.0)]),
                series("b", &[(0, 10.0), (1, 20.0)]),
            ],
            t(0),
            t(2),
            60,
        );
        assert_eq!(m.labels, vec!["a", "b"]);
        assert_eq!(m.rows.len(), 2);
        assert_eq!(
            m.rows[0],
            Row {
                ts: t(0),
                values: vec![Some(1.0), Some(10.0)]
            }
        );
        assert_eq!(
            m.rows[1],
            Row {
                ts: t(1),
                values: vec![Some(2.0), Some(20.0)]
            }
        );
    }

    #[test]
    fn a_gap_is_none_and_never_zero() {
        // Reporting a gap as 0 turns "not reporting" into "value was zero",
        // which reads as healthy -- the opposite of the truth during an
        // instance outage.
        let m = align(&[series("a", &[(0, 5.0), (2, 7.0)])], t(0), t(3), 60);
        assert_eq!(m.rows[0].values, vec![Some(5.0)]);
        assert_eq!(m.rows[1].values, vec![None], "no datapoint, not a zero");
        assert_eq!(m.rows[2].values, vec![Some(7.0)]);
    }

    #[test]
    fn series_that_disagree_about_which_instants_exist_are_reconciled() {
        // CloudWatch omits timestamps a series has no data for, so two series
        // over the same window routinely have different lengths.
        let m = align(
            &[
                series("a", &[(0, 1.0), (1, 2.0), (2, 3.0)]),
                series("b", &[(1, 20.0)]),
            ],
            t(0),
            t(3),
            60,
        );
        assert_eq!(m.rows.len(), 3);
        assert_eq!(m.rows[0].values, vec![Some(1.0), None]);
        assert_eq!(m.rows[1].values, vec![Some(2.0), Some(20.0)]);
        assert_eq!(m.rows[2].values, vec![Some(3.0), None]);
    }

    #[test]
    fn every_row_has_one_value_per_series() {
        let m = align(
            &[
                series("a", &[(0, 1.0)]),
                series("b", &[]),
                series("c", &[(2, 3.0)]),
            ],
            t(0),
            t(3),
            60,
        );
        assert!(
            m.rows.iter().all(|r| r.values.len() == 3),
            "ragged rows break every consumer"
        );
    }

    #[test]
    fn timestamps_are_ordered_and_evenly_spaced() {
        let m = align(&[series("a", &[])], t(0), t(5), 60);
        let times: Vec<DateTime<Utc>> = m.rows.iter().map(|r| r.ts).collect();
        assert_eq!(times, vec![t(0), t(1), t(2), t(3), t(4)]);
    }

    #[test]
    fn the_window_end_is_exclusive() {
        // An inclusive end would emit a trailing row that is always empty,
        // because CloudWatch labels a bucket by its start.
        let m = align(&[series("a", &[])], t(0), t(3), 60);
        assert_eq!(m.rows.len(), 3);
        assert_eq!(m.rows.last().unwrap().ts, t(2));
    }

    #[test]
    fn points_are_snapped_to_their_bucket() {
        // A datapoint at 10:00:37 belongs to the 10:00 bucket at 60s.
        let odd = Utc.with_ymd_and_hms(2026, 9, 4, 10, 0, 37).unwrap();
        let m = align(
            &[Series {
                label: "a".into(),
                points: vec![(odd, 9.0)],
            }],
            t(0),
            t(2),
            60,
        );
        assert_eq!(m.rows[0].values, vec![Some(9.0)]);
    }

    #[test]
    fn no_series_still_yields_the_grid() {
        let m = align(&[], t(0), t(3), 60);
        assert_eq!(m.rows.len(), 3);
        assert!(m.rows.iter().all(|r| r.values.is_empty()));
        assert!(m.labels.is_empty());
    }

    #[test]
    fn an_inverted_or_empty_window_yields_no_rows() {
        assert!(align(&[series("a", &[])], t(3), t(0), 60).rows.is_empty());
        assert!(align(&[series("a", &[])], t(0), t(0), 60).rows.is_empty());
    }

    // ---- summarising and ranking -----------------------------------------

    #[test]
    fn a_summary_reports_the_shape_of_a_series() {
        let s = summarise(&series("a", &[(0, 5.0), (1, 1.0), (2, 9.0), (3, 3.0)]));
        assert_eq!(s.datapoints, 4);
        assert_eq!(s.min, Some(1.0));
        assert_eq!(s.max, Some(9.0));
        assert_eq!(s.mean, Some(4.5));
        assert_eq!(
            s.latest,
            Some(3.0),
            "latest is by time, not by position in the input"
        );
    }

    #[test]
    fn latest_follows_time_order_not_input_order() {
        // CloudWatch does not guarantee ordering, and "is it still happening"
        // is wrong if `latest` is whatever arrived last.
        let jumbled = Series {
            label: "a".into(),
            points: vec![(t(2), 9.0), (t(0), 5.0), (t(1), 1.0)],
        };
        assert_eq!(summarise(&jumbled).latest, Some(9.0));
    }

    #[test]
    fn an_empty_series_summarises_to_nothing_not_to_zero() {
        // "Not reporting" and "reported zero" are different findings, and
        // only one of them is good news.
        let s = summarise(&series("a", &[]));
        assert_eq!(s.datapoints, 0);
        assert_eq!((s.min, s.mean, s.max, s.latest), (None, None, None, None));
    }

    #[test]
    fn ranking_orders_highest_first_by_the_chosen_statistic() {
        // Chosen so the three statistics genuinely disagree: a peaks higher,
        // b is busier on average, a is higher right now.
        let a = summarise(&series("a", &[(0, 1.0), (1, 100.0)])); // max 100, mean 50.5, latest 100
        let b = summarise(&series("b", &[(0, 60.0), (1, 60.0)])); // max  60, mean 60.0, latest  60
        let order = |by| {
            rank(vec![a.clone(), b.clone()], by)
                .into_iter()
                .map(|s| s.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            order(Rank::Max),
            vec!["a", "b"],
            "the spike has the higher peak"
        );
        assert_eq!(
            order(Rank::Mean),
            vec!["b", "a"],
            "the sustained one is busier overall"
        );
        assert_eq!(order(Rank::Latest), vec!["a", "b"], "a is higher right now");
    }

    #[test]
    fn series_with_no_data_sort_last_whatever_the_statistic() {
        // An absent series is not a quiet one, and must not displace a
        // resource that actually reported.
        let quiet = summarise(&series("quiet", &[(0, 0.1)]));
        let absent = summarise(&series("absent", &[]));
        for by in [Rank::Max, Rank::Mean, Rank::Latest] {
            let order: Vec<String> = rank(vec![absent.clone(), quiet.clone()], by)
                .into_iter()
                .map(|s| s.label)
                .collect();
            assert_eq!(order, vec!["quiet", "absent"], "{by:?}");
        }
    }

    #[test]
    fn equal_values_rank_deterministically() {
        let a = summarise(&series("bbb", &[(0, 7.0)]));
        let b = summarise(&series("aaa", &[(0, 7.0)]));
        let order: Vec<String> = rank(vec![a, b], Rank::Max)
            .into_iter()
            .map(|s| s.label)
            .collect();
        assert_eq!(
            order,
            vec!["aaa", "bbb"],
            "ties break on label, not on input order"
        );
    }

    #[test]
    fn non_finite_values_are_excluded_rather_than_poisoning_the_statistics() {
        // A NaN in the input makes every comparison false and would scramble
        // the ordering silently.
        let poisoned = Series {
            label: "a".into(),
            points: vec![(t(0), 1.0), (t(1), f64::NAN), (t(2), 3.0)],
        };
        let s = summarise(&poisoned);
        assert_eq!(s.datapoints, 2);
        assert_eq!(s.max, Some(3.0));
        assert_eq!(s.mean, Some(2.0));
    }

    // ---- period selection -------------------------------------------------

    #[test]
    fn the_native_period_is_inferred_from_the_gaps_between_points() {
        // EC2 basic monitoring publishes every 5 minutes. Asking for 60s
        // resolution then yields a matrix that is 80% empty, which reads as
        // an outage rather than a resolution mismatch.
        let five_min = series("a", &[(0, 1.0), (5, 2.0), (10, 3.0), (15, 4.0)]);
        assert_eq!(native_period(&[five_min]), Some(300));
    }

    #[test]
    fn an_event_driven_metric_does_not_report_a_bogus_period() {
        // Lambda `Errors` and ALB 5xx counts publish only when non-zero. Two
        // errors an hour apart in an otherwise 60s metric must not advise
        // collapsing it to hourly buckets -- that throws the metric away.
        let sparse = series("Errors", &[(0, 1.0), (55, 1.0)]);
        assert_eq!(
            native_period(&[sparse]),
            None,
            "two points are not evidence"
        );
    }

    #[test]
    fn one_close_pair_does_not_suppress_the_finding() {
        // Taking the minimum let a single 60s-apart pair in an otherwise
        // 5-minute series hide the mismatch for every other point.
        let mostly_five = Series {
            label: "a".into(),
            points: [0i64, 300, 600, 900, 960]
                .iter()
                .map(|s| (Utc.timestamp_opt(t(0).timestamp() + s, 0).unwrap(), 1.0))
                .collect(),
        };
        assert_eq!(
            native_period(&[mostly_five]),
            Some(300),
            "the modal gap wins"
        );
    }

    #[test]
    fn a_series_too_short_to_infer_from_yields_nothing() {
        assert_eq!(native_period(&[series("a", &[(0, 1.0)])]), None);
        assert_eq!(native_period(&[series("a", &[])]), None);
        assert_eq!(native_period(&[]), None);
        // Three points give two gaps, still below the evidence threshold.
        assert_eq!(
            native_period(&[series("a", &[(0, 1.0), (5, 1.0), (10, 1.0)])]),
            None
        );
    }

    #[test]
    fn a_regular_series_reports_its_interval_regardless_of_odd_gaps() {
        // One missing datapoint creates a double-width gap; the modal value
        // is still the true interval.
        let with_hole = series("a", &[(0, 1.0), (5, 1.0), (15, 1.0), (20, 1.0), (25, 1.0)]);
        assert_eq!(native_period(&[with_hole]), Some(300));
    }

    #[test]
    fn a_recent_window_gets_one_minute_resolution() {
        assert_eq!(choose_period(3_600, 3_600, 1), 60);
    }

    #[test]
    fn a_window_older_than_the_one_minute_tier_steps_coarser() {
        // Asking for 60s data 30 days back returns an empty set, which looks
        // exactly like a flat metric. Stepping to 300s returns real data.
        assert_eq!(choose_period(3_600, 30 * 86_400, 1), 300);
        assert_eq!(choose_period(3_600, 100 * 86_400, 1), 3_600);
    }

    #[test]
    fn a_long_window_steps_coarser_to_stay_under_the_datapoint_cap() {
        // 14 days at 60s is 20,160 points -- fine for one series, but 10
        // series exceeds the cap and the request would be rejected outright.
        assert_eq!(choose_period(14 * 86_400, 14 * 86_400, 1), 60);
        assert_eq!(choose_period(14 * 86_400, 14 * 86_400, 10), 300);
    }

    #[test]
    fn beyond_the_coarsest_tier_the_period_grows_and_stays_a_whole_minute() {
        let p = choose_period(455 * 86_400, 455 * 86_400, 50);
        assert!(p >= 3_600, "never finer than the coarsest tier: {p}");
        assert_eq!(
            p % 60,
            0,
            "CloudWatch rejects periods that are not a multiple of 60: {p}"
        );
    }

    #[test]
    fn the_chosen_period_always_keeps_the_request_under_the_cap() {
        for (window, age, series) in [
            (3_600i64, 0i64, 1usize),
            (86_400, 86_400, 5),
            (14 * 86_400, 14 * 86_400, 20),
            (60 * 86_400, 60 * 86_400, 30),
            (400 * 86_400, 400 * 86_400, 100),
        ] {
            let p = choose_period(window, age, series);
            let points = (window / p) * series as i64;
            assert!(
                points <= MAX_DATAPOINTS,
                "{points} points for {window}s x{series} at {p}s"
            );
        }
    }
}
