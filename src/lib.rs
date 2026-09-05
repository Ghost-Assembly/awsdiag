//! `awsdiag` — AWS diagnostic data acquisition and report rendering.

// Shipped code contains no `unsafe`. The exemption is for tests only:
// Rust 2024 made `std::env::set_var` an unsafe fn, and several tests must set
// AWS_* variables to exercise the credential and profile paths. Scoping the
// lint with `not(test)` keeps the guarantee where it matters instead of
// weakening it to `deny` everywhere.
#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod cluster;
pub mod cmd;
pub mod common;
pub mod metrics;
pub mod output;
pub mod report;
