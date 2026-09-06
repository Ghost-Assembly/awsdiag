# awsdiag task runner. `just ci` is what CI runs; run it before pushing.

_default:
    @just --list

# Install the pinned toolchain and build dependencies.
#
# `rustup component add` is not redundant: mise provisions a minimal toolchain
# that ships neither rustfmt nor clippy, so `just lint` fails on a clean
# machine without this. It is a no-op where they are already present.
setup:
    mise install
    rustup component add rustfmt clippy
    cargo fetch

# Rewrite absolute build paths out of the binary.
#
# rustc bakes `file!()` paths into panic messages at compile time, so a
# release binary carries the absolute path of every crate that can panic --
# measured at 528 strings containing the builder's home directory, which
# `strip = true` does NOT remove. Publishing a locally built binary would
# therefore publish the builder's username. `trim-paths` would be the tidy
# fix but is not stabilised in Cargo 1.98, so remap explicitly. CI builds are
# already clean (the runner's path is generic), but release artefacts must
# not depend on where they happened to be built.
export CARGO_HOME := env_var_or_default("CARGO_HOME", env_var("HOME") / ".cargo")
remap := "--remap-path-prefix=" + CARGO_HOME + "=/cargo " + \
         "--remap-path-prefix=" + justfile_directory() + "=/awsdiag"

# Exported, so every cargo invocation in every recipe is remapped. Setting it
# per-recipe meant `test-browser` -- which also builds --release -- rebuilt
# the binary without remapping and silently undid `just build`.
export RUSTFLAGS := remap + " " + env_var_or_default("RUSTFLAGS", "")

# Coverage, as an LCOV report plus a browsable HTML one.
coverage:
    cargo llvm-cov --all-features --workspace --html
    cargo llvm-cov --all-features --workspace --summary-only
    @echo "HTML report: target/llvm-cov/html/index.html"

# The two reports SonarQube Cloud consumes.
sonar-reports:
    # Clippy is not re-run by Sonar (sonar.rust.clippy.enabled=false). This is
    # the same invocation `just lint` gates on, so the report and the gate can
    # never disagree about what was checked. Without `-D warnings` it exits 0
    # on warnings and non-zero only on a genuine compile failure, which is the
    # behaviour wanted here.
    mkdir -p target/sonar
    cargo clippy --all-targets --all-features --message-format=json \
        > target/sonar/clippy-report.json
    cargo llvm-cov --all-features --workspace \
        --lcov --output-path target/sonar/lcov.info

# Format sources in place.
fmt:
    cargo fmt --all

# Lint. Warnings are errors so CI cannot drift from local.
lint:
    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings

# Run the test suite.
test:
    cargo test --all-features

# Browser tests for the generated report.
#
# The report is a single HTML file opened from disk, so the only way to know
# it works is to open it in a browser. Playwright comes from npm on demand
# and is in no manifest, which is why it is installed here rather than
# assumed. Chromium and Firefox only -- WebKit is unsupported on this base.
# Pinned: an unpinned install resolves to whatever npm serves that day, so a
# green branch could break with no repository change -- the same reasoning
# that pins GitHub Actions by commit SHA.
playwright_version := "1.63.0"

test-browser:
    npm install --no-save --no-audit --no-fund @playwright/test@{{playwright_version}}
    # --with-deps installs system libraries and needs root; it is right on a
    # CI runner and impossible on a workstation, so it follows CI.
    {{ if env_var_or_default("CI", "") == "" { "npx playwright install chromium firefox" } else { "npx playwright install --with-deps chromium firefox" } }}
    cargo build --release
    ./target/release/awsdiag report --schema > target/fixture-findings.json
    ./target/release/awsdiag report --data target/fixture-findings.json \
        --out target/fixture-report.html --output json
    node tests/browser/make-scale-fixture.mjs target/scale-findings.json
    ./target/release/awsdiag report --data target/scale-findings.json \
        --out target/scale-report.html --output json
    node tests/browser/make-probe-fixture.mjs target/probe-findings.json
    ./target/release/awsdiag report --data target/probe-findings.json \
        --out target/probe-report.html --output json
    REPORT_PATH=target/fixture-report.html \
        SCALE_REPORT_PATH=target/scale-report.html \
        PROBE_REPORT_PATH=target/probe-report.html \
        npx playwright test --config=playwright.config.mjs

# Everything security-related, in one command.
#
# `cargo audit` alone was the private-repo posture. A public repo publishes
# its history as well as its code, so secret scanning over the full history
# and Actions-specific linting both matter now. Each tool is independent --
# they are listed rather than chained with && so a failure names itself.
security:
    cargo audit
    cargo deny check
    gitleaks detect --source . --no-banner --redact
    actionlint
    zizmor --no-progress .github/workflows/

# Build the release binary.
build:
    cargo build --release

# Run the binary; pass arguments after `--`, e.g. `just run -- whoami`.
run *ARGS:
    cargo run -- {{ARGS}}

# Remove build artefacts.
clean:
    cargo clean

# Everything CI runs. The required status check is named `ci`.
ci: lint test security
    cargo build --release

# Everything, including the browser suite. Separate from `ci` because it
# downloads ~250 MB of browsers; CI runs it as its own cached step.
ci-full: ci test-browser
