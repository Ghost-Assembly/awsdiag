//! SDK configuration and error translation.

use crate::common::credcache;
use crate::common::errors::Error;
use crate::common::flags::TargetArgs;
use crate::common::profiles;
use aws_config::BehaviorVersion;
use aws_config::retry::RetryConfig;
use aws_config::timeout::TimeoutConfig;
use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_types::SdkConfig;
use chrono::Utc;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

/// Location of the AWS config file, honouring `AWS_CONFIG_FILE`.
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("AWS_CONFIG_FILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".aws").join("config")
}

/// Profile names configured on this machine.
///
/// A missing config file yields an empty list rather than an error; the
/// caller's glob will then fail with a message naming the pattern, which is
/// more useful than a bare "file not found".
pub fn available_profiles() -> Result<Vec<String>, Error> {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(profiles::parse_profile_names(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Expand `--profile` / `--profiles` into the concrete profiles to query.
pub fn resolve_targets(target: &TargetArgs) -> Result<Vec<String>, Error> {
    let available = available_profiles()?;
    let selector = target.selector();
    // An explicit `--profile` naming something absent from the config could
    // still work via environment credentials, so only globs and non-default
    // names are validated against the file.
    if target.profiles.is_none() && selector == "default" && available.is_empty() {
        return Ok(vec!["default".to_string()]);
    }
    Ok(profiles::resolve(selector, &available)?
        .into_iter()
        .map(str::to_owned)
        .collect())
}

/// Build an SDK config for one profile, using cached credentials when a
/// usable entry exists.
///
/// On a hit the provider chain is replaced with the cached credentials, which
/// skips the `sso:GetRoleCredentials` round trip; region still resolves from
/// the profile, which is a local file read. On a miss credentials are
/// resolved eagerly and stored, so the cost is paid once rather than by every
/// later process.
pub async fn config_for(profile: &str, region: Option<&str>) -> SdkConfig {
    // The key must cover what decides the principal, not just the profile
    // name -- see `credcache::key_for`.
    let config_path = config_path();
    let config_text = std::fs::read_to_string(&config_path).unwrap_or_default();
    let key = credcache::key_for(
        profile,
        &config_path,
        &crate::common::profiles::section_text(&config_text, profile),
    );
    let cached = credcache::load(&key, Utc::now());

    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .profile_name(profile)
        .timeout_config(timeouts())
        .retry_config(retries());
    if let Some(r) = region {
        loader = loader.region(aws_config::Region::new(r.to_string()));
    }
    if let Some(entry) = &cached {
        loader = loader.credentials_provider(SharedCredentialsProvider::new(Credentials::new(
            entry.access_key_id.clone(),
            entry.secret_access_key.clone(),
            entry.session_token.clone(),
            entry.expires_at.map(SystemTime::from),
            "awsdiag-cache",
        )));
    }

    let config = loader.load().await;
    if cached.is_some() {
        return config;
    }

    // Cache miss. Resolve once, store it, and hand the resolved credentials
    // back to the config so the SDK does not repeat the fetch on the first
    // API call -- doing both cost ~360 ms of duplicated round trip.
    let Some(provider) = config.credentials_provider() else {
        return config;
    };
    let Ok(creds) = provider.provide_credentials().await else {
        return config;
    };
    credcache::store(&key, &creds);
    config
        .into_builder()
        .credentials_provider(SharedCredentialsProvider::new(creds))
        .build()
}

/// Connect timeout, deliberately more generous than the SDK default.
///
/// The default (~3.1s) is tuned for a datacentre. Measured from a VPN'd WSL2
/// host it produced repeated `HTTP connect timeout occurred after 3.1s`
/// failures on a link that was working fine — a diagnostic tool that gives up
/// on a slow network is useless precisely when the network is the problem.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Ceiling for one attempt, including transfer. Generous because a wide
/// `FilterLogEvents` page can legitimately take a while.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);

/// Total budget for an operation across retries.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(180);

fn timeouts() -> TimeoutConfig {
    TimeoutConfig::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .operation_attempt_timeout(ATTEMPT_TIMEOUT)
        .operation_timeout(OPERATION_TIMEOUT)
        .build()
}

/// More attempts than the default three: transient connect failures on a
/// flaky link are exactly what retries exist for, and a diagnostic run is
/// worth a few more seconds rather than a spurious error mid-incident.
fn retries() -> RetryConfig {
    RetryConfig::standard().with_max_attempts(5)
}

/// Translate an SDK error into our structured form.
///
/// The SDK does not expose a single typed variant for "credentials are stale",
/// so this walks the source chain for the markers AWS actually emits. It is a
/// heuristic, and deliberately conservative: anything unrecognised stays a
/// generic `Aws` error with the original message intact, rather than being
/// mislabelled as an auth problem and sending the caller to re-login for
/// something a login will not fix.
pub fn map_sdk_error<E: std::error::Error + 'static>(
    err: E,
    profile: &str,
    operation: &str,
    sso: bool,
) -> Error {
    // Joined with ": ", the conventional outer-to-inner error chain. Newlines
    // here would break single-line table output and make the JSON harder to
    // read for no gain.
    let mut parts: Vec<String> = Vec::new();
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&err);
    while let Some(e) = source {
        let text = e.to_string();
        // The SDK repeats the same wrapper text at several levels.
        if parts.last() != Some(&text) {
            parts.push(text);
        }
        source = e.source();
    }
    let chain = parts.join(": ");
    let lower = chain.to_lowercase();

    // Verified against the messages this SDK version actually emits for a
    // logged-out SSO profile, not guessed: the first version of this list
    // missed "failed to load the cached SSO token" entirely, so the most
    // common failure in practice produced no recovery hint at all.
    const AUTH_MARKERS: [&str; 9] = [
        "expiredtoken",
        "token has expired",
        "sso session associated with this profile has expired",
        "no credentials in the property bag",
        "credentialsnotloaded",
        "failed to load credentials",
        "failed to load the cached sso token",
        "error occurred while loading an access token",
        "error occurred while loading credentials",
    ];
    const DENIED_MARKERS: [&str; 3] = [
        "accessdenied",
        "not authorized to perform",
        "unauthorizedoperation",
    ];

    if AUTH_MARKERS.iter().any(|m| lower.contains(m)) {
        return Error::Auth {
            profile: profile.to_string(),
            sso,
        };
    }
    if DENIED_MARKERS.iter().any(|m| lower.contains(m)) {
        return Error::AccessDenied {
            profile: profile.to_string(),
            operation: operation.to_string(),
        };
    }
    Error::Aws {
        operation: operation.to_string(),
        message: chain.trim_end().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Chained {
        msg: String,
        source: Option<Box<Chained>>,
    }
    impl std::fmt::Display for Chained {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.msg)
        }
    }
    impl std::error::Error for Chained {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source
                .as_deref()
                .map(|e| e as &(dyn std::error::Error + 'static))
        }
    }
    fn err(outer: &str, inner: Option<&str>) -> Chained {
        Chained {
            msg: outer.into(),
            source: inner.map(|i| {
                Box::new(Chained {
                    msg: i.into(),
                    source: None,
                })
            }),
        }
    }

    /// The chain this SDK emits for a logged-out SSO profile.
    ///
    /// These are the SDK's own wrapper strings, not service output. They are
    /// pinned here because the marker list is matched against them: if a SDK
    /// upgrade reworded any of them the expiry hint would silently stop
    /// firing, which is exactly the failure this guards.
    const LOGGED_OUT_SSO_CHAIN: [&str; 5] = [
        "dispatch failure",
        "an error occurred while loading credentials",
        "an error occurred while loading credentials",
        "an error occurred while loading an access token",
        "failed to load the cached SSO token",
    ];

    fn nest(msgs: &[&str]) -> Chained {
        let mut node = Chained {
            msg: msgs[msgs.len() - 1].into(),
            source: None,
        };
        for m in msgs[..msgs.len() - 1].iter().rev() {
            node = Chained {
                msg: (*m).into(),
                source: Some(Box::new(node)),
            };
        }
        node
    }

    #[test]
    fn the_real_logged_out_sso_chain_produces_a_login_hint() {
        // This is the case that shipped broken: the observed message matched
        // none of the guessed markers, so the commonest failure gave no hint.
        let mapped = map_sdk_error(
            nest(&LOGGED_OUT_SSO_CHAIN),
            "beta-power",
            "sts:GetCallerIdentity",
            true,
        );
        assert_eq!(mapped.kind(), "auth");
        assert_eq!(
            mapped.hint().unwrap(),
            "run `aws sso login --profile beta-power`"
        );
    }

    #[test]
    fn the_rendered_chain_is_single_line_and_deduplicated() {
        // Newlines broke text-mode column alignment; the SDK also repeats the
        // same wrapper text at consecutive levels.
        let mapped = map_sdk_error(nest(&LOGGED_OUT_SSO_CHAIN), "p", "op", true);
        let Error::Auth { .. } = mapped else {
            panic!("expected auth")
        };
        let generic = map_sdk_error(nest(&["outer", "outer", "inner detail"]), "p", "op", true);
        let text = generic.to_string();
        assert!(!text.contains('\n'), "single line: {text:?}");
        assert_eq!(text, "outer: inner detail");
    }

    #[test]
    fn expired_sso_is_recognised_from_a_nested_source() {
        // The useful marker is never in the outermost message, so a
        // non-recursive check would miss every real expiry.
        let e = err(
            "dispatch failure",
            Some("the SSO session associated with this profile has expired"),
        );
        assert_eq!(
            map_sdk_error(e, "beta-power", "sts:GetCallerIdentity", true).kind(),
            "auth"
        );
    }

    #[test]
    fn expired_token_service_error_is_recognised() {
        let e = err(
            "service error",
            Some("ExpiredToken: The security token included in the request is expired"),
        );
        assert_eq!(
            map_sdk_error(e, "p", "sts:GetCallerIdentity", true).kind(),
            "auth"
        );
    }

    #[test]
    fn access_denied_maps_to_its_own_kind() {
        let e = err(
            "service error",
            Some("AccessDenied: User is not authorized to perform ssm:SendCommand"),
        );
        let mapped = map_sdk_error(e, "beta-readonly", "ssm:SendCommand", true);
        assert_eq!(mapped.kind(), "access_denied");
        assert!(mapped.hint().unwrap().contains("higher-privilege"));
    }

    #[test]
    fn an_unrecognised_error_keeps_its_message_and_is_not_called_auth() {
        // Mislabelling a throttle as expired credentials sends the caller to
        // `aws sso login`, which wastes time and does not help.
        let e = err(
            "dispatch failure",
            Some("ThrottlingException: Rate exceeded"),
        );
        let mapped = map_sdk_error(e, "p", "logs:FilterLogEvents", true);
        assert_eq!(mapped.kind(), "aws");
        assert!(mapped.to_string().contains("Rate exceeded"));
        assert!(mapped.hint().is_none());
    }

    #[test]
    fn marker_matching_is_case_insensitive() {
        let e = err("x", Some("EXPIREDTOKEN"));
        assert_eq!(map_sdk_error(e, "p", "op", true).kind(), "auth");
    }

    #[test]
    fn config_path_honours_the_environment_override() {
        // Uses a distinct value so it cannot pass by coincidence.
        let prev = std::env::var("AWS_CONFIG_FILE").ok();
        unsafe { std::env::set_var("AWS_CONFIG_FILE", "/tmp/awsdiag-test-config") };
        assert_eq!(config_path(), PathBuf::from("/tmp/awsdiag-test-config"));
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AWS_CONFIG_FILE", v),
                None => std::env::remove_var("AWS_CONFIG_FILE"),
            }
        }
    }
}
