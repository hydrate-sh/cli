//! Dual output: human-readable on a TTY, JSON when piped. `--json` / `--human`
//! override. Both modes carry the same information.
//!
//! Consumed by the command handlers as the verbs are implemented; until then the
//! renderers are exercised by this module's tests only.
#![allow(dead_code)]

use std::io::IsTerminal;

use crate::error::CliError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Human,
    Json,
}

impl OutputMode {
    /// Resolve from the global flags plus whether stdout is a TTY. `--json` wins
    /// over `--human` (clap also makes them mutually exclusive); with neither,
    /// a TTY gets human output and a pipe gets JSON.
    pub fn resolve(json: bool, human: bool, stdout_is_tty: bool) -> OutputMode {
        if json {
            OutputMode::Json
        } else if human || stdout_is_tty {
            OutputMode::Human
        } else {
            OutputMode::Json
        }
    }

    /// Resolve against the real stdout.
    pub fn from_flags(json: bool, human: bool) -> OutputMode {
        Self::resolve(json, human, std::io::stdout().is_terminal())
    }
}

/// The JSON error envelope: `{"error": {"kind": ..., "message": ...}}`. Clients
/// switch over `error.kind`; both fields are always present.
pub fn error_json(err: &CliError) -> serde_json::Value {
    serde_json::json!({ "error": { "kind": err.kind(), "message": err.to_string() } })
}

/// Render an error to stderr in the selected mode, with a stable `error.kind`.
pub fn print_error(err: &CliError, mode: OutputMode) {
    match mode {
        OutputMode::Json => {
            // serde_json::to_string on this fixed shape cannot fail; if it ever
            // did, fall back to a minimal valid envelope rather than panicking.
            let line = serde_json::to_string(&error_json(err))
                .unwrap_or_else(|_| r#"{"error":{"kind":"error"}}"#.to_string());
            eprintln!("{line}");
        }
        OutputMode::Human => eprintln!("hydrate: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_flags_win() {
        assert_eq!(OutputMode::resolve(true, false, true), OutputMode::Json);
        assert_eq!(OutputMode::resolve(false, true, false), OutputMode::Human);
    }

    #[test]
    fn tty_defaults_to_human_pipe_to_json() {
        assert_eq!(OutputMode::resolve(false, false, true), OutputMode::Human);
        assert_eq!(OutputMode::resolve(false, false, false), OutputMode::Json);
    }

    #[test]
    fn json_envelope_carries_kind_and_message() {
        let v = error_json(&CliError::MissingApiKey);
        assert_eq!(v["error"]["kind"], "missing_api_key");
        assert_eq!(v["error"]["message"], CliError::MissingApiKey.to_string());
        // a service error passes its kind through
        let svc = CliError::Service {
            status: 422,
            kind: "malformed_delta_field".into(),
            reason: None,
        };
        assert_eq!(error_json(&svc)["error"]["kind"], "malformed_delta_field");
    }
}
