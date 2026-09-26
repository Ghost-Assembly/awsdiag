# Working on awsdiag

`awsdiag` is a Rust CLI: fast, compact AWS diagnostic data collection, built
to be driven by an AI agent and rendered into a single self-contained HTML
report. README.md is for humans; this file holds the rules an agent working
in this repository must not break.

## What gets published

- A GitHub Release per tag: a Linux tarball
  (`awsdiag-x86_64-unknown-linux-gnu.tar.gz`) and a macOS tarball
  (`awsdiag-aarch64-apple-darwin.tar.gz`), each with a `.sha256` alongside
  it, built by `.github/workflows/release.yml`.
- Nothing is published to crates.io (`publish = false` in `Cargo.toml` — the
  output envelope is not a stable public API yet).
- One-liner and links: the same one-liner lives in README.md's opening
  paragraph, `Cargo.toml`'s `description`, and the crate's clap `about`.
  Keep the three in sync when either changes. The GitHub repository
  description/homepage and the hub card and profile row in the other
  Ghost Assembly repositories carry the same text — see **Cross-repo
  duties**.

## Commands

Table from the `justfile`. Run `just ci` before claiming done.

| Recipe | Does |
| --- | --- |
| `just setup` | `mise install`, `rustup component add rustfmt clippy`, `cargo fetch` |
| `just fmt` | `cargo fmt --all` |
| `just lint` | `cargo fmt --all -- --check` + `cargo clippy --all-targets --all-features -- -D warnings` |
| `just test` | `cargo test --all-features` |
| `just test-browser` | Builds a release binary, generates fixture reports, runs the Playwright suite against them (needs Node; downloads Chromium and Firefox) |
| `just security` | `cargo audit`, `cargo deny check`, `gitleaks detect`, `actionlint`, `zizmor` |
| `just coverage` | `cargo llvm-cov`, LCOV plus a browsable HTML report |
| `just sonar-reports` | The clippy JSON and LCOV report SonarQube Cloud consumes |
| `just build` | `cargo build --release` |
| `just run -- <args>` | `cargo run -- <args>` (positional; a `--profiles '*-glob'` argument is not shell-expanded) |
| `just clean` | `cargo clean` |
| `just ci` | `lint`, `test`, `security`, then a release build — the required status check |
| `just ci-full` | `ci` plus `test-browser` — CI runs both, `test-browser` as its own cached step, since it downloads ~250 MB of browsers |

`just test-browser` and `ci-full` need a real Node/npm and a browser
download; they will not run in a sandbox with no network. Everything else
needs only the pinned Rust toolchain from `mise install`.

## Hard constraints

- **Read-only AWS.** The tool calls exactly seven API operations, all
  `Describe`, `Get`, `List` or `Filter` (`sts:GetCallerIdentity`,
  `ec2:DescribeInstances`, `ec2:DescribeInstanceStatus`,
  `logs:DescribeLogGroups`, `logs:FilterLogEvents`,
  `cloudwatch:ListMetrics`, `cloudwatch:GetMetricData`). Never add a call
  that creates, modifies, or deletes an AWS resource — that is the whole
  premise the README's "AWS permissions" section promises the reader.
- **Stdout purity.** Progress output (`src/common/progress.rs`) goes
  exclusively to stderr; stdout carries only the rendered envelope.
  `tests/stdout_is_never_touched_by_progress.rs` asserts stdout is
  byte-identical with progress forced on versus off, including the error
  path. A change that writes anything else to stdout — a `println!` added
  for debugging, a progress line that leaks — breaks this invisibly (only
  on a terminal, only while a command is slow) unless the test catches it.
- **The JSON envelope contract.** Every subcommand emits
  `{ ok, command, params, count, truncated, next, data }`
  (`src/common/envelope.rs`). `count` is always derived from `data`, never
  passed in. `truncated: true` means the result is incomplete — set it
  whenever a cap or page limit was hit, never leave it to be inferred.
  `next` is reserved for a future resume token and is always `null` today.
  `--output ndjson` (`src/output.rs`) writes one row per line, then the
  envelope without `data` as a final trailer line — the only line carrying
  `ok`, so a streaming consumer tells it apart from a row with
  `jq -c 'select(has("ok") | not)'`. An empty result is still exactly that
  one trailer line, with `count: 0`. Do not special-case an empty result
  into zero output lines.
- **Exit codes.** Two only: `ExitCode::SUCCESS` (0) on a rendered result,
  `ExitCode::FAILURE` (1) on any `Error`. Do not introduce per-error-kind
  exit codes; callers branch on the envelope's `kind` field instead
  (`src/common/errors.rs`), which is why every `Error` variant is mapped to
  a stable `kind()` string and, where recoverable, a `hint()`.
- **No `unwrap()` or `expect()` in production paths.** Test code is exempt.
  A parse or lookup that "cannot fail" still gets a `?` or an explicit
  fallback — see `src/common/time.rs`'s comment on why an `and_hms_opt`
  that provably cannot fail is still not unwrapped.
