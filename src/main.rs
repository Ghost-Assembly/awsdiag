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
    about = "Fast, compact AWS diagnostic data acquisition",
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
        #[command(flatten)]
        window: WindowArgs,
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        out: OutputArgs,
    },
    /// Print recent raw events, newest last.
    Tail {
        #[arg(long, value_name = "NAME")]
        group: String,
        #[arg(long, value_name = "PATTERN")]
        filter: Option<String>,
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
        Command::Cache { action } => {
            let (result, format) = match action {
                CacheAction::Status { out } => {
                    let p = Progress::new(out.progress);
                    (
                        cmd::cache::status(chrono::Utc::now(), &p)
                            .map(|e| output::render(&e, out.output)),
                        out.output,
                    )
                }
                CacheAction::Clear { out } => {
                    let p = Progress::new(out.progress);
                    (
                        cmd::cache::clear(&p).map(|e| output::render(&e, out.output)),
                        out.output,
                    )
                }
            };
            match result {
                Ok(text) => {
                    println!("{text}");
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => fail("cache", &e, format),
            }
        }
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
                report::write_report(&findings, out, &progress)
            })();
            match result {
                Ok(summary) => {
                    let env = Envelope::new("report", json!({}), vec![summary]);
                    emit(output::render(&env, output.output))
                }
                Err(e) => {
                    progress.abandon();
                    fail("report", &e, output.output)
                }
            }
        }
        Command::Ec2 { action } => run_ec2(action).await,
        Command::Metrics { action } => run_metrics(action).await,
        Command::Logs { action } => run_logs(action).await,
        Command::Whoami { target, out } => {
            match cmd::whoami::run(target, &Progress::new(out.progress)).await {
                Ok(env) => {
                    println!("{}", output::render(&env, out.output));
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => fail("whoami", &e, out.output),
            }
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
            match ec2::ls(
                target,
                state.as_deref(),
                name.as_deref(),
                out.limit,
                &progress,
            )
            .await
            {
                Ok(env) => emit(output::render(&env, out.output)),
                Err(e) => {
                    progress.abandon();
                    fail("ec2 ls", &e, out.output)
                }
            }
        }
        Ec2Action::Show {
            instances,
            name,
            target,
            out,
        } => {
            let progress = Progress::new(out.progress);
            match ec2::show(target, instances, name.as_deref(), &progress).await {
                Ok(env) => emit(output::render(&env, out.output)),
                Err(e) => {
                    progress.abandon();
                    fail("ec2 show", &e, out.output)
                }
            }
        }
        Ec2Action::Health {
            instances,
            target,
            out,
        } => {
            let progress = Progress::new(out.progress);
            match ec2::health(target, instances, out.limit, &progress).await {
                Ok(env) => emit(output::render(&env, out.output)),
                Err(e) => {
                    progress.abandon();
                    fail("ec2 health", &e, out.output)
                }
            }
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
                Ok::<_, awsdiag::common::errors::Error>(output::render(&env, out.output))
            }
            .await;
            match result {
                Ok(text) => emit(text),
                Err(e) => {
                    progress.abandon();
                    fail("metrics compare", &e, out.output)
                }
            }
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
                Ok::<_, awsdiag::common::errors::Error>(output::render(&env, out.output))
            }
            .await;
            match result {
                Ok(text) => emit(text),
                Err(e) => {
                    progress.abandon();
                    fail("metrics top", &e, out.output)
                }
            }
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
        } => match logs::groups(
            target,
            pattern.as_deref(),
            out.limit,
            &Progress::new(out.progress),
        )
        .await
        {
            Ok(e) => emit(output::render(&e, out.output)),
            Err(e) => fail("logs groups", &e, out.output),
        },

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
                    return Ok::<_, awsdiag::common::errors::Error>(output::render(
                        &env, out.output,
                    ));
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

                let clusters =
                    logs::cluster_events(target, &req, &scan.events, base, &progress).await?;
                let env = Envelope::new("logs scan", params, clusters).truncated(scan.truncated);
                Ok(output::render(&env, out.output))
            }
            .await;
            match result {
                Ok(text) => emit(text),
                Err(e) => fail("logs scan", &e, out.output),
            }
        }

        LogsAction::Drill {
            cluster,
            group,
            stream,
            filter,
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
                    max_groups: 1,
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
                Ok::<_, awsdiag::common::errors::Error>(output::render(&env, out.output))
            }
            .await;
            match result {
                Ok(text) => emit(text),
                Err(e) => fail("logs drill", &e, out.output),
            }
        }

        LogsAction::Tail {
            group,
            filter,
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
                    max_groups: 1,
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
                Ok::<_, awsdiag::common::errors::Error>(output::render(&env, out.output))
            }
            .await;
            match result {
                Ok(text) => emit(text),
                Err(e) => fail("logs tail", &e, out.output),
            }
        }
    }
}

fn emit(text: String) -> std::process::ExitCode {
    println!("{text}");
    std::process::ExitCode::SUCCESS
}

/// Emit the error envelope on stdout and a human summary on stderr.
///
/// stdout stays machine-parseable in every case, so a caller can pipe to `jq`
/// unconditionally rather than branching on the exit code first; the stderr
/// line is what a person sees when running it by hand.
fn fail(command: &str, err: &Error, format: OutputFormat) -> std::process::ExitCode {
    let envelope = ErrorEnvelope::new(command, err);
    match format {
        OutputFormat::Text => println!("error: {err}"),
        _ => println!(
            "{}",
            serde_json::to_string_pretty(&envelope).unwrap_or_default()
        ),
    }
    eprintln!("error: {err}");
    if let Some(h) = err.hint() {
        eprintln!("hint: {h}");
    }
    std::process::ExitCode::FAILURE
}
