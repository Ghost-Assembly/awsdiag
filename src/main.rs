//! `awsdiag` — AWS diagnostic data acquisition.

// Shipped code contains no `unsafe`. The exemption is for tests only:
// Rust 2024 made `std::env::set_var` an unsafe fn, and several tests must set
// AWS_* variables to exercise the credential and profile paths. Scoping the
// lint with `not(test)` keeps the guarantee where it matters instead of
// weakening it to `deny` everywhere.
#![cfg_attr(not(test), forbid(unsafe_code))]

use awsdiag::cmd;
use awsdiag::common::envelope::Envelope;
use awsdiag::common::envelope::ErrorEnvelope;
use awsdiag::common::errors::Error;
use awsdiag::common::flags::{OutputArgs, OutputFormat, TargetArgs, WindowArgs};
use awsdiag::common::progress::Progress;
use awsdiag::output;
use clap::{Parser, Subcommand};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "awsdiag",
    version,
    about = "Fast, compact AWS diagnostic data collection, built to be driven \
             by an AI agent and rendered into a single self-contained HTML \
             report",
    long_about = "Acquires and shapes AWS diagnostic data for analysis.\n\n\
                  Every subcommand emits the same JSON envelope:\n  \
                  {ok, command, params, count, truncated, next, data}\n\n\
                  `truncated` means the result is incomplete — narrow the \
                  window or raise --limit rather than treating it as final.\n\
                  On failure the envelope has ok=false and a `hint` field \
                  naming the command that fixes it."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect or clear the on-disk credential cache.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Render a findings document into one self-contained HTML file.
    Report {
        /// Path to the findings JSON document.
        #[arg(long, value_name = "FILE", conflicts_with = "schema")]
        data: Option<std::path::PathBuf>,
        /// Where to write the HTML. Defaults to report.html.
        #[arg(long, value_name = "FILE", default_value = "report.html")]
        out: std::path::PathBuf,
        /// Print a worked example of the expected document and exit.
        #[arg(long)]
        schema: bool,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// EC2 instance inventory and health.
    Ec2 {
        #[command(subcommand)]
        action: Ec2Action,
    },
    /// CloudWatch metrics on a shared timeline.
    Metrics {
        #[command(subcommand)]
        action: MetricsAction,
    },
    /// CloudWatch Logs: discover groups, scan with clustering, drill down.
    Logs {
        #[command(subcommand)]
        action: LogsAction,
    },
    /// Resolve caller identity for the selected profile(s).
    Whoami {
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        out: OutputArgs,
    },
}

