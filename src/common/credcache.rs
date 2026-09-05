//! On-disk cache of resolved AWS credentials.
//!
//! # Why this exists
//!
//! The SDK caches resolved credentials in-process only, so every `awsdiag`
//! invocation performed a fresh `sso:GetRoleCredentials` round trip. Measured
//! against a live SSO profile that made a call ~240 ms slower than the same
//! call through the AWS CLI, which reuses resolved credentials it keeps in
//! `~/.aws/cli/cache/`. For a tool whose premise is being faster, paying a
//! network round trip per process is the wrong trade.
//!
//! # What lands on disk
//!
//! Short-lived STS credentials — the same class of material the AWS CLI
//! already caches, and with the same protection: directory `0700`, file
//! `0600`, owner-only. Nothing here is a long-lived secret, and an entry is
//! refused once expired.
//!
//! Set `AWSDIAG_NO_CACHE=1` to disable reading and writing entirely.
//!
//! Every failure path degrades to "no cache": a corrupt, unreadable or
//! truncated entry causes a normal credential fetch rather than an error. A
//! cache is an optimisation, and it must never be the reason a diagnostic
//! command fails during an incident.

use aws_credential_types::Credentials;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Refuse an entry this close to its expiry, so credentials cannot lapse
/// mid-request on a slow call.
const EXPIRY_SKEW: Duration = Duration::seconds(120);

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// Absent means non-expiring, which is never cached — see `store`.
    pub expires_at: Option<DateTime<Utc>>,
}

impl Entry {
    pub fn is_usable_at(&self, now: DateTime<Utc>) -> bool {
        match self.expires_at {
            Some(exp) => exp > now + EXPIRY_SKEW,
            None => false,
        }
    }
}

fn disabled() -> bool {
    if std::env::var("AWSDIAG_NO_CACHE").is_ok_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    // Ambient credentials in the environment override profile configuration
    // entirely, so a profile-keyed entry would describe a principal that is
    // not the one in use. Bypass rather than answer from a stale key.
    ["AWS_ACCESS_KEY_ID", "AWS_SESSION_TOKEN"]
        .iter()
        .any(|v| std::env::var(v).is_ok_and(|s| !s.is_empty()))
}

/// `$XDG_STATE_HOME/awsdiag/creds`, falling back to `~/.local/state`.
///
/// Returns `None` rather than a relative path when neither variable yields an
/// absolute location. An earlier version fell back to `PathBuf::from("")`,
/// which produced the *relative* path `.local/state/awsdiag/creds` when `HOME`
/// was unset — writing credentials into whatever the current directory
/// happened to be, such as a checked-out repository. Callers treat `None` as
/// "no cache", which is always a safe outcome.
pub fn cache_dir_opt() -> Option<PathBuf> {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => match std::env::var("HOME") {
            Ok(h) if !h.is_empty() => PathBuf::from(h).join(".local/state"),
            _ => return None,
        },
    };
    // A relative base is never acceptable for credential material.
    base.is_absolute()
        .then(|| base.join("awsdiag").join("creds"))
}

/// Convenience for display and for callers that tolerate a missing cache.
pub fn cache_dir() -> PathBuf {
    cache_dir_opt().unwrap_or_default()
}

/// Identity of a cache entry.
///
/// **Not** the profile name alone. The name says nothing about which
/// principal it resolves to, and `config_for` *replaces* the credential
/// provider chain on a hit — so keying on the name alone means a profile
/// re-pointed from an admin role to a read-only one keeps using the old,
/// higher-privileged credentials until the entry expires, silently. Two
/// config files that both define `[profile prod]` for different accounts
/// collide the same way, and `whoami` — whose whole job is answering "which
/// account am I pointed at" — would answer wrong.
///
/// So the key covers the config file path, the profile name, and the
/// profile's own section, which is where everything that decides the
/// principal lives.
pub fn key_for(profile: &str, config_path: &Path, section: &str) -> CacheKey {
    CacheKey(stable_hash(&format!(
        "{}\0{profile}\0{section}",
        config_path.display()
    )))
}

/// A cache entry's filename stem.
///
/// A newtype rather than a `String` so it cannot be constructed from
/// arbitrary text. `entry_path` interpolates it into a path, and a profile
/// may legitimately be named `../../etc/passwd`; making `key_for` the only
/// constructor means the hashing step cannot be skipped by accident at a
/// future call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey(String);

impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Path for one cache entry.
///
/// The filename is a hash, never a name: profile names may contain `/` or
/// `..`, and interpolating one into a path is a traversal waiting to happen.
pub fn entry_path(key: &CacheKey) -> Option<PathBuf> {
    Some(cache_dir_opt()?.join(format!("{key}.json")))
}

/// FNV-1a. Only needs to be stable and collision-resistant enough to key a
/// local cache; it is not protecting anything.
fn stable_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}")
}

