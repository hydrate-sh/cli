//! CLI error type and its mapping to process exit codes + the machine-readable
//! `error.kind` shown in `--json` output.
//!
//! Errors fail loud: every variant maps to a non-zero exit and a clear message;
//! retry-relevant cases (conflict, network) get distinct exit codes.

use std::fmt;

use hydrate_wire::apis::Error as WireError;

use crate::exit;

#[derive(Debug)]
pub enum CliError {
    /// `HYD_API_KEY` is not set.
    MissingApiKey,
    /// `HYD_BASE_URL` could not be parsed (or has an unsupported scheme).
    InvalidBaseUrl(String),
    /// `HYD_BASE_URL` would send credentials over plaintext to a non-local host.
    InsecureBaseUrl(String),
    /// Transport failure reaching the service (connect/timeout/DNS/TLS). Retryable.
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
            // Defensive: the `From<WireError>` impl routes all 409s to
            // `VersionConflict`, so a `Service` carrying 409 only arises if one
            // is constructed directly — keep it mapping to CONFLICT regardless.
            CliError::Service { status: 409, .. } => exit::CONFLICT,
            _ => exit::GENERIC,
        }
    }

    /// Stable machine token for `--json` output; clients switch over this.
    pub fn kind(&self) -> &str {
        match self {
            CliError::MissingApiKey => "missing_api_key",
            CliError::InvalidBaseUrl(_) => "invalid_base_url",
            CliError::InsecureBaseUrl(_) => "insecure_base_url",
            CliError::Network(_) => "network",
            CliError::VersionConflict { .. } => "version_conflict",
            CliError::Service { kind, .. } => kind,
            CliError::Other(_) => "error",
        }
    }
}

impl fmt::Display for CliError {
    /// Human-readable, actionable message (also what `--json` carries).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::MissingApiKey => {
                write!(f, "HYD_API_KEY is not set; export it or put it in a .env file")
            }
            CliError::InvalidBaseUrl(detail) => write!(f, "invalid HYD_BASE_URL: {detail}"),
            CliError::InsecureBaseUrl(url) => write!(
                f,
                "refusing to send credentials over plaintext http to a non-local host ({url}); use https"
            ),
            CliError::Network(detail) => write!(f, "could not reach the service: {detail}"),
            CliError::VersionConflict { current_version: Some(v) } => {
                write!(f, "version conflict: the branch is now at version {v}; re-fetch and retry")
            }
            CliError::VersionConflict { current_version: None } => {
                write!(f, "version conflict: the branch moved; re-fetch and retry")
            }
            CliError::Service { status, reason: Some(r), .. } => write!(f, "service error ({status}): {r}"),
            CliError::Service { status, .. } => write!(f, "service error ({status})"),
            CliError::Other(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for CliError {}

/// Map the generated client's error onto our typed error.
impl<T> From<WireError<T>> for CliError {
    fn from(err: WireError<T>) -> Self {
        match err {
            WireError::Reqwest(re) => {
                // Transport-layer failures (connect, DNS, TLS handshake, timeout,
                // request/body transport) are retryable network errors; a decode
                // or builder error is not.
                if re.is_connect() || re.is_timeout() || re.is_request() || re.is_body() {
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
    use hydrate_wire::apis::ResponseContent;
    use reqwest::StatusCode;

    fn response_error(status: u16, body: &str) -> CliError {
        // T is irrelevant for the mapping (it reads status + raw content).
        let rc = ResponseContent::<()> {
            status: StatusCode::from_u16(status).unwrap(),
            content: body.to_string(),
            entity: None,
        };
        CliError::from(WireError::ResponseError(rc))
    }

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
            CliError::InsecureBaseUrl("http://x".into()).exit_code(),
            exit::GENERIC
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
    fn response_409_maps_to_version_conflict() {
        let e = response_error(
            409,
            r#"{"detail":{"error":"version_conflict","current_version":7}}"#,
        );
        match e {
            CliError::VersionConflict { current_version } => assert_eq!(current_version, Some(7)),
            other => panic!("expected VersionConflict, got {other:?}"),
        }
        assert_eq!(response_error(409, "{}").exit_code(), exit::CONFLICT);
    }

    #[test]
    fn response_non_409_maps_to_service_with_kind() {
        let e = response_error(
            422,
            r#"{"detail":{"error":"malformed_delta_field","reason":"bad type"}}"#,
        );
        match e {
            CliError::Service {
                status,
                kind,
                reason,
            } => {
                assert_eq!(status, 422);
                assert_eq!(kind, "malformed_delta_field");
                assert_eq!(reason.as_deref(), Some("bad type"));
            }
            other => panic!("expected Service, got {other:?}"),
        }
    }

    #[test]
    fn service_kind_passes_through_for_json() {
        let e = CliError::Service {
            status: 422,
            kind: "malformed_delta_field".into(),
            reason: None,
        };
        assert_eq!(e.kind(), "malformed_delta_field");
    }

    #[test]
    fn messages_are_actionable() {
        assert!(CliError::MissingApiKey.to_string().contains("HYD_API_KEY"));
        assert!(CliError::InsecureBaseUrl("http://x".into())
            .to_string()
            .contains("https"));
        assert!(CliError::VersionConflict {
            current_version: Some(7)
        }
        .to_string()
        .contains('7'));
        assert!(CliError::Service {
            status: 422,
            kind: "k".into(),
            reason: Some("why".into())
        }
        .to_string()
        .contains("why"));
    }

    #[test]
    fn parse_detail_tolerates_garbage() {
        assert_eq!(parse_detail("not json"), (None, None, None));
    }
}
