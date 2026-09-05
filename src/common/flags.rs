//! Shared command-line arguments.
//!
//! Composed rather than flattened into one universal struct: `--since` on a
//! command with no time dimension would be accepted and silently ignored,
//! which is a worse contract than not offering it. So every command takes
//! `TargetArgs` and `OutputArgs`, and only time-ranged commands add
//! `WindowArgs`.

use crate::common::errors::Error;
use crate::common::time::parse_instant;
use chrono::{DateTime, Utc};
use clap::{Args, ValueEnum};

/// Which account(s) and region to talk to.
#[derive(Debug, Clone, Args)]
pub struct TargetArgs {
    /// A single AWS profile from ~/.aws/config.
    #[arg(long, value_name = "NAME", conflicts_with = "profiles")]
    pub profile: Option<String>,

    /// A glob selecting several profiles to query in parallel,
    /// e.g. '*-power'. Quote it so the shell does not expand it.
    #[arg(long, value_name = "GLOB")]
    pub profiles: Option<String>,

    /// Override the region configured for the profile.
    #[arg(long, value_name = "REGION")]
    pub region: Option<String>,
}

impl TargetArgs {
    /// The profile selector, defaulting to the ambient `default` profile when
    /// neither flag is given.
    pub fn selector(&self) -> &str {
        self.profiles
            .as_deref()
            .or(self.profile.as_deref())
            .unwrap_or("default")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// A single envelope object. The default, and what tooling should consume.
    Json,
    /// One JSON object per line, for streaming into line-oriented tools.
    Ndjson,
    /// Terse human-readable columns.
    Text,
}

#[derive(Debug, Clone, Args)]
pub struct OutputArgs {
    #[arg(long, value_enum, default_value = "json")]
    pub output: OutputFormat,

    /// Cap the number of records returned. Hitting the cap sets
    /// `truncated: true` in the envelope rather than failing.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Progress reporting on stderr. `auto` draws only on an interactive
    /// terminal. stdout is unaffected either way.
    #[arg(long, value_enum, default_value = "auto")]
    pub progress: crate::common::progress::ProgressMode,
}

/// The time range for commands that query over one.
#[derive(Debug, Clone, Args)]
pub struct WindowArgs {
    /// Start of the window: a duration (2h, 30m, 7d), an RFC 3339 instant,
    /// a date (2026-09-04), or a clock time (10:35).
    #[arg(long, value_name = "TIME", default_value = "1h")]
    pub since: String,

    /// End of the window. Defaults to now.
    #[arg(long, value_name = "TIME")]
    pub until: Option<String>,
}

/// A resolved, validated time range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
}

impl WindowArgs {
    /// Resolve both bounds against a single `now`, so a slow parse cannot make
    /// the window's ends disagree about what "now" was.
    pub fn resolve(&self, now: DateTime<Utc>) -> Result<Window, Error> {
        let since = parse_instant(&self.since, now)?;
        let until = match &self.until {
            Some(u) => parse_instant(u, now)?,
            None => now,
        };
        // An inverted range would return zero results and read as "nothing
        // happened", which during an incident is a dangerous thing to imply.
        if since > until {
            return Err(Error::InvertedWindow {
                since: since.to_rfc3339(),
                until: until.to_rfc3339(),
            });
        }
        Ok(Window { since, until })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 4, 14, 0, 0).unwrap()
    }

    fn args(since: &str, until: Option<&str>) -> WindowArgs {
        WindowArgs {
            since: since.into(),
            until: until.map(Into::into),
        }
    }

    #[test]
    fn until_defaults_to_now() {
        let w = args("2h", None).resolve(now()).unwrap();
        assert_eq!(w.since, Utc.with_ymd_and_hms(2026, 9, 4, 12, 0, 0).unwrap());
        assert_eq!(w.until, now());
    }

    #[test]
    fn both_bounds_accept_every_supported_form() {
        let w = args("2026-09-04T10:00:00Z", Some("30m"))
            .resolve(now())
            .unwrap();
        assert_eq!(w.since, Utc.with_ymd_and_hms(2026, 9, 4, 10, 0, 0).unwrap());
        assert_eq!(
            w.until,
            Utc.with_ymd_and_hms(2026, 9, 4, 13, 30, 0).unwrap()
        );
    }

    #[test]
    fn an_inverted_window_is_rejected_rather_than_returning_nothing() {
        // `--since 1h --until 2h` is a plausible typo, and an empty result
        // would be indistinguishable from a healthy system.
        let err = args("1h", Some("2h")).resolve(now()).unwrap_err();
        assert_eq!(err.kind(), "bad_argument");
        assert!(err.to_string().contains("starts after"));
    }

    #[test]
    fn a_zero_length_window_is_allowed() {
        let w = args("2026-09-04T10:00:00Z", Some("2026-09-04T10:00:00Z"))
            .resolve(now())
            .unwrap();
        assert_eq!(w.since, w.until);
    }

    #[test]
    fn a_bad_bound_surfaces_the_parse_error() {
        assert_eq!(
            args("yesterday", None).resolve(now()).unwrap_err().kind(),
            "bad_argument"
        );
    }

    #[test]
    fn selector_prefers_the_glob_and_falls_back_to_default() {
        let t = |p: Option<&str>, ps: Option<&str>| TargetArgs {
            profile: p.map(Into::into),
            profiles: ps.map(Into::into),
            region: None,
        };
        assert_eq!(t(None, None).selector(), "default");
        assert_eq!(t(Some("alpha-power"), None).selector(), "alpha-power");
        assert_eq!(t(None, Some("*-power")).selector(), "*-power");
    }
}