/// Load a usable entry, or `None` for any reason at all.
pub fn load(key: &CacheKey, now: DateTime<Utc>) -> Option<Entry> {
    if disabled() {
        return None;
    }
    let text = std::fs::read_to_string(entry_path(key)?).ok()?;
    let entry: Entry = serde_json::from_str(&text).ok()?;
    entry.is_usable_at(now).then_some(entry)
}

/// Persist credentials for `profile`.
///
/// Credentials with no expiry are not cached: without one there is no way to
/// know when the entry became wrong, and a stale entry is worse than none.
/// Errors are swallowed deliberately — see the module note.
pub fn store(key: &CacheKey, creds: &Credentials) {
    if disabled() {
        return;
    }
    let Some(expiry) = creds.expiry() else { return };
    let entry = Entry {
        access_key_id: creds.access_key_id().to_string(),
        secret_access_key: creds.secret_access_key().to_string(),
        session_token: creds.session_token().map(ToString::to_string),
        expires_at: Some(DateTime::<Utc>::from(expiry)),
    };
    let Some(path) = entry_path(key) else {
        return;
    };
    let _ = write_private(&path, &entry);
}

/// Write owner-only, creating the directory owner-only too.
///
/// The file is written to a temporary path and renamed, so a concurrent
/// reader sees either the old entry or the new one, never a half-written
/// file. Permissions are set before any credential material is written.
fn write_private(path: &Path, entry: &Entry) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;

    // `create_new` rather than `create`: it fails if the path already exists,
    // so a pre-placed symlink cannot redirect credential material somewhere
    // else. A stale temp file from a crashed process is cleared and retried
    // once rather than blocking the cache forever.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let open = |p: &Path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(p)
    };
    let mut f = match open(&tmp) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&tmp)?;
            open(&tmp)?
        }
        Err(e) => return Err(e),
    };
    let json = serde_json::to_vec(entry).map_err(std::io::Error::other)?;
    f.write_all(&json)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// Remove every cached entry. Backs `awsdiag cache clear`.
