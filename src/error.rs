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
    VersionConflict {
        current_version: Option<i64>,
    },
    /// The service returned an error response (non-409).
    Service {
        status: u16,
        kind: String,
        reason: Option<String>,
    },
    /// A `.hydrate/` workdir-state read/write/parse failure.
    State(String),
    /// A command argument failed a client-side shape check (e.g. branch name).
    InvalidArgument(String),
    /// A staging/inspection verb was run outside a bound `.hydrate/` workdir.
    /// A scoped read's `404`, translated. Distinct from `InvalidArgument` so a
    /// consumer can tell "the branch no longer has this" from "you typed a bad
    /// path" — the two want different recovery, and folding them lost that.
    StaleView(String),
    NotInWorkdir,
    /// The single-project rule found no project to act on.
    NoProject,
    /// The single-project rule found more than one project, so the target is
    /// ambiguous and must be disambiguated rather than guessed.
    AmbiguousProject {
        count: usize,
    },
    /// `init` refused to edit `AGENTS.md` to avoid destroying the user's content
    /// (a malformed/ambiguous hydrate block, or a symlink at the target).
    InitRefused(String),
    /// A `/v1` scope gate refused the request (403) on a route where we can
    /// infer WHICH scope is missing from the route itself — every `/v1` scope
    /// gate returns the same fixed `{"detail": "forbidden"}` body with no
    /// `code` or `missing_scope` field, so there is nothing in the response to
    /// key off. Callers must only construct this where the route is known to
    /// require exactly one extra scope (documented at each call site); if a
    /// route ever grows a second scope requirement this mapping becomes
    /// ambiguous and needs revisiting.
    MissingScope {
        scope: String,
    },
    /// Anything else (a bug, an unexpected response).
    Other(String),
}

