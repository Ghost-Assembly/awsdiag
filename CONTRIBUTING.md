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
- Conventional Commits, imperative subject, no trailing period.
- Tests come with the change. If a test would still pass with the feature
  removed, it is not testing the feature — several bugs in this codebase were
  found exactly there.
- No `unwrap()` or `expect()` outside test code. The crate is
  `forbid(unsafe_code)` for everything that ships.
- New `aws-sdk-*` dependencies need `default-features = false` plus
  `default-https-client` and `rt-tokio` — the default features pull a legacy
  TLS stack with known advisories. `just security` catches a regression.
