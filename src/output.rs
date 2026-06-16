//! Dual output: human-readable on a TTY, JSON when piped. `--json` / `--human`
//! override. Both modes carry the same information.

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

/// Render an error to stderr in the selected mode, with a stable `error.kind`.
pub fn print_error(err: &CliError, mode: OutputMode) {
    match mode {
        OutputMode::Json => {
            let body = serde_json::json!({
                "error": { "kind": err.kind(), "message": err.message() }
            });
            // serde_json::to_string on this fixed shape cannot fail; if it ever
            // did, fall back to a minimal valid envelope rather than panicking.
            let line = serde_json::to_string(&body)
                .unwrap_or_else(|_| r#"{"error":{"kind":"error"}}"#.to_string());
            eprintln!("{line}");
        }
        OutputMode::Human => eprintln!("hydrate: {}", err.message()),
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
}
