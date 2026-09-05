//! Discovery and glob-matching of AWS profiles from `~/.aws/config`.
//!
//! Fanning one query across several accounts is the common case here — a
//! service spans dev/test/stg/uat/prod, and "is this everywhere or just
//! prod?" is usually the first question. `--profiles '*-power'`
//! answers it in one call.
//!
//! Section names are parsed directly rather than via the SDK, which offers no
//! public listing API. Only the header lines matter, so this stays a small
//! pure function over text and is tested as one.

use crate::common::errors::Error;
use globset::Glob;

/// Extract profile names from the text of an AWS config file.
///
/// `[default]` yields `default`; `[profile foo]` yields `foo`. Other section
/// types — `[sso-session x]`, `[services x]` — are not profiles and are
/// skipped, so a glob can never select something unusable as `--profile`.
pub fn parse_profile_names(config: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in config.lines() {
        let line = line.trim();
        // Strip trailing comments before matching the closing bracket.
        let line = line.split_once('#').map_or(line, |(head, _)| head.trim());
        let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
            continue;
        };
        let section = section.trim();
        if section == "default" {
            names.push("default".to_string());
        } else if let Some(name) = section.strip_prefix("profile ") {
            let name = name.trim();
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Select the profiles matching `pattern`.
///
/// A pattern with no glob metacharacters must still match an existing
/// profile: a typo like `beta-powr` fails loudly here rather than
/// producing a confusing credential error several seconds later.
pub fn resolve<'a>(pattern: &str, available: &'a [String]) -> Result<Vec<&'a str>, Error> {
    let glob = Glob::new(pattern)
        .map_err(|e| Error::NoProfileMatch {
            pattern: format!("{pattern} ({e})"),
        })?
        .compile_matcher();

    let matched: Vec<&str> = available
        .iter()
        .filter(|p| glob.is_match(p.as_str()))
        .map(String::as_str)
        .collect();

    if matched.is_empty() {
        return Err(Error::NoProfileMatch {
            pattern: pattern.to_string(),
        });
    }
    Ok(matched)
}

/// Whether `profile` authenticates via AWS SSO.
///
/// Decides which recovery command an auth failure suggests. Keys are read
/// from the profile's own section only — `sso_session` in a neighbouring
/// profile says nothing about this one.
pub fn is_sso(config: &str, profile: &str) -> bool {
    let wanted: Vec<String> = if profile == "default" {
        vec!["default".to_string()]
    } else {
        vec![format!("profile {profile}")]
    };
    let mut inside = false;
    for line in config.lines() {
        let line = line.trim();
        let line = line.split_once('#').map_or(line, |(head, _)| head.trim());
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            inside = wanted.iter().any(|w| w == section.trim());
            continue;
        }
        if inside
            && let Some((key, _)) = line.split_once('=')
            && matches!(key.trim(), "sso_session" | "sso_start_url")
        {
            return true;
        }
    }
    false
}

