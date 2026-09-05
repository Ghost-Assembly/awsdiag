# awsdiag

Fast, compact AWS diagnostic data acquisition — built to be driven by an AI
agent during troubleshooting, and usable by hand.

It acquires and *shapes* data. It does not analyse. The caller reasons over
the output and writes the report; `awsdiag report` renders that report into a
single self-contained HTML file.

## Why

Driving `aws` CLI + `jq` through an agent is slow, inconsistent between runs,
and expensive in tokens. `awsdiag` addresses three specific costs:

- **Process startup.** `aws --version` measures ~211 ms; `awsdiag --version`
  measures ~1 ms.
- **Credential reuse.** Resolved credentials are cached on disk, so a warm
  call costs ~137 ms against ~418 ms for `aws sts get-caller-identity`.
- **No parallelism.** Scanning many log groups or several accounts is a
  sequential shell loop with the CLI. Here it is one process fanning out —
  two profiles measured ~185 ms against ~844 ms sequentially, and the gap
  widens with more targets.
- **Raw volume.** Log output is clustered into templates with counts, a time
  histogram and a per-stream breakdown, rather than dumped line by line.

Timings are the median of 5 runs on one Linux workstation against a single
region, and are there to show the shape of the difference rather than to be
a benchmark. Network-bound numbers will vary with your latency to AWS.

## Requirements