#[derive(Subcommand)]
enum Ec2Action {
    /// List instances, filtered server-side.
    Ls {
        /// Instance state, e.g. running, stopped.
        #[arg(long, value_name = "STATE")]
        state: Option<String>,
        /// Name tag, `*` and `?` wildcards accepted.
        #[arg(long, value_name = "GLOB")]
        name: Option<String>,
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Full detail for named instances: volumes, security groups, tags.
    Show {
        /// Instance ids to show.
        #[arg(long = "instance", value_name = "ID")]
        instances: Vec<String>,
        /// Name tag, `*` and `?` wildcards accepted.
        #[arg(long, value_name = "GLOB")]
        name: Option<String>,
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Status checks and scheduled events, worst first.
    Health {
        /// Restrict to these instance ids; all instances when omitted.
        #[arg(long = "instance", value_name = "ID")]
        instances: Vec<String>,
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        out: OutputArgs,
    },
}

#[derive(Subcommand)]
enum MetricsAction {
    /// Rank resources by a metric without needing their ids.
    ///
    /// Dimension values are discovered from CloudWatch, so you ask "which
    /// instances are hottest" rather than supplying a list you had to find
    /// first.
    Top {
        /// Metric namespace, e.g. AWS/EC2.
        #[arg(long, value_name = "NS")]
        namespace: String,
        /// Metric name, e.g. CPUUtilization.
        #[arg(long, value_name = "NAME")]
        metric: String,
        /// Dimension to rank across, e.g. InstanceId.
        #[arg(long, value_name = "NAME")]
        dimension: String,
        #[arg(long, default_value = "Average", value_name = "STAT")]
        stat: String,
        /// Statistic to rank by.
        #[arg(long = "by", value_enum, default_value = "max")]
        rank_by: awsdiag::metrics::align::Rank,
        /// How many to show.
        #[arg(long, default_value_t = 10, value_name = "N")]
        count: usize,
        #[arg(long, value_name = "SECONDS")]
        period: Option<i64>,
        #[command(flatten)]
        window: WindowArgs,
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Fetch several series onto one aligned timeline.
    ///
    /// Repeat --series to compare resources. Every row is one instant with
    /// one column per series, so reading across a row shows what every
    /// resource was doing at that moment.
    Compare {
        /// Namespace/MetricName[:Stat][,Dim=Value...], repeatable.
        #[arg(long = "series", value_name = "SPEC", required = true)]
        series: Vec<String>,
        /// Resolution in seconds. Chosen automatically when omitted, which
        /// also avoids asking for data finer than CloudWatch retains.
        #[arg(long, value_name = "SECONDS")]
        period: Option<i64>,
        #[command(flatten)]
        window: WindowArgs,
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        out: OutputArgs,
    },
}

#[derive(Subcommand)]
enum LogsAction {
    /// List log groups, optionally filtered by prefix or glob.
    Groups {
        /// Name prefix, or a glob such as '/aws/lambda/*'.
        #[arg(long, value_name = "PATTERN")]
        pattern: Option<String>,
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Scan a log group and return clustered templates rather than raw lines.
    Scan {
        /// Log group name, or a glob such as '/aws/lambda/*'.
        #[arg(long, value_name = "NAME")]
        group: String,
        /// CloudWatch filter pattern applied server-side, e.g. 'ERROR'.
        #[arg(long, value_name = "PATTERN")]
        filter: Option<String>,
        /// Compare against the period of this length immediately before the
        /// window, marking each cluster new, spiking or steady.
        #[arg(long, value_name = "DURATION")]
        baseline: Option<String>,
        /// Return raw events instead of clusters.
        #[arg(long)]
        no_cluster: bool,
        /// Cap on log groups a glob may fan out to.
        #[arg(long, default_value_t = awsdiag::cmd::logs::DEFAULT_MAX_GROUPS, value_name = "N")]
        max_groups: usize,
        #[command(flatten)]
        window: WindowArgs,
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Return the verbatim lines behind one cluster id.
    Drill {
        /// Cluster id from a previous `logs scan`.
        #[arg(long, value_name = "ID")]
        cluster: String,
        /// The same log group the scan used.
        #[arg(long, value_name = "NAME")]
        group: String,
        /// Narrow to a single log stream.
        #[arg(long, value_name = "NAME")]
        stream: Option<String>,
        #[arg(long, value_name = "PATTERN")]
        filter: Option<String>,
        /// Cap on log groups a glob may fan out to. Match the scan's value,
        /// or a glob scan's cluster is looked for in fewer groups than it
        /// came from.
        #[arg(long, default_value_t = awsdiag::cmd::logs::DEFAULT_MAX_GROUPS, value_name = "N")]
        max_groups: usize,
        #[command(flatten)]
        window: WindowArgs,
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Print recent raw events, newest last.
    Tail {
        /// Log group name, or a glob such as '/aws/lambda/*'.
        #[arg(long, value_name = "NAME")]
        group: String,
        #[arg(long, value_name = "PATTERN")]
        filter: Option<String>,
        /// Cap on log groups a glob may fan out to.
        #[arg(long, default_value_t = awsdiag::cmd::logs::DEFAULT_MAX_GROUPS, value_name = "N")]
        max_groups: usize,
        #[command(flatten)]
        window: WindowArgs,
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        out: OutputArgs,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Show cached entries and when they expire.
    Status {
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Remove every cached entry.
    Clear {
        #[command(flatten)]
        out: OutputArgs,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match &cli.command {
        Command::Cache { action } => match action {
            CacheAction::Status { out } => {
                let progress = Progress::new(out.progress);
                let result = cmd::cache::status(chrono::Utc::now(), &progress)
                    .and_then(|e| output::render(&e, out.output));
                finish("cache status", result, out.output, &progress)
            }
            CacheAction::Clear { out } => {
                let progress = Progress::new(out.progress);
                let result =
                    cmd::cache::clear(&progress).and_then(|e| output::render(&e, out.output));
                finish("cache clear", result, out.output, &progress)
            }
        },
        Command::Report {
            data,
            out,
            schema,
            output,
        } => {
            use awsdiag::cmd::report;
            if *schema {
                println!("{}", report::EXAMPLE);
                return std::process::ExitCode::SUCCESS;
            }
            let Some(path) = data else {
                let e = awsdiag::common::errors::Error::BadFindings {
                    source_name: "input".into(),
                    detail: "give --data <FILE>, or --schema to see the expected shape".into(),
                };
                return fail("report", &e, output.output);
            };
            let progress = Progress::new(output.progress);
            let result = (|| {
                let findings = report::read_findings(path)?;
                let summary = report::write_report(&findings, out, &progress)?;
                let env = Envelope::new("report", json!({}), vec![summary]);
                output::render(&env, output.output)
            })();
            finish("report", result, output.output, &progress)
        }
        Command::Ec2 { action } => run_ec2(action).await,
        Command::Metrics { action } => run_metrics(action).await,
        Command::Logs { action } => run_logs(action).await,
        Command::Whoami { target, out } => {
            let progress = Progress::new(out.progress);
            let result = cmd::whoami::run(target, &progress)
                .await
                .and_then(|env| output::render(&env, out.output));
            finish("whoami", result, out.output, &progress)
        }
    }
}

async fn run_ec2(action: &Ec2Action) -> std::process::ExitCode {
    use awsdiag::cmd::ec2;
    match action {
        Ec2Action::Ls {
            state,
            name,
            target,
            out,
        } => {
            let progress = Progress::new(out.progress);
            let result = ec2::ls(
                target,
                state.as_deref(),
                name.as_deref(),
                out.limit,
                &progress,
            )
            .await
            .and_then(|env| output::render(&env, out.output));
            finish("ec2 ls", result, out.output, &progress)
        }
        Ec2Action::Show {
            instances,
            name,
            target,
            out,
        } => {
            let progress = Progress::new(out.progress);
            let result = ec2::show(target, instances, name.as_deref(), &progress)
                .await
                .and_then(|env| output::render(&env, out.output));
            finish("ec2 show", result, out.output, &progress)
        }
        Ec2Action::Health {
            instances,
            target,
            out,
        } => {
            let progress = Progress::new(out.progress);
            let result = ec2::health(target, instances, out.limit, &progress)
                .await
                .and_then(|env| output::render(&env, out.output));
            finish("ec2 health", result, out.output, &progress)
        }
    }
}

async fn run_metrics(action: &MetricsAction) -> std::process::ExitCode {
    use awsdiag::cmd::metrics;
    let now = chrono::Utc::now();
    match action {
        MetricsAction::Compare {
            series,
            period,
            window,
            target,
            out,
        } => {
            let progress = Progress::new(out.progress);
            let result = async {
                let w = window.resolve(now)?;
                let specs = series
                    .iter()
                    .map(|s| metrics::SeriesSpec::parse(s))
                    .collect::<Result<Vec<_>, _>>()?;
                let env = metrics::compare(target, &specs, w, *period, &progress).await?;
                output::render(&env, out.output)
            }
            .await;
            finish("metrics compare", result, out.output, &progress)
        }
        MetricsAction::Top {
            namespace,
            metric,
            dimension,
            stat,
            rank_by,
            count,
            period,
            window,
            target,
            out,
        } => {
            let progress = Progress::new(out.progress);
            let result = async {
                let w = window.resolve(now)?;
                let req = metrics::TopRequest {
                    namespace,
                    metric,
                    dimension,
                    stat,
                    by: *rank_by,
                    count: *count,
                    window: w,
                    period: *period,
                };
                let env = metrics::top(target, &req, &progress).await?;
                output::render(&env, out.output)
            }
            .await;
            finish("metrics top", result, out.output, &progress)
        }
    }
}

async fn run_logs(action: &LogsAction) -> std::process::ExitCode {
    use awsdiag::cmd::logs;
    let now = chrono::Utc::now();

    match action {
        LogsAction::Groups {
            pattern,
            target,
            out,
        } => {
            let progress = Progress::new(out.progress);
            let result = logs::groups(target, pattern.as_deref(), out.limit, &progress)
                .await
                .and_then(|env| output::render(&env, out.output));
            finish("logs groups", result, out.output, &progress)
        }

        LogsAction::Scan {
            group,
            filter,
            baseline,
            no_cluster,
            max_groups,
            window,
            target,
            out,
        } => {
            let progress = Progress::new(out.progress);
            let result = async {
                let w = window.resolve(now)?;
                let req = logs::ScanRequest {
                    group,
                    window: w,
                    filter: filter.as_deref(),
                    limit: out.limit,
                    max_groups: *max_groups,
                };
                let (scan, profile) = logs::fetch(target, &req, &progress).await?;
                let params = json!({
                    "profile": profile,
                    "group": group,
                    "groups_scanned": scan.groups_scanned,
                    "since": w.since.to_rfc3339(),
                    "until": w.until.to_rfc3339(),
                    "filter": filter,
                    "baseline": baseline,
                    "coverage": scan.coverage_json(w),
                });

                if *no_cluster {
                    let env =
                        Envelope::new("logs scan", params, scan.events).truncated(scan.truncated);
                    return output::render(&env, out.output);
                }

                // The baseline is the equally long period immediately before
                // the window: "what is new since the same stretch before this".
                let base = baseline
                    .as_deref()
                    .map(|b| {
                        let start = awsdiag::common::time::parse_instant(b, w.since)?;
                        Ok::<_, awsdiag::common::errors::Error>(awsdiag::common::flags::Window {
                            since: start,
                            until: w.since,
                        })
                    })
                    .transpose()?;

                let (clusters, baseline_truncated) =
                    logs::cluster_events(target, &req, &scan.events, base, &progress).await?;
                // A capped baseline undercounts, which makes steady clusters
                // read as new or spiking; that is as incomplete as a capped
                // window and is reported the same way.
                let env = Envelope::new("logs scan", params, clusters)
                    .truncated(scan.truncated || baseline_truncated);
                output::render(&env, out.output)
            }
            .await;
            finish("logs scan", result, out.output, &progress)
        }

        LogsAction::Drill {
            cluster,
            group,
            stream,
            filter,
            max_groups,
            window,
            target,
            out,
        } => {
            let progress = Progress::new(out.progress);
            let result = async {
                let w = window.resolve(now)?;
                let req = logs::ScanRequest {
                    group,
                    window: w,
                    filter: filter.as_deref(),
                    limit: None,
                    max_groups: *max_groups,
                };
                let (scan, profile) = logs::fetch(target, &req, &progress).await?;
                let picked = logs::select_cluster(&scan.events, cluster, stream.as_deref());
                let limit = out.limit.unwrap_or(usize::MAX);
                let truncated = picked.len() > limit;
                let rows: Vec<_> = picked.into_iter().take(limit).collect();
                let env = Envelope::new(
                    "logs drill",
                    json!({
                        "profile": profile, "group": group, "cluster": cluster,
                        "stream": stream, "since": w.since.to_rfc3339(),
                        "until": w.until.to_rfc3339(),
                    }),
                    rows,
                )
                .truncated(truncated || scan.truncated);
                output::render(&env, out.output)
            }
            .await;
            finish("logs drill", result, out.output, &progress)
        }

        LogsAction::Tail {
            group,
            filter,
            max_groups,
            window,
            target,
            out,
        } => {
            let progress = Progress::new(out.progress);
            let result = async {
                let w = window.resolve(now)?;
                let req = logs::ScanRequest {
                    group,
                    window: w,
                    filter: filter.as_deref(),
                    limit: out.limit,
                    max_groups: *max_groups,
                };
                let (scan, profile) = logs::fetch(target, &req, &progress).await?;
                let env = Envelope::new(
                    "logs tail",
                    json!({ "profile": profile, "group": group,
                            "since": w.since.to_rfc3339(), "until": w.until.to_rfc3339(),
                            "coverage": scan.coverage_json(w) }),
                    scan.events,
                )
                .truncated(scan.truncated);
                output::render(&env, out.output)
            }
            .await;
            finish("logs tail", result, out.output, &progress)
        }
    }
}

/// Print a rendered result, or fail with whatever stopped it -- including a
/// render error. Progress is cleared on every failure, so a spinner is never
/// left frozen above the error.
fn finish(
    command: &str,
    result: Result<String, Error>,
    format: OutputFormat,
    progress: &Progress,
) -> std::process::ExitCode {
    match result {
        Ok(text) => {
            println!("{text}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            progress.abandon();
            fail(command, &e, format)
        }
    }
}

/// Emit the error envelope on stdout and a human summary on stderr.
///
/// For the machine formats stdout stays parseable in every case, so a caller
/// can pipe to `jq` unconditionally rather than branching on the exit code
/// first: `json` gets the envelope, `ndjson` gets it as a single line so a
/// line-oriented reader still sees one object per line. `text` is for a
/// person, who reads stderr, so stdout stays empty rather than repeating the
/// same line twice.
fn fail(command: &str, err: &Error, format: OutputFormat) -> std::process::ExitCode {
    let envelope = ErrorEnvelope::new(command, err);
    let rendered = match format {
        OutputFormat::Json => serde_json::to_string_pretty(&envelope),
        OutputFormat::Ndjson => serde_json::to_string(&envelope),
        OutputFormat::Text => Ok(String::new()),
    };
    // The envelope is plain strings and cannot fail to serialize; should it
    // ever, the stderr lines below still say what went wrong.
    if let Ok(text) = rendered
        && !text.is_empty()
    {
        println!("{text}");
    }
    eprintln!("error: {err}");
    if let Some(h) = err.hint() {
        eprintln!("hint: {h}");
    }
    std::process::ExitCode::FAILURE
}
