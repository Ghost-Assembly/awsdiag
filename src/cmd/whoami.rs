//! `awsdiag whoami` — resolve identity for the selected profile(s).
//!
//! Phase 0's end-to-end proof: it exercises profile resolution, glob fan-out,
//! parallel execution, SDK configuration, error translation and the envelope
//! in one call. It is also the fastest way to answer "are my credentials
//! live, and which account am I actually pointed at" before a longer query
//! fails halfway through.

use crate::common::aws;
use crate::common::envelope::Envelope;
use crate::common::errors::Error;
use crate::common::flags::TargetArgs;
use crate::common::progress::Progress;
use futures::future::join_all;
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Serialize)]
pub struct Identity {
    pub profile: String,
    pub account: Option<String>,
    pub arn: Option<String>,
    pub user_id: Option<String>,
    pub region: Option<String>,
    /// `None` when the lookup succeeded. Populated per row so one dead profile
    /// in a fan-out does not discard the results from the live ones.
    pub error: Option<String>,
    pub hint: Option<String>,
}

pub async fn run(target: &TargetArgs, progress: &Progress) -> Result<Envelope<Identity>, Error> {
    let profiles = aws::resolve_targets(target)?;
    progress.phase(format!(
        "resolving identity for {} profile{}",
        profiles.len(),
        if profiles.len() == 1 { "" } else { "s" }
    ));

    // Read once, not per profile: the answer decides which recovery command
    // an auth failure suggests.
    let config = std::fs::read_to_string(aws::config_path()).unwrap_or_default();
    let lookups = profiles.iter().map(|p| {
        let task = progress.task(p.clone());
        let sso = crate::common::profiles::is_sso(&config, p);
        async move {
            let row = identity(p, target.region.as_deref(), sso).await;
            match (&row.error, &row.account) {
                (None, Some(account)) => task.done(format!("{p}  {account}")),
                _ => task.failed(format!("{p}  unavailable")),
            }
            row
        }
    });
    let rows: Vec<Identity> = join_all(lookups).await;
    let ok = rows.iter().filter(|r| r.error.is_none()).count();
    progress.finish(format!("resolved {ok}/{} profiles", rows.len()));

    Ok(Envelope::new(
        "whoami",
        json!({ "selector": target.selector(), "region": target.region }),
        rows,
    ))
}

async fn identity(profile: &str, region: Option<&str>, sso: bool) -> Identity {
    let cfg = aws::config_for(profile, region).await;
    let resolved_region = cfg.region().map(ToString::to_string);
    let client = aws_sdk_sts::Client::new(&cfg);

    match client.get_caller_identity().send().await {
        Ok(out) => Identity {
            profile: profile.to_string(),
            account: out.account().map(ToString::to_string),
            arn: out.arn().map(ToString::to_string),
            user_id: out.user_id().map(ToString::to_string),
            region: resolved_region,
            error: None,
            hint: None,
        },
        Err(e) => {
            let mapped = aws::map_sdk_error(e, profile, "sts:GetCallerIdentity", sso);
            Identity {
                profile: profile.to_string(),
                account: None,
                arn: None,
                user_id: None,
                region: resolved_region,
                error: Some(mapped.to_string()),
                hint: mapped.hint(),
            }
        }
    }
}
