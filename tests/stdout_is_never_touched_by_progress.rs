//! The contract that makes progress safe to add at all.
//!
//! This tool's primary consumer parses JSON from stdout. A spinner written
//! there corrupts it, and the failure would be intermittent — only on a
//! terminal, only while a command is slow — so it would survive a long time
//! before anyone noticed.
//!
//! `--progress always` is used deliberately: `auto` hides itself when stderr
//! is not a terminal, which a test harness never is, so `auto` would exercise
//! a disabled no-op and prove nothing.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_awsdiag");

/// A command that needs no AWS credentials, so this stays hermetic.
fn run(args: &[&str]) -> (String, String, bool) {
    let out = Command::new(BIN)
        .args(args)
        .env(
            "XDG_STATE_HOME",
            std::env::temp_dir().join("awsdiag-progress-test"),
        )
        .output()
        .expect("binary runs");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn stdout_is_byte_identical_whether_progress_is_on_or_off() {
    for args in [
        vec!["cache", "status", "--output", "json"],
        vec!["cache", "status", "--output", "text"],
        vec!["cache", "status", "--output", "ndjson"],
    ] {
        let mut off = args.clone();
        off.extend(["--progress", "never"]);
        let mut on = args.clone();
        on.extend(["--progress", "always"]);

        let (stdout_off, _, ok_off) = run(&off);
        let (stdout_on, _, ok_on) = run(&on);

        assert_eq!(ok_off, ok_on, "exit status differs for {args:?}");
        assert_eq!(
            stdout_off, stdout_on,
            "stdout differs with progress enabled for {args:?}"
        );
    }
}

#[test]
fn stdout_carries_no_escape_sequences_even_with_progress_forced_on() {
    let (stdout, _, _) = run(&[
        "cache",
        "status",
        "--output",
        "json",
        "--progress",
        "always",
    ]);
    assert!(
        !stdout.contains('\u{1b}'),
        "escape sequence reached stdout: {stdout:?}"
    );
    assert!(
        !stdout.contains('\r'),
        "carriage return reached stdout: {stdout:?}"
    );
    // And it must still be parseable, which is the point of all of this.
    serde_json::from_str::<serde_json::Value>(stdout.trim()).expect("stdout parses as JSON");
}

#[test]
fn an_error_envelope_still_reaches_stdout_with_progress_on() {
    // The failure path writes the envelope to stdout so a caller can pipe to
    // `jq` unconditionally. Progress must not disturb that either.
    let (stdout, _, ok) = run(&[
        "whoami",
        "--profile",
        "definitely-not-a-real-profile",
        "--progress",
        "always",
    ]);
    assert!(!ok, "an unknown profile fails");
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("error envelope parses");
    assert_eq!(v["ok"], serde_json::json!(false));
    assert_eq!(v["kind"], serde_json::json!("no_profile_match"));
}

#[test]
fn progress_writes_to_stderr_when_forced() {
    // Guards against the opposite failure: silently disabling progress and
    // passing the invariance test trivially.
    let (_, stderr, _) = run(&["cache", "status", "--progress", "always"]);
    assert!(
        !stderr.is_empty(),
        "forced progress produced no stderr output"
    );
}
