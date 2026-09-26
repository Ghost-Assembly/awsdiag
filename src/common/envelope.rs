//! The uniform output envelope every subcommand emits.
//!
//! One shape for every command means a consumer never has to learn a
//! per-command result format. Two fields carry more weight than they look:
//!
//! - `truncated` says the result is incomplete. Without it a capped query is
//!   indistinguishable from an exhaustive one, and a diagnosis gets built on
//!   a partial picture with no indication anything is missing.
//! - `count` is derived from the data, never passed in, so it cannot drift
//!   from what was actually returned.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct Envelope<T: Serialize> {
    /// Always `true`. Present so success and failure share one discriminant.
    pub ok: bool,
    pub command: String,
    pub params: Value,
    pub count: usize,
    pub truncated: bool,
    /// Reserved for a resume token. No command can resume a partial result
    /// yet, so this is always `null`; `truncated` is what says data is
    /// missing.
    pub next: Option<String>,
    pub data: Vec<T>,
}

impl<T: Serialize> Envelope<T> {
    pub fn new(command: impl Into<String>, params: Value, data: Vec<T>) -> Self {
        Self {
            ok: true,
            command: command.into(),
            params,
            count: data.len(),
            truncated: false,
            next: None,
            data,
        }
    }

    /// Mark the result as incomplete. Call this whenever a limit was hit or a
    /// page boundary was left unread.
    pub fn truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }
}

/// The failure counterpart. `hint` is the actionable half: it carries the
/// command that fixes the problem, so a caller can recover without guessing.
#[derive(Debug, Serialize)]
pub struct ErrorEnvelope {
    /// Always `false`.
    pub ok: bool,
    pub command: String,
    pub kind: &'static str,
    pub error: String,
    pub hint: Option<String>,
}

impl ErrorEnvelope {
    pub fn new(command: impl Into<String>, err: &crate::common::errors::Error) -> Self {
        Self {
            ok: false,
            command: command.into(),
            kind: err.kind(),
            error: err.to_string(),
            hint: err.hint(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::errors::Error;
    use serde_json::json;

    fn env(data: Vec<&'static str>) -> Envelope<&'static str> {
        Envelope::new("logs scan", json!({"group": "/aws/lambda/x"}), data)
    }

    #[test]
    fn count_is_derived_from_the_data_it_describes() {
        assert_eq!(env(vec!["a", "b", "c"]).count, 3);
        assert_eq!(env(vec![]).count, 0);
    }

    #[test]
    fn a_complete_result_is_not_marked_truncated() {
        let e = env(vec!["a"]);
        assert!(e.ok);
        assert!(!e.truncated);
        assert!(e.next.is_none());
    }

    #[test]
    fn success_serializes_with_every_contract_field_present() {
        let v = serde_json::to_value(env(vec!["a", "b"])).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["command"], json!("logs scan"));
        assert_eq!(v["count"], json!(2));
        assert_eq!(v["truncated"], json!(false));
        assert_eq!(v["next"], json!(null));
        assert_eq!(v["data"], json!(["a", "b"]));
        assert_eq!(v["params"]["group"], json!("/aws/lambda/x"));
    }

    #[test]
    fn failure_serializes_with_ok_false_and_carries_the_hint() {
        let err = Error::Auth {
            profile: "beta-power".into(),
            sso: true,
        };
        let v = serde_json::to_value(ErrorEnvelope::new("logs scan", &err)).unwrap();
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["kind"], json!("auth"));
        assert!(v["error"].as_str().unwrap().contains("beta-power"));
        assert_eq!(v["hint"], json!("run `aws sso login --profile beta-power`"));
    }
}