pub fn clear() -> std::io::Result<usize> {
    let Some(dir) = cache_dir_opt() else {
        return Ok(0);
    };
    let mut removed = 0;
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            for e in entries.flatten() {
                // Temp files too. A process killed mid-write leaves a
                // `.tmp<pid>` holding real credential material, which the
                // json-only filter left behind and `cache status` never
                // showed. "Clear" has to mean clear.
                let path = e.path();
                let evictable = path
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| x == "json" || x.starts_with("tmp"));
                if evictable {
                    std::fs::remove_file(&path)?;
                    removed += 1;
                }
            }
            Ok(removed)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Mutex, MutexGuard};

    /// Environment variables are process-global while tests run in parallel
    /// threads, so a test that clears HOME can break one that reads it.
    /// Measured at roughly 2 failures per 15 runs before this lock existed.
    static ENV: Mutex<()> = Mutex::new(());

    /// Poisoning only means another test panicked while holding the lock; the
    /// guarded data is a unit, so recovering is correct rather than masking.
    fn env_lock() -> MutexGuard<'static, ()> {
        ENV.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn entry(mins: i64) -> Entry {
        Entry {
            access_key_id: "AKIAEXAMPLE".into(),
            secret_access_key: "secret".into(),
            session_token: Some("token".into()),
            expires_at: Some(now() + Duration::minutes(mins)),
        }
    }

    #[test]
    fn a_fresh_entry_is_usable() {
        assert!(entry(60).is_usable_at(now()));
    }

    #[test]
    fn an_expired_entry_is_refused() {
        assert!(!entry(-1).is_usable_at(now()));
    }

    #[test]
    fn an_entry_inside_the_skew_window_is_refused() {
        // Expiring in 60s would very likely lapse mid-request; the 120s skew
        // is what stops a "valid" cache hit from failing the call it serves.
        assert!(!entry(1).is_usable_at(now()));
        assert!(entry(3).is_usable_at(now()));
    }

    #[test]
    fn an_entry_without_an_expiry_is_never_usable() {
        let mut e = entry(60);
        e.expires_at = None;
        assert!(!e.is_usable_at(now()));
    }

    fn key(profile: &str) -> CacheKey {
        key_for(profile, Path::new("/home/u/.aws/config"), "sso_session = s")
    }

    #[test]
    fn the_filename_is_a_hash_so_a_profile_name_cannot_escape_the_directory() {
        let _guard = env_lock();
        // A profile literally named "../../etc/passwd" must not resolve
        // outside the cache directory. `CacheKey` makes hashing unskippable.
        let p = entry_path(&key("../../etc/passwd")).expect("HOME is set in tests");
        assert_eq!(p.parent().unwrap(), cache_dir());
        assert!(!p.to_string_lossy().contains(".."));
    }

    #[test]
    fn the_key_covers_more_than_the_profile_name() {
        // Two config files each defining `[profile prod]` for different
        // accounts must not share an entry: `config_for` replaces the whole
        // provider chain on a hit, so a collision means authenticating as the
        // wrong principal while reporting the right profile name.
        let a = key_for("prod", Path::new("/a/config"), "sso_account_id = 111");
        let b = key_for("prod", Path::new("/b/config"), "sso_account_id = 111");
        assert_ne!(a, b, "config file path must be part of the key");

        // Re-pointing a profile at a different role must invalidate too, or
        // the old higher-privileged credentials keep being used until expiry.
        let ro = key_for("prod", Path::new("/a/config"), "sso_role_name = ReadOnly");
        let admin = key_for("prod", Path::new("/a/config"), "sso_role_name = Admin");
        assert_ne!(ro, admin, "the resolved role must be part of the key");

        // Unchanged configuration keeps hitting the same entry.
        assert_eq!(
            key_for("prod", Path::new("/a/config"), "sso_role_name = ReadOnly"),
            key_for("prod", Path::new("/a/config"), "sso_role_name = ReadOnly")
        );
    }

    #[test]
    fn ambient_environment_credentials_bypass_the_cache() {
        // Environment credentials override profile configuration, so a
        // profile-keyed entry would describe a principal that is not in use.
        let _guard = env_lock();
        let prev = std::env::var("AWS_ACCESS_KEY_ID").ok();
        unsafe { std::env::set_var("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE") };
        assert!(load(&key("prod"), now()).is_none());
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AWS_ACCESS_KEY_ID", v),
                None => std::env::remove_var("AWS_ACCESS_KEY_ID"),
            }
        }
    }

    #[test]
    fn different_profiles_get_different_files() {
        let _guard = env_lock();
        assert_ne!(
            entry_path(&key("alpha-power")),
            entry_path(&key("beta-power"))
        );
        assert_eq!(
            entry_path(&key("beta-power")),
            entry_path(&key("beta-power"))
        );
    }

    #[test]
    fn a_written_entry_round_trips_and_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("awsdiag-ct-{}", std::process::id()));
        let path = dir.join("e.json");
        let e = entry(60);
        write_private(&path, &e).expect("write");

        let back: Entry = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, e);

        // Credentials on disk readable by other local users would be a real
        // exposure, so the mode is asserted rather than assumed.
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );

        // No temporary file is left holding credential material.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|f| f.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_and_missing_entries_read_as_a_miss_not_an_error() {
        let _guard = env_lock();
        // A cache must never be the reason a command fails mid-incident.
        let dir = std::env::temp_dir().join(format!("awsdiag-ct2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.json");
        std::fs::write(&path, b"{not json").unwrap();
        assert!(serde_json::from_str::<Entry>(&std::fs::read_to_string(&path).unwrap()).is_err());
        std::fs::remove_dir_all(&dir).ok();

        // A profile that was never cached simply misses.
        assert!(load(&key("profile-that-was-never-cached-xyz"), now()).is_none());
    }

    #[test]
    fn no_cache_dir_is_resolved_when_the_environment_gives_no_absolute_home() {
        let _guard = env_lock();
        // Regression guard. This previously yielded the relative path
        // `.local/state/awsdiag/creds`, writing credentials into whatever the
        // current working directory happened to be.
        let (xdg, home) = (
            std::env::var("XDG_STATE_HOME").ok(),
            std::env::var("HOME").ok(),
        );
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
            std::env::remove_var("HOME");
        }
        assert_eq!(cache_dir_opt(), None);
        assert_eq!(entry_path(&key("any-profile")), None);

        // A relative XDG_STATE_HOME is refused for the same reason.
        unsafe { std::env::set_var("XDG_STATE_HOME", "relative/path") };
        assert_eq!(cache_dir_opt(), None);

        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
            if let Some(x) = xdg {
                std::env::set_var("XDG_STATE_HOME", x);
            }
            if let Some(h) = home {
                std::env::set_var("HOME", h);
            }
        }
    }

    #[test]
    fn a_preplaced_file_at_the_temp_path_cannot_capture_the_write() {
        // create_new refuses an existing path, so a symlink planted at the
        // temp name cannot redirect credentials. The stale entry is cleared
        // and the write retried once.
        let dir = std::env::temp_dir().join(format!("awsdiag-ct3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("e.json");
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, b"stale").unwrap();

        write_private(&path, &entry(60)).expect("retry after clearing the stale temp file");
        assert!(path.exists());
        assert!(!tmp.exists(), "temp file cleaned up");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_cache_dir_follows_xdg_state_home() {
        let _guard = env_lock();
        let prev = std::env::var("XDG_STATE_HOME").ok();
        unsafe { std::env::set_var("XDG_STATE_HOME", "/tmp/awsdiag-xdg") };
        assert_eq!(
            cache_dir_opt().unwrap(),
            PathBuf::from("/tmp/awsdiag-xdg/awsdiag/creds")
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_STATE_HOME", v),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
    }
}
