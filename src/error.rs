//! CLI error type and its mapping to process exit codes + the machine-readable
//! `error.kind` shown in `--json` output.
//!
//! Errors fail loud: every variant maps to a non-zero exit and a clear message;
//! retry-relevant cases (conflict, network) get distinct exit codes.

use hydrate_wire::apis::Error as WireError;

use crate::exit;

#[derive(Debug)]
pub enum CliError {
    /// `HYD_API_KEY` is not set.
    MissingApiKey,
    /// Transport failure reaching the service (connect/timeout). Retryable.
    Network(String),
    /// Optimistic-concurrency conflict — the branch moved (409). Retryable.
    VersionConflict { current_version: Option<i64> },
    /// The service returned an error response (non-409).
    Service {
        status: u16,
        kind: String,
        reason: Option<String>,
    },
    /// Anything else (a bug, an unexpected response).
    Other(String),
}

impl CliError {
    /// Process exit code.
    pub fn exit_code(&self) -> u8 {
        match self {
            CliError::Network(_) => exit::NETWORK,
            CliError::VersionConflict { .. } => exit::CONFLICT,
            CliError::Service { status: 409, .. } => exit::CONFLICT,
            _ => exit::GENERIC,
        }
    }

    /// Stable machine token for `--json` output; clients switch over this.
    pub fn kind(&self) -> &str {
        match self {
            CliError::MissingApiKey => "missing_api_key",
            CliError::Network(_) => "network",
            CliError::VersionConflict { .. } => "version_conflict",
            CliError::Service { kind, .. } => kind,
            CliError::Other(_) => "error",
        }
    }

    /// Human-readable, actionable message.
    pub fn message(&self) -> String {
        match self {
            CliError::MissingApiKey => {
                "HYD_API_KEY is not set; export it or put it in a .env file".to_string()
            }
            CliError::Network(detail) => format!("could not reach the service: {detail}"),
            CliError::VersionConflict {
                current_version: Some(v),
            } => {
                format!("version conflict: the branch is now at version {v}; re-fetch and retry")
            }
            CliError::VersionConflict {
                current_version: None,
            } => "version conflict: the branch moved; re-fetch and retry".to_string(),
            CliError::Service {
                status,
                reason: Some(r),
                ..
            } => format!("service error ({status}): {r}"),
            CliError::Service { status, .. } => format!("service error ({status})"),
            CliError::Other(detail) => detail.clone(),
        }
    }
}

/// Map the generated client's error onto our typed error.
impl<T> From<WireError<T>> for CliError {
    fn from(err: WireError<T>) -> Self {
        match err {
            WireError::Reqwest(re) => {
                if re.is_connect() || re.is_timeout() {
                    CliError::Network(re.to_string())
                } else {
                    CliError::Other(format!("request failed: {re}"))
                }
            }
            WireError::ResponseError(rc) => {
                let status = rc.status.as_u16();
                let (kind, reason, current_version) = parse_detail(&rc.content);
                if status == 409 {
                    CliError::VersionConflict { current_version }
                } else {
                    CliError::Service {
                        status,
                        kind: kind.unwrap_or_else(|| "service_error".to_string()),
                        reason,
                    }
                }
            }
            WireError::Serde(e) => CliError::Other(format!("could not parse the response: {e}")),
            WireError::Io(e) => CliError::Other(format!("io error: {e}")),
        }
    }
}

/// Best-effort extraction of `(error.kind, reason, current_version)` from the
/// `{"detail": {...}}` error envelope. A body that doesn't parse yields no extra
/// detail — the HTTP status still drives the exit code, so nothing is swallowed.
fn parse_detail(body: &str) -> (Option<String>, Option<String>, Option<i64>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return (None, None, None);
    };
    let detail = v.get("detail").unwrap_or(&v);
    let kind = detail
        .get("error")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let reason = detail
        .get("reason")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let current_version = detail
        .get("current_version")
        .and_then(serde_json::Value::as_i64);
    (kind, reason, current_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_retry_relevant() {
        assert_eq!(CliError::MissingApiKey.exit_code(), exit::GENERIC);
        assert_eq!(CliError::Network("x".into()).exit_code(), exit::NETWORK);
        assert_eq!(
            CliError::VersionConflict {
                current_version: Some(3)
            }
            .exit_code(),
            exit::CONFLICT
        );
        assert_eq!(
            CliError::Service {
                status: 409,
                kind: "version_conflict".into(),
                reason: None
            }
            .exit_code(),
            exit::CONFLICT
        );
        assert_eq!(
            CliError::Service {
                status: 422,
                kind: "malformed_delta_field".into(),
                reason: None
            }
            .exit_code(),
            exit::GENERIC
        );
    }

    #[test]
    fn parse_detail_reads_the_envelope() {
        let body = r#"{"detail":{"error":"version_conflict","current_version":7}}"#;
        let (kind, _reason, cv) = parse_detail(body);
        assert_eq!(kind.as_deref(), Some("version_conflict"));
        assert_eq!(cv, Some(7));
    }

    #[test]
    fn parse_detail_tolerates_garbage() {
        assert_eq!(parse_detail("not json"), (None, None, None));
    }

    #[test]
    fn missing_key_kind_is_stable() {
        assert_eq!(CliError::MissingApiKey.kind(), "missing_api_key");
    }
}