/// The body of one profile's section, normalised.
///
/// Used to key the credential cache. Everything that decides *which*
/// principal a profile resolves to lives here — `sso_session`,
/// `sso_account_id`, `sso_role_name`, `role_arn`, `source_profile` — so
/// hashing the whole section catches any change to them without this code
/// needing to know which keys matter. Lines are sorted so merely reordering
/// the file does not invalidate the entry.
pub fn section_text(config: &str, profile: &str) -> String {
    let wanted = if profile == "default" {
        "default".to_string()
    } else {
        format!("profile {profile}")
    };
    let mut lines: Vec<String> = Vec::new();
    let mut inside = false;
    for line in config.lines() {
        let line = line.trim();
        let line = line.split_once('#').map_or(line, |(head, _)| head.trim());
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            inside = section.trim() == wanted;
            continue;
        }
        if inside && !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    lines.sort();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
[sso-session example-sso]
sso_start_url = https://example.awsapps.com/start
sso_region = us-east-1

[default]
region = us-east-1

[profile alpha-readonly]
sso_session = example-sso
sso_role_name = ReadOnly

[profile alpha-power]
sso_session = example-sso

[profile beta-power]
sso_session = example-sso

[profile beta-admin]
sso_session = example-sso

[profile gamma-admin]
region = us-east-1
"#;

    fn names() -> Vec<String> {
        parse_profile_names(CONFIG)
    }

    #[test]
    fn parses_default_and_named_profiles() {
        assert_eq!(
            names(),
            vec![
                "default",
                "alpha-readonly",
                "alpha-power",
                "beta-power",
                "beta-admin",
                "gamma-admin",
            ]
        );
    }

    #[test]
    fn sso_session_sections_are_not_profiles() {
        // `[sso-session example-sso]` is not usable as --profile; selecting it
        // would fail at credential resolution with an opaque message.
        assert!(!names().contains(&"example-sso".to_string()));
    }

    #[test]
    fn tolerates_comments_and_odd_spacing() {
        let cfg = "[ profile spaced ]\n[profile commented] # trailing note\n";
        assert_eq!(parse_profile_names(cfg), vec!["spaced", "commented"]);
    }

    #[test]
    fn glob_selects_across_environments() {
        let n = names();
        assert_eq!(
            resolve("*-power", &n).unwrap(),
            vec!["alpha-power", "beta-power"]
        );
    }

    #[test]
    fn glob_selects_all_roles_in_one_environment() {
        let n = names();
        assert_eq!(
            resolve("beta-*", &n).unwrap(),
            vec!["beta-power", "beta-admin"]
        );
    }

    #[test]
    fn an_exact_name_resolves_to_just_itself() {
        let n = names();
        assert_eq!(resolve("gamma-admin", &n).unwrap(), vec!["gamma-admin"]);
    }

    #[test]
    fn a_typo_fails_loudly_instead_of_matching_nothing_silently() {
        // Returning an empty set here would look like "no results found",
        // which reads as a clean bill of health rather than a mistake.
        let n = names();
        let err = resolve("beta-powr", &n).unwrap_err();
        assert_eq!(err.kind(), "no_profile_match");
        assert!(err.to_string().contains("beta-powr"));
    }

    #[test]
    fn a_glob_matching_nothing_also_fails() {
        let n = names();
        assert!(resolve("nonexistent-*", &n).is_err());
    }

    #[test]
    fn sso_profiles_are_detected_from_their_own_section() {
        assert!(is_sso(CONFIG, "alpha-readonly"));
        assert!(is_sso(CONFIG, "beta-power"));
    }

    #[test]
    fn a_profile_without_sso_keys_is_not_sso() {
        // `gamma-admin` here has only `region`, while the profile directly
        // above it has `sso_session` -- so this also proves section state is
        // reset at each header rather than leaking downward.
        assert!(!is_sso(CONFIG, "gamma-admin"));
        assert!(!is_sso(CONFIG, "default"));
        assert!(!is_sso(CONFIG, "nonexistent"));
    }

    #[test]
    fn section_text_captures_what_decides_the_principal() {
        let t = section_text(CONFIG, "alpha-readonly");
        assert!(t.contains("sso_session = example-sso"));
        assert!(t.contains("sso_role_name = ReadOnly"));
        // Only this profile's own section.
        assert!(!t.contains("prod"));
    }

    #[test]
    fn section_text_changes_when_the_role_changes() {
        // Re-pointing a profile at a different role must produce a different
        // fingerprint, or a cached credential outlives the change.
        let before = section_text(CONFIG, "alpha-readonly");
        let after = section_text(&CONFIG.replace("ReadOnly", "AdminAccess"), "alpha-readonly");
        assert_ne!(before, after);
    }

    #[test]
    fn section_text_is_insensitive_to_line_order() {
        let reordered = CONFIG.replace(
            "[profile alpha-readonly]\nsso_session = example-sso\nsso_role_name = ReadOnly",
            "[profile alpha-readonly]\nsso_role_name = ReadOnly\nsso_session = example-sso",
        );
        assert_eq!(
            section_text(CONFIG, "alpha-readonly"),
            section_text(&reordered, "alpha-readonly")
        );
    }

    #[test]
    fn an_absent_profile_has_an_empty_section() {
        assert_eq!(section_text(CONFIG, "nonexistent"), "");
    }

    #[test]
    fn empty_config_yields_no_profiles() {
        assert!(parse_profile_names("").is_empty());
        assert!(resolve("*", &[]).is_err());
    }
}