impl CliError {
    /// Process exit code.
    pub fn exit_code(&self) -> u8 {
        match self {
            CliError::Network(_) => exit::NETWORK,
            // Only the retryable optimistic-concurrency conflict gets the CONFLICT
            // code. Other 409s (e.g. a branch cap or an inactive branch) are not
            // retryable and surface as `Service`, so they take the generic code.
            CliError::VersionConflict { .. } => exit::CONFLICT,
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
            CliError::State(_) => "state_error",
            CliError::InvalidArgument(_) => "invalid_argument",
            CliError::StaleView(_) => "stale_view",
            CliError::NotInWorkdir => "not_in_workdir",
            CliError::NoProject => "no_project",
            CliError::AmbiguousProject { .. } => "ambiguous_project",
            CliError::InitRefused(_) => "init_refused",
            CliError::MissingScope { .. } => "missing_scope",
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
                write!(f, "version conflict: the branch is now at version {v}; re-run the command to retry against it")
            }
            CliError::VersionConflict { current_version: None } => {
                write!(f, "version conflict: the branch moved; re-run the command to retry against it")
            }
            CliError::Service { status, reason: Some(r), .. } => write!(f, "service error ({status}): {r}"),
            CliError::Service { status, .. } => write!(f, "service error ({status})"),
            CliError::State(detail) => write!(f, "{detail}"),
            CliError::InvalidArgument(detail) => write!(f, "{detail}"),
            CliError::StaleView(m) => write!(f, "{m}"),
            CliError::NotInWorkdir => write!(
                f,
                "not inside a hydrate working copy; run `hydrate fork <name>` first"
            ),
            CliError::NoProject => write!(
                f,
                "no project found for this account; create one at https://hydrate.sh first"
            ),
            CliError::AmbiguousProject { count } => write!(
                f,
                "found {count} active projects; this command needs exactly one — \
                 pick one with `--project <name|id>` or the HYD_PROJECT environment \
                 variable (run `hydrate projects` to see the names and ids)"
            ),
            CliError::InitRefused(detail) => write!(f, "{detail}"),
            CliError::MissingScope { scope } => write!(
                f,
                "this API key was not minted with the `{scope}` scope, so it \
                 cannot do this; mint a new key that includes `{scope}` and set \
                 it as HYD_API_KEY"
            ),
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
                // A 409 is the retryable version conflict ONLY when the body says
                // so. Other 409s (a branch cap, an inactive branch) are distinct,
                // non-retryable failures and must not be dressed up as "re-fetch
                // and retry" — they pass through as `Service` with their own kind.
                if status == 409 && kind.as_deref() == Some("version_conflict") {
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
///
/// The service uses two envelope shapes for the machine-readable kind: the
/// delta/branch routes carry it as `error` (e.g. `version_conflict`), while the
/// `{code, message}` shape carries it as `code` (e.g. `name_taken`,
/// `not_found`). Both are checked so neither family of route loses its kind
/// here; `error` is tried first only because it was there first, not because
/// one takes precedence over the other in a body that (today) never carries
/// both.
///
/// The `{code, message}` shape is NOT new — per the vendored spec it already
/// applied to every `/v1` route's shared not-found envelope (branches, graph,
/// node, subtree, the projects listing), not just the project-lifecycle
/// routes this fix was written for. Before this, every one of those 404s
/// resolved `kind` to the generic `"service_error"` fallback; after it, they
/// report their real `code` (e.g. `"not_found"`) in `--json` output. That is a
/// behavior change to the stable, documented `error.kind` contract on routes
/// this otherwise unrelated to project lifecycle — every internal caller was
/// traced and none matches on `kind` for a 404 (`cmd::walk`'s remap keys on
/// `status` alone; see its own test pinning that), but an external `--json`
/// consumer that switched on `error.kind == "service_error"` for one of those
/// routes would see a different string starting here.
fn parse_detail(body: &str) -> (Option<String>, Option<String>, Option<i64>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return (None, None, None);
    };
    let detail = v.get("detail").unwrap_or(&v);
    let kind = detail
        .get("error")
        .or_else(|| detail.get("code"))
        .and_then(|x| x.as_str())
        .map(str::to_string);
    // The human-readable text rides in `reason` (the delta-error envelope) or
    // `message` (the plain HTTP-error envelope); accept either so the server's
    // actionable text is never dropped.
    let reason = detail
        .get("reason")
        .or_else(|| detail.get("message"))
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
    fn version_conflict_409_maps_to_version_conflict() {
        let e = response_error(
            409,
            r#"{"detail":{"error":"version_conflict","current_version":7}}"#,
        );
        match e {
            CliError::VersionConflict { current_version } => assert_eq!(current_version, Some(7)),
            other => panic!("expected VersionConflict, got {other:?}"),
        }
        assert_eq!(e.exit_code(), exit::CONFLICT);
    }

    #[test]
    fn non_version_conflict_409_is_service_not_retryable() {
        // A branch-cap 409 must NOT masquerade as a retryable version conflict:
        // it surfaces as a Service error with its own kind/message, exit code 1.
        let e = response_error(
            409,
            r#"{"detail":{"error":"branch_limit_reached","message":"too many branches"}}"#,
        );
        match &e {
            CliError::Service {
                status,
                kind,
                reason,
            } => {
                assert_eq!(*status, 409);
                assert_eq!(kind, "branch_limit_reached");
                // `message` (plain envelope) is surfaced as the reason.
                assert_eq!(reason.as_deref(), Some("too many branches"));
            }
            other => panic!("expected Service, got {other:?}"),
        }
        assert_eq!(e.kind(), "branch_limit_reached");
        assert_eq!(e.exit_code(), exit::GENERIC);
    }

    #[test]
    fn inactive_branch_409_is_service_not_conflict() {
        let e = response_error(409, r#"{"detail":{"error":"branch_not_active"}}"#);
        assert!(
            matches!(e, CliError::Service { status: 409, .. }),
            "got {e:?}"
        );
        assert_eq!(e.exit_code(), exit::GENERIC);
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
        // The ambiguous-project error names every escape hatch, so it is
        // self-recovering: the user can resolve it with only what it says.
        let ambiguous = CliError::AmbiguousProject { count: 3 }.to_string();
        assert!(ambiguous.contains("--project"), "{ambiguous}");
        assert!(ambiguous.contains("HYD_PROJECT"), "{ambiguous}");
        assert!(ambiguous.contains("hydrate projects"), "{ambiguous}");
    }

    #[test]
    fn init_refused_has_its_own_kind_and_generic_exit() {
        // A refusal to touch AGENTS.md is a distinct machine token (so a client
        // can tell it apart from a plain IO error) and a generic, non-retryable
        // failure.
        let e = CliError::InitRefused("AGENTS.md is a symlink; refusing".into());
        assert_eq!(e.kind(), "init_refused");
        assert_eq!(e.exit_code(), exit::GENERIC);
        assert!(e.to_string().contains("symlink"));
    }

    #[test]
    fn parse_detail_tolerates_garbage() {
        assert_eq!(parse_detail("not json"), (None, None, None));
    }

    #[test]
    fn code_keyed_envelope_maps_to_a_kind_and_message() {
        // The project routes (create/patch/delete) use `{code, message}`, not
        // the delta routes' `{error, reason}`. Before this, `kind` came back
        // `None` for every project 4xx and the CLI fell back to the useless
        // generic "service_error" token.
        let e = response_error(
            409,
            r#"{"detail":{"code":"name_taken","message":"You already have an active project with this name."}}"#,
        );
        match &e {
            CliError::Service {
                status,
                kind,
                reason,
            } => {
                assert_eq!(*status, 409);
                assert_eq!(kind, "name_taken");
                assert_eq!(
                    reason.as_deref(),
                    Some("You already have an active project with this name.")
                );
            }
            other => panic!("expected Service, got {other:?}"),
        }
        assert_eq!(e.kind(), "name_taken");
    }

    #[test]
    fn shared_not_found_envelope_maps_to_not_found_kind() {
        let e = response_error(
            404,
            r#"{"detail":{"code":"not_found","message":"Resource not found or not accessible."}}"#,
        );
        assert_eq!(e.kind(), "not_found");
    }

    #[test]
    fn missing_scope_names_the_scope_and_is_actionable() {
        let e = CliError::MissingScope {
            scope: "project:delete".to_string(),
        };
        assert_eq!(e.kind(), "missing_scope");
        assert_eq!(e.exit_code(), exit::GENERIC);
        let msg = e.to_string();
        assert!(msg.contains("project:delete"), "{msg}");
        assert!(msg.contains("HYD_API_KEY"), "{msg}");
    }
}
