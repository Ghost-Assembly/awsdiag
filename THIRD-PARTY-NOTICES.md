# Third-party notices

`awsdiag` embeds the following third-party software in the binary and in every
HTML report it generates. Each is reproduced under its own licence, and the
full licence text is included alongside the vendored files.

## uPlot

- **Version:** 1.6.32
- **Licence:** MIT
- **Copyright:** Copyright (c) 2022 Leon Sorokin
- **Upstream:** https://github.com/leeoniya/uPlot
- **Full text:** [`assets/vendor/LICENSE.uPlot`](assets/vendor/LICENSE.uPlot)
- **Vendored files:** `assets/vendor/uPlot.iife.min.js`,
  `assets/vendor/uPlot.min.css`

uPlot renders the charts in the generated report. It is vendored rather than
loaded from a CDN because a report must open from `file://` with no network,
and it is compiled into the binary with `include_str!`, so every generated
report contains a copy. Both vendored files carry a `/*! … MIT … */` banner
that survives into the report output; `src/report/render.rs` has a test
asserting this, so attribution cannot be dropped by a future refactor.

## Rust dependencies

The crates listed in `Cargo.lock` are not vendored — Cargo fetches them at
build time and they are statically linked into the binary. All are under
permissive licences (predominantly `MIT OR Apache-2.0`, plus `Unicode-3.0`,
`ISC`, `BSD-3-Clause`, `Zlib`, `0BSD`, `BlueOak-1.0.0` and `CC0-1.0`). The
allowlist is enforced by `cargo deny check licenses`, configured in
`deny.toml` and run by `just security`.
