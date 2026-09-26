# Contributing

## Before you open an issue

**Do not paste real diagnostic output.** `awsdiag`'s output is, by design,
account identifiers, resource identifiers and raw log content from a live AWS
environment. A generated `report.html` is a full incident export.

Reduce a bug to a synthetic example wherever you can. Where you genuinely
cannot, redact before pasting: account ids, instance and volume ids, ARNs, log
group names, hostnames, IP addresses and the log content itself.

The repository ignores `*.html`, `*findings*.json`, `*.ndjson`, `*.csv` and
`*.log` for the same reason — so a real capture in your working tree cannot be
committed by accident.

## Getting set up

```bash
just setup     # mise install, rustfmt + clippy, cargo fetch
just ci        # what CI runs: fmt, clippy -D warnings, tests, security
```

`just test-browser` additionally runs the Playwright suite against a generated
report. It needs Node and downloads Chromium and Firefox. WebKit is not
supported.

## House rules

- `just ci` must pass. Clippy warnings are errors.
- Conventional Commits, imperative subject, no trailing period. PRs are
  squashed on merge.
- Tests come with the change. If a test would still pass with the feature
  removed, it is not testing the feature — several bugs in this codebase were
  found exactly there.

The invariants a change must not break — read-only AWS calls, stdout
purity, the JSON envelope contract, no `unwrap()`/`expect()` outside tests,
`forbid(unsafe_code)`, the `aws-sdk-*` feature flags, and the rest — are kept
in one place: [`AGENTS.md`](AGENTS.md). That file is the source of truth for
agent-grade rules; this section stays intentionally short rather than
repeating them.