- **`#![forbid(unsafe_code)]`** at the crate root, scoped to `not(test)`.
  The only exemption is that Rust 2024 made `std::env::set_var` an unsafe
  fn, and several tests must set `AWS_*` variables to exercise credential
  and profile paths; that unsafety never ships. Do not widen the
  exemption or add `unsafe` to shipped code.
- **aws-sdk feature flags.** Every `aws-sdk-*` dependency in `Cargo.toml`
  needs `default-features = false` plus `default-https-client` and
  `rt-tokio`. The default features pull a legacy hyper-0.14 + rustls-0.21
  stack with known RustSec advisories; `default-https-client` uses the
  current aws-lc path instead. `just security` (`cargo audit`) is what
  catches a regression, so do not add a new `aws-sdk-*` crate without the
  same feature list.

## Credential cache

`src/common/credcache.rs` caches resolved, short-lived STS credentials at
`$XDG_STATE_HOME/awsdiag/creds` (default `~/.local/state/awsdiag/creds`),
directory `0700`, file `0600`, written via a temp-file-then-rename so a
reader never sees a half-written entry.

The cache key (`key_for`) covers the config file path, the profile name,
**and the profile's own section** — never the profile name alone. The name
says nothing about which principal it resolves to: re-pointing a profile at
a different role, or two config files that both define the same profile
name for different accounts, must not share an entry, or `config_for`
replaces the whole provider chain on a hit and the tool silently
authenticates as the wrong principal while reporting the old name.

An entry is refused within 120 s of expiry, and any unreadable, corrupt or
missing entry reads as a plain cache miss, never an error — the cache must
never be why a command fails mid-incident. `AWSDIAG_NO_CACHE=1` disables
reading and writing entirely; ambient `AWS_ACCESS_KEY_ID` /
`AWS_SESSION_TOKEN` in the environment bypass it the same way, since those
override profile configuration outright.

Never add a `Debug` derive to `Entry` — the hand-written impl exists
specifically to keep the secret key and session token out of a log, a panic
message, or an `{:?}`.

## RUSTFLAGS path remap

The `justfile` sets `RUSTFLAGS` to `--remap-path-prefix` the cargo home and
the repo directory to `/cargo` and `/awsdiag`. `rustc` bakes `file!()` paths
into panic messages at compile time; a release binary otherwise carries the
absolute path of every crate that can panic (528 strings containing the
builder's home directory, measured — `strip = true` does not remove them).
Publishing a locally built binary would publish the builder's username.
`RUSTFLAGS` is exported at the top of the `justfile` so every cargo
invocation in every recipe is remapped consistently; do not set it
per-recipe instead — `test-browser` also builds `--release`, and a
per-recipe setting there previously rebuilt the binary without remapping
and silently undid `just build`.

## Tests

- Unit tests live beside the code in `#[cfg(test)] mod tests`.
- Integration tests live in `tests/`: `error_output.rs` (what a failure
  writes and where, in each output format), `stdout_is_never_touched_by_progress.rs`,
  `clustering_invariants.rs`, `tokenizer_robustness.rs`.
- AWS-facing unit tests use a canned-response client: an HTTP connector
  (`aws-smithy-runtime-api`, a dev-dependency already in the tree through
  the SDK) that replays fixed XML bodies in order, so pagination and
  multi-page behavior are tested without a network call or an account. See
  `src/cmd/ec2.rs`'s `mod replay` for the pattern; reuse it rather than
  inventing a second mocking approach.
  Do not add another dev-dependency mocking library — `just security`
  and `cargo deny check` should not need to gain a new crate for this.
- The generated HTML report is tested with Playwright (`tests/browser/`,
  run via `just test-browser`): it is a single file opened from disk, so a
  real browser is the only thing that can confirm it actually renders.
  Chromium and Firefox only; WebKit is not supported on this base.
- No real diagnostic output in fixtures: no real account ids, instance or
  volume ids, ARNs, log group names, hostnames, IPs, or log content.
  Synthesize everything.

## Conventions

- Conventional Commits, imperative subject, no trailing period.
- PRs are squashed on merge.
- Third-party GitHub Actions are pinned by commit SHA, never a tag.
- American English in prose, comments, identifiers and commit messages.
- Nothing personal anywhere in code, tests, fixtures or docs.
- CONTRIBUTING.md carries the human-facing contributor guide; this file is
  the source of truth for agent-grade rules. Do not duplicate a rule in
  both files — update this one and point CONTRIBUTING.md at it.

## Cross-repo duties

This repository is one of several under Ghost Assembly. Keep these in sync
with the one-liner above whenever it changes:

- The awsdiag card on the Ghost Assembly hub site.
- The awsdiag row on the maintainer's profile.
- The GitHub repository's About description and homepage URL
  (`https://ghost-assembly.com/`).

The site domain is `https://ghost-assembly.com/`; the old
`ghost-assembly.github.io` URLs 301 there. Replace only the literal
`ghost-assembly.github.io` host if you ever find it — this repository has
no such reference today.
