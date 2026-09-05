//! Structured errors carrying an actionable `hint`.
//!
//! Every variant answers two questions: what went wrong, and what to run next.
//! The second is what lets a caller recover on its own instead of retrying the
//! same failing call, and expired SSO credentials — by far the most common
//! failure here — are otherwise indistinguishable from a permissions problem.

use crate::common::time::TimeError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Time(#[from] TimeError),

    #[error("AWS credentials for profile `{profile}` are missing or expired")]
    Auth {
        profile: String,
        /// Whether the profile authenticates via SSO. Decides which recovery
        /// command the hint names; suggesting `aws sso login` for a profile
        /// using static keys sends the caller somewhere that cannot help.
        sso: bool,
    },

    #[error("access denied calling {operation} with profile `{profile}`")]
    AccessDenied { profile: String, operation: String },

    #[error("no configured profile matches {pattern:?}")]
    NoProfileMatch { pattern: String },

    #[error("time window starts after it ends: since={since} until={until}")]
    InvertedWindow { since: String, until: String },

    #[error("cannot parse {spec:?}: {reason}")]
    BadSpec { spec: String, reason: String },

    #[error("{source_name} is not a valid findings document: {detail}")]
    BadFindings { source_name: String, detail: String },

    #[error("{message}")]
    Aws { operation: String, message: String },

    #[error("cannot {action} {path}: {source}")]
    File {
        action: &'static str,
        path: String,
        source: std::io::Error,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// A stable machine-readable discriminant. Callers branch on this rather
    /// than pattern-matching the human message, which is free to change.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Time(_) | Error::InvertedWindow { .. } | Error::BadSpec { .. } => "bad_argument",
            Error::BadFindings { .. } => "bad_findings",
            Error::Auth { .. } => "auth",
            Error::AccessDenied { .. } => "access_denied",
            Error::NoProfileMatch { .. } => "no_profile_match",
            Error::Aws { .. } => "aws",
            Error::File { .. } | Error::Io(_) => "io",
        }
    }

    /// The recovery step, where one exists. `None` means there is no single
    /// command that fixes it — better to say nothing than to invent advice.
    pub fn hint(&self) -> Option<String> {
        match self {
            Error::Auth { profile, sso: true } => {
                Some(format!("run `aws sso login --profile {profile}`"))
            }
            Error::Auth {
                profile,
                sso: false,
            } => Some(format!(
                "profile `{profile}` does not use SSO; check its credentials \
                 with `aws sts get-caller-identity --profile {profile}`"
            )),
            Error::AccessDenied { profile, operation } => Some(format!(
                "profile `{profile}` lacks permission for {operation}; \
                 try a higher-privilege profile for the same account"
            )),
            Error::NoProfileMatch { .. } => {
                Some("list configured profiles with `aws configure list-profiles`".into())
            }
            // A malformed --series spec is worth a worked example: the format
            // is not guessable from the error text alone.
            Error::BadFindings { .. } => {
                Some("print the expected shape with `awsdiag report --schema`".into())
            }
            Error::BadSpec { .. } => Some(
                "series format is Namespace/MetricName[:Stat][,Dim=Value...], \
                 e.g. AWS/EC2/CPUUtilization:Average,InstanceId=i-0abc"
                    .into(),
            ),
            Error::File { .. } => {
                Some("check the path exists and that the parent directory is writable".into())
            }
            Error::Time(_) | Error::InvertedWindow { .. } | Error::Aws { .. } | Error::Io(_) => {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_credentials_name_the_exact_login_command() {
        // The whole point of the hint: recoverable without guessing the flag.
        let e = Error::Auth {
            profile: "beta-power".into(),
            sso: true,
        };
        assert_eq!(e.kind(), "auth");
        assert_eq!(
            e.hint().unwrap(),
            "run `aws sso login --profile beta-power`"
        );
    }

    #[test]
    fn a_non_sso_profile_is_not_told_to_run_sso_login() {
        let e = Error::Auth {
            profile: "static-keys".into(),
            sso: false,
        };
        let hint = e.hint().unwrap();
        assert!(!hint.contains("sso login"), "got: {hint}");
        assert!(hint.contains("static-keys"));
    }

    #[test]
    fn access_denied_is_distinct_from_auth() {
        // Conflating these sends the caller to re-login when the real problem
        // is the role, which wastes an SSO round trip and still fails.
        let e = Error::AccessDenied {
            profile: "beta-readonly".into(),
            operation: "ssm:SendCommand".into(),
        };
        assert_eq!(e.kind(), "access_denied");
        assert!(e.to_string().contains("ssm:SendCommand"));
        assert!(e.hint().unwrap().contains("higher-privilege"));
    }

    #[test]
    fn unmatched_profile_glob_suggests_how_to_list_them() {
        let e = Error::NoProfileMatch {
            pattern: "*-admin".into(),
        };
        assert_eq!(e.kind(), "no_profile_match");
        assert!(e.to_string().contains("*-admin"));
        assert!(e.hint().is_some());
    }

    #[test]
    fn errors_without_a_known_remedy_offer_no_hint() {
        // Fabricating a plausible-sounding fix is worse than admitting none.
        assert!(
            Error::Aws {
                operation: "logs:FilterLogEvents".into(),
                message: "throttled".into()
            }
            .hint()
            .is_none()
        );
        assert!(Error::Time(TimeError::Empty).hint().is_none());
    }

    #[test]
    fn time_errors_convert_and_keep_their_message() {
        let e: Error = TimeError::MissingUnit("12".into()).into();
        assert_eq!(e.kind(), "bad_argument");
        assert!(e.to_string().contains("missing a unit"));
    }
}
