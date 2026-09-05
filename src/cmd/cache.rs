//! `awsdiag cache` — inspect and clear the credential cache.
//!
//! Credentials on disk should never be a black box. `status` says what is
//! stored and when it expires; `clear` removes it. Entries are keyed by a
//! hash of the profile name, so `status` reports what it can see without
//! claiming to know which profile an entry belongs to.

use crate::common::credcache;
use crate::common::envelope::Envelope;
use crate::common::errors::Error;
use crate::common::progress::Progress;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Serialize)]
pub struct CacheEntry {
    pub file: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub usable: bool,
}

pub fn status(now: DateTime<Utc>, progress: &Progress) -> Result<Envelope<CacheEntry>, Error> {
    progress.phase("reading credential cache");
    let Some(dir) = credcache::cache_dir_opt() else {
        return Ok(Envelope::new(
            "cache status",
            json!({ "dir": null }),
            Vec::new(),
        ));
    };
    let mut rows = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            // A malformed entry is still worth reporting: it explains why a
            // profile keeps missing the cache.
            let parsed = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<credcache::Entry>(&t).ok());
            rows.push(CacheEntry {
                file: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                expires_at: parsed.as_ref().and_then(|p| p.expires_at),
                usable: parsed.as_ref().is_some_and(|p| p.is_usable_at(now)),
            });
        }
    }
    rows.sort_by(|a, b| a.file.cmp(&b.file));
    let usable = rows.iter().filter(|r| r.usable).count();
    progress.finish(format!("{} entries, {usable} usable", rows.len()));
    Ok(Envelope::new(
        "cache status",
        json!({ "dir": dir.to_string_lossy() }),
        rows,
    ))
}

#[derive(Debug, Serialize)]
pub struct Cleared {
    pub removed: usize,
    pub dir: String,
}

pub fn clear(progress: &Progress) -> Result<Envelope<Cleared>, Error> {
    progress.phase("clearing credential cache");
    let dir = credcache::cache_dir().to_string_lossy().into_owned();
    let removed = credcache::clear()?;
    progress.finish(format!("removed {removed} entries"));
    Ok(Envelope::new(
        "cache clear",
        json!({}),
        vec![Cleared { removed, dir }],
    ))
}
