//! What a failure writes, and where, in each output format.
//!
//! Every case here fails before any AWS call, so the suite stays hermetic:
//! `AWS_CONFIG_FILE` points at a file that does not exist, which makes any
//! named profile unknown.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_awsdiag");

struct Run {
    stdout: String,
    stderr: String,
    ok: bool,
}

fn run(args: &[&str]) -> Run {
    let tmp = std::env::temp_dir();
    let out = Command::new(BIN)
        .args(args)
        .env(
            "AWS_CONFIG_FILE",
            tmp.join("awsdiag-error-output-no-such-config"),
        )
        .env("XDG_STATE_HOME", tmp.join("awsdiag-error-output-test"))
        .env("AWSDIAG_NO_CACHE", "1")
        .output()
        .expect("binary runs");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        ok: out.status.success(),
    }
}

const UNKNOWN_PROFILE: [&str; 3] = ["whoami", "--profile", "definitely-not-a-real-profile"];

fn unknown_profile(format: &str) -> Run {
    let mut args = UNKNOWN_PROFILE.to_vec();
    args.extend(["--output", format, "--progress", "never"]);
    run(&args)
}

#[test]
fn json_failure_is_one_envelope_on_stdout() {
    let r = unknown_profile("json");
    assert!(!r.ok);
    let v: serde_json::Value = serde_json::from_str(r.stdout.trim()).expect("envelope parses");
    assert_eq!(v["ok"], serde_json::json!(false));
    assert_eq!(v["kind"], serde_json::json!("no_profile_match"));
}

#[test]
fn ndjson_failure_is_exactly_one_line() {
    // It was the pretty-printed, multi-line envelope, so a line-oriented
    // reader got a dozen fragments, none of which parsed.
    let r = unknown_profile("ndjson");
    assert!(!r.ok);
    let lines: Vec<&str> = r.stdout.lines().collect();
    assert_eq!(lines.len(), 1, "one line: {:?}", r.stdout);
    let v: serde_json::Value = serde_json::from_str(lines[0]).expect("the line parses");
    assert_eq!(v["ok"], serde_json::json!(false));
    assert_eq!(v["kind"], serde_json::json!("no_profile_match"));
}

#[test]
fn text_failure_goes_to_stderr_only() {
    // It was printed to stdout *and* stderr, so a person saw it twice and a
    // redirect captured an error line as if it were data.
    let r = unknown_profile("text");
    assert!(!r.ok);
    assert_eq!(r.stdout, "", "nothing on stdout");
    assert_eq!(
        r.stderr.matches("error:").count(),
        1,
        "stated once: {}",
        r.stderr
    );
    assert!(r.stderr.contains("hint:"), "{}", r.stderr);
}

#[test]
fn an_unacceptable_period_is_a_bad_argument_before_any_call() {
    for args in [
        vec![
            "metrics",
            "compare",
            "--series",
            "AWS/EC2/CPUUtilization",
            "--period",
            "0",
        ],
        vec![
            "metrics",
            "top",
            "--namespace",
            "AWS/EC2",
            "--metric",
            "CPUUtilization",
            "--dimension",
            "InstanceId",
            "--period",
            "45",
        ],
    ] {
        let mut args = args.clone();
        args.extend(["--profile", "definitely-not-a-real-profile"]);
        let r = run(&args);
        assert!(!r.ok, "{args:?}");
        let v: serde_json::Value = serde_json::from_str(r.stdout.trim()).expect("envelope parses");
        // bad_argument, not no_profile_match: rejected before the profile is
        // even looked up, let alone called.
        assert_eq!(v["kind"], serde_json::json!("bad_argument"), "{args:?}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("--period"),
            "{v}"
        );
    }
}

#[test]
fn ec2_show_without_instance_or_name_is_a_bad_argument_with_no_series_hint() {
    // This used to be `BadSpec`, whose hint describes the `--series` format --
    // nonsensical advice for a command that has no such flag.
    let args = ["ec2", "show", "--profile", "definitely-not-a-real-profile"];
    let r = run(&args);
    assert!(!r.ok);
    let v: serde_json::Value = serde_json::from_str(r.stdout.trim()).expect("envelope parses");
    assert_eq!(v["kind"], serde_json::json!("bad_argument"));
    assert!(v["hint"].is_null(), "no --series hint here: {v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap_or_default()
            .contains("--instance"),
        "{v}"
    );
}