- [`mise`](https://mise.jdx.dev) — provisions the pinned Rust toolchain
- [`rustup`](https://rustup.rs) — mise's Rust backend defers to it
- [`just`](https://just.systems) — the task runner
- Node.js and npm — only for `just test-browser`, which pulls Playwright

Linux and macOS. The report is tested on Chromium and Firefox; WebKit is not
supported. Windows is not currently a build target — the credential cache
uses Unix file modes.

## Install

```bash
mise install          # pinned Rust toolchain
just build            # ./target/release/awsdiag
```

## AWS permissions

`awsdiag` is **read-only**. It calls exactly seven API operations and no
mutating operation exists anywhere in the codebase:

```
sts:GetCallerIdentity
ec2:DescribeInstances          ec2:DescribeInstanceStatus
logs:DescribeLogGroups         logs:FilterLogEvents
cloudwatch:ListMetrics         cloudwatch:GetMetricData
```

The AWS managed policy `ReadOnlyAccess` covers all of them, as does
`ViewOnlyAccess`. Credentials come from the SDK's own provider chain — the
tool never asks for a key and never stores a long-lived one. See
**Credential cache** below for what it does keep on disk.

## Output contract

Every subcommand emits the same envelope:

```json
{ "ok": true, "command": "whoami", "params": {}, "count": 1,
  "truncated": false, "next": null, "data": [ … ] }
```

- `truncated: true` means **the result is incomplete**. Narrow the window or
  raise `--limit`; do not treat it as a full picture.
- `count` is always derived from `data`, so it cannot disagree with it.
- Failures use the same shape with `ok: false`, a stable `kind`, and a `hint`
  naming the command that fixes it. The error envelope goes to **stdout** so
  it stays pipeable to `jq`; a human-readable summary goes to stderr.

## Common flags

```
--profile <name>        a single profile from ~/.aws/config
--profiles '<glob>'     fan out across profiles, e.g. '*-power'
--region <region>       override the profile's region
--since / --until       2h | 30m | 7d | 2026-09-04T10:00:00Z | 2026-09-04 | 10:35
--output json|ndjson|text
--limit <n>
```

A bare clock time that has not happened yet today resolves to yesterday, so
`--since 10:35` always means the 10:35 that already passed.

## report

```bash
awsdiag report --schema > findings.json     # a worked example of the shape
awsdiag report --data findings.json --out incident.html
```

One HTML file. uPlot, CSS and JS are inlined, so it opens from `file://`,
survives being emailed, needs no network, and prints to PDF. Dark and light
follow the viewer.

The document is deliberately shaped like the tool's own output — a chart's
`series` and `rows` are exactly `metrics compare`'s `params.labels` and
`data`, and a cluster is exactly what `logs scan` emits — so you paste rather
than transform. Everything except `title` is optional; a log-only incident
does not have to invent charts.

Charts share one x-axis with a **synced cursor**: hover anywhere and every
chart shows its value at that instant. Brush-drag zooms all of them together,
series toggle across every chart at once, and `events` draw vertical markers
**labelled on the line itself** so a spike lines up with the deploy that
caused it. A series keeps one colour across every chart and its toggle, which
is what makes comparing panels possible. **Axes are in
UTC**, matching the rest of the report rather than the viewer's local zone.

Log clusters render as a searchable, sortable table; a row expands to its
exemplar, per-stream breakdown and histogram sparkline.

Unknown fields are rejected rather than ignored — a typo that silently dropped
a whole panel would be worse than an error — and the message names the field,
its position and the valid alternatives.

### Safety

The report embeds raw log lines, which are written by whatever produced the
logs. `<`, `>`, `&` and the JS line terminators are escaped in the JSON
payload so a message containing `</script>` cannot close the element, and the
page builds every node with `textContent` — nothing uses `innerHTML`. Both
properties are asserted by unit tests and by a browser test that checks no
element was injected.

## Progress

Long commands report what they are doing on **stderr**:

```
⠋ scanning 3 log groups
  ⠙ /aws/lambda/alpha   page 12  8,400 events
  ✓ /aws/lambda/bravo   4,000 events
```

`--progress auto|always|never` (default `auto`). `auto` draws only when stderr
is an interactive terminal, so piping, redirecting and CI logs stay clean
without needing a flag. `NO_COLOR` drops the colour but keeps the progress.

**stdout is never touched.** Progress output goes exclusively to stderr, and
`tests/stdout_is_never_touched_by_progress.rs` asserts stdout is byte-identical
with progress forced on versus off — including the error-envelope path. That
matters because the failure would otherwise be intermittent (only on a
terminal, only while a command is slow) and could survive a long time unnoticed.

`always` exists to force drawing when stderr is redirected. Since a redirected
stream cannot be redrawn in place, it emits each frame on its own line; `auto`
is what you want interactively.

## Credential cache

Resolved credentials are cached at `$XDG_STATE_HOME/awsdiag/creds`
(default `~/.local/state/awsdiag/creds`), directory `0700`, files `0600`,
keyed by a hash of the profile name. This is the same class of short-lived
material the AWS CLI already caches in `~/.aws/cli/cache/`.

Entries are refused within 120 s of expiry, and any unreadable or corrupt
entry is treated as a miss rather than an error — the cache must never be why
a command fails mid-incident.

```
awsdiag cache status     # what is stored, and when it expires
awsdiag cache clear      # remove everything
AWSDIAG_NO_CACHE=1 …     # bypass reads and writes entirely
```

## logs

```bash
awsdiag logs groups --profile p --pattern '/aws/lambda/*'
awsdiag logs scan   --profile p --group '/aws/lambda/my-fn' --since 2h --baseline 24h
awsdiag logs drill  --profile p --group '/aws/lambda/my-fn' --cluster c7f3a1 --since 2h
awsdiag logs tail   --profile p --group '/aws/lambda/my-fn' --since 15m
```

`scan` returns **clusters, not lines**. Events sharing a template are grouped,
so 3,000 events become a handful of rows. Measured against a 3,000-event
production log group of JSON service events: 4 clusters.

Each cluster carries the three fields that do the diagnostic work:

- `histogram` — 60 buckets across the window: onset, plateau, recovery.
- `by_stream` — *where*. "Everywhere, or one host?" is often the whole answer.
- `status` — with `--baseline`, each cluster is `new`, `spiking` or `steady`.
  What is new since the preceding period is usually the thing that broke.

### metrics

```bash
awsdiag metrics compare --profile p --since 6h \
  --series 'AWS/EC2/CPUUtilization,InstanceId=i-aaa' \
  --series 'AWS/EC2/CPUUtilization,InstanceId=i-bbb'
```

Series format is `Namespace/MetricName[:Stat][,Dim=Value...]`; the statistic
defaults to `Average`. Every series lands on **one shared timeline**, so
reading across a row shows what every resource was doing at that instant:

```
ts                    CPUUtilization i-aaa  CPUUtilization i-bbb
2026-09-04T10:00:00Z  12.4                  11.8
2026-09-04T10:01:00Z  91.2                  -
```

Two details that are easy to get wrong by hand:

- **A gap is `null`, never `0`.** A missing datapoint means the resource was
  not reporting; rendering it as zero reads as healthy, which is the opposite
  of the truth during an outage.
- **The period is chosen for you.** CloudWatch keeps 1-minute data for 15
  days, 5-minute for 63, 1-hour for 455. Asking for finer data than is
  retained returns an *empty result*, indistinguishable from a flat metric.
  `--period` overrides when you want a specific resolution.

- **Sparse results are explained.** A metric published every 5 minutes queried
  at 60s returns a matrix that is 80% empty — which reads as an outage. The
  reported `native_period_seconds` and `note` say what the metric's real
  interval is and what `--period` to pass instead.

Column names come from whatever actually distinguishes the series — the
differing dimension, the metric name, or both — so a legend never contains
two identical entries.

### metrics top

```bash
awsdiag metrics top --profile p --namespace AWS/EC2 --metric CPUUtilization \
  --dimension InstanceId --by max --count 10 --since 2h
```

Ranks resources **without needing their ids** — dimension values are
discovered from CloudWatch, so you ask "which instances are hottest" rather
than supplying a list you had to find first. That is usually where diagnosis
starts.

`--by max` finds what spiked, `--by mean` what is persistently busy, `--by
latest` what is wrong *right now*. Each row carries min/mean/max/latest and a
datapoint count, and resources with no data sort last rather than counting as
zero.

`candidates` and `reporting` in the params show how many resources were
discovered versus how many actually returned data — in a real account most
discovered ids belong to terminated instances. Discovery is capped, and
hitting the cap sets `truncated`.

## ec2

```bash
awsdiag ec2 ls     --profile p --state running --name 'web-*'
awsdiag ec2 show   --profile p --name 'web-01'
awsdiag ec2 health --profile p
```

`ls` filters **server-side** — `--name` accepts EC2's own `*` and `?`
wildcards — so a large account is not paged back in full just to discard most
of it.

`show` adds what an instance is made of — attached volumes, security groups,
every tag — plus whether **detailed monitoring** is on, which decides the
finest resolution `metrics compare` can return for it.

`health` combines status checks with **scheduled events**, and sorts anything
not `ok` to the top. Scheduled events are included because an instance can
pass every status check and still be scheduled for a stop tonight, which is
precisely what a status check alone hides.

## A known limitation

Clustering groups by template, and a value it cannot recognise by shape stays
part of the template. A host name is the common case — `node-4c2e`,
`db-primary` and `srv00427` are all plausible, so there is no format to match
on. A log line stamped with its host therefore produces one cluster per host.

A pass that merged such templates automatically was tried and reverted: it
collapsed every JSON event into a single meaningless row, and merged Windows
Event IDs 4625 and 5145 into one cluster. Correct-but-verbose beats
wrong-but-tidy, so clustering stays conservative for now.
`tests/clustering_invariants.rs` records the properties any future attempt
must satisfy.

`cluster_id` is a hash of the template and is stable across runs, so
`logs drill --cluster <id>` returns the verbatim lines behind it. Add
`--stream` to narrow to one host, or `--no-cluster` to skip clustering.

### Reading truncation on a scan

CloudWatch returns events **oldest first**. If a scan hits `--limit`, you get
the *earliest* slice of the window and the newest events are missing — which
looks identical to a quiet system unless you check. `params.coverage` always
reports the range actually returned, and adds an explicit note when the window
was not fully covered. Narrow `--since` or raise `--limit` when you see it.

## Status

Working: shared plumbing and credential cache, `whoami`, `cache`, `logs`
(groups, scan, drill, tail) with clustering and baseline classification,
`metrics` (get, compare, top), `ec2` (ls, show, health), and `report`.

Planned: in-guest collection over SSM (`host`), and the remaining services —
`ebs`, `elb`, `lambda`, `apigw`, `fsx`, `s3`, `route53`, `trail`.

## Tasks

`just setup | fmt | lint | test | security | build | run | clean | ci`

`just ci` is what CI runs. Run it before pushing. `just security` runs
`cargo audit`, `cargo deny check`, `gitleaks`, `actionlint` and `zizmor`.

## Credits

Charts are rendered by [uPlot](https://github.com/leeoniya/uPlot) 1.6.32 by
Leon Sorokin, MIT licensed, vendored into `assets/vendor/` and compiled into
the binary so reports open with no network. See
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

## Licence

MIT — see [LICENSE](LICENSE).
