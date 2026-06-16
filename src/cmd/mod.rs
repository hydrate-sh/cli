//! Verb handlers. [`dispatch`] routes a parsed [`Cli`] to exactly one handler.
//!
//! Implemented verbs return `Result<(), CliError>` and render their own success
//! output in the selected mode; [`finish`] prints any error in that same mode
//! and maps it to a stable exit code. The remaining verbs are still stubs that
//! fail loud (they never pretend to succeed) until their phase lands.

use std::process::ExitCode;

use crate::cli::{Cli, Command, EdgeAction, NodeAction};
use crate::error::CliError;
use crate::exit;
use crate::output::{self, OutputMode};

mod branches;
mod commit;
mod context;
mod diff;
mod edge;
mod fork;
mod node;
mod status;

/// Route a parsed command to its handler.
pub fn dispatch(cli: Cli) -> ExitCode {
    let mode = OutputMode::from_flags(cli.json, cli.human);
    match cli.command {
        Command::Fork(args) => finish(fork::run(args, mode), mode),
        Command::Branches => finish(branches::run(mode), mode),
        Command::Node { action } => match action {
            NodeAction::Add(args) => node::add(args),
        },
        Command::Edge { action } => match action {
            EdgeAction::Add(args) => edge::add(args),
        },
        Command::Status => status::run(),
        Command::Diff => diff::run(),
        Command::Commit => commit::run(),
    }
}

/// Render a verb's outcome: success was already printed by the verb, so map it
/// to `0`; an error is printed in `mode` and mapped to its stable exit code.
fn finish(result: Result<(), CliError>, mode: OutputMode) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            output::print_error(&e, mode);
            ExitCode::from(e.exit_code())
        }
    }
}

/// Shared stub: report that a verb is not implemented yet and fail loud.
fn not_implemented(verb: &str) -> ExitCode {
    eprintln!("hydrate: `{verb}` is not implemented yet");
    ExitCode::from(exit::GENERIC)
}
